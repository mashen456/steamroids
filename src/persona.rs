//! Player profile details over a logged-in CM session — no `WebAPI` key needed.
//!
//! Two routes, both ridden on a [`SessionHandle`]:
//!
//! - [`request_player_summary`] — persona name, avatar, online state, and the
//!   game the player is in, via `CMsgClientRequestFriendData` →
//!   `CMsgClientPersonaState` (an unsolicited reply, correlated by `SteamID`).
//! - [`request_profile_info`] — the public profile fields (real name, location,
//!   summary, account age) via the job-correlated `CMsgClientFriendProfileInfo`.
//!
//! Both work for *any* account, not just friends. The avatar arrives as a SHA-1
//! hash; [`avatar_url`] turns it into a CDN URL, and the profile URL is derived
//! from the `SteamID`.
//!
//! ```no_run
//! # async fn demo(session: steamroids::session::SessionHandle) -> steamroids::Result<()> {
//! let summary = steamroids::persona::request_player_summary(&session, 76_561_198_000_000_000).await?;
//! println!("{} — {}", summary.persona_name, summary.profile_url);
//! println!("avatar: {}", summary.avatar_url);
//! # Ok(())
//! # }
//! ```

use std::time::Duration;

use prost::Message;
use tokio::sync::broadcast;
use tokio::time::timeout;

use crate::codec::SteamMessage;
use crate::error::eresult;
use crate::proto::{
    CMsgClientFriendProfileInfo, CMsgClientPersonaState, CMsgClientRequestFriendData,
};
use crate::session::SessionHandle;
use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

// EMsg values, from `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_PERSONA_STATE: u32 = 766;
const EMSG_CLIENT_REQUEST_FRIEND_DATA: u32 = 815;
const EMSG_CLIENT_FRIEND_PROFILE_INFO: u32 = 5535;

/// `EClientPersonaStateFlag` bits we ask for: Status | `PlayerName` | Presence |
/// `LastSeen` | `GameExtraInfo`. `PlayerName` carries the avatar hash too.
const PERSONA_FLAGS: u32 = 1 | 2 | 16 | 64 | 256;

/// The one `status_flags` bit a `CMsgClientPersonaState` must carry before it
/// counts as an answer to our request: `PlayerName`, which backs the persona
/// name and avatar hash. Steam pushes unsolicited partial updates too (a bare
/// Status "went online", say), and those carry no name. Status itself is *not*
/// required: a non-friend, privacy-restricted or offline account is often
/// answered with a name and avatar and no Status bit, which
/// [`PersonaState::Offline`] covers.
const REQUIRED_SUMMARY_FLAGS: u32 = 2;

/// How long to wait for the persona-state reply.
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(10);

/// Steam's avatar CDN host. The full URL is `{HOST}{hash_hex}_full.jpg`.
const AVATAR_HOST: &str = "https://avatars.steamstatic.com/";

/// The well-known hash Steam serves for accounts without a custom avatar.
const DEFAULT_AVATAR_HASH: &str = "fef49e7fa7e1997310d705b2a6158ff8dc1cdfeb";

/// A player's online status (`EPersonaState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PersonaState {
    /// Offline (or invisible to us).
    Offline,
    /// Online.
    Online,
    /// Busy.
    Busy,
    /// Away.
    Away,
    /// Snooze (extended away).
    Snooze,
    /// Looking to trade.
    LookingToTrade,
    /// Looking to play.
    LookingToPlay,
    /// Invisible.
    Invisible,
    /// A value newer than this enum knows about.
    Unknown(u32),
}

impl PersonaState {
    fn from_raw(value: u32) -> Self {
        match value {
            0 => Self::Offline,
            1 => Self::Online,
            2 => Self::Busy,
            3 => Self::Away,
            4 => Self::Snooze,
            5 => Self::LookingToTrade,
            6 => Self::LookingToPlay,
            7 => Self::Invisible,
            other => Self::Unknown(other),
        }
    }
}

/// The game a player is currently in, if any.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CurrentGame {
    /// App id of the game.
    pub app_id: u32,
    /// Full 64-bit `CGameID` (non-zero for non-Steam games / mods).
    pub game_id: u64,
    /// Display name the client reports (`game_extra_info`); may be empty.
    pub name: String,
}

/// A player's persona summary: the at-a-glance profile bits.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlayerSummary {
    /// 64-bit `SteamID`.
    pub steam_id: u64,
    /// Current display (persona) name.
    pub persona_name: String,
    /// Public community profile URL (`/profiles/{id}` — redirects to a vanity
    /// URL if the account has one).
    pub profile_url: String,
    /// Full-size avatar URL.
    pub avatar_url: String,
    /// Avatar SHA-1 as lowercase hex (the default-avatar hash when unset).
    pub avatar_hash: String,
    /// Online status.
    pub state: PersonaState,
    /// The game the player is in, if any.
    pub current_game: Option<CurrentGame>,
}

/// The public profile fields behind a player's community profile.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ProfileInfo {
    /// 64-bit `SteamID`.
    pub steam_id: u64,
    /// Real name, if the player set one.
    pub real_name: Option<String>,
    /// Country name, if shared.
    pub country: Option<String>,
    /// State / region, if shared.
    pub state: Option<String>,
    /// City, if shared.
    pub city: Option<String>,
    /// Profile summary / "about" text.
    pub summary: Option<String>,
    /// Account creation time (unix seconds).
    pub created_at: Option<u32>,
}

/// Build the canonical community profile URL for a `SteamID`.
///
/// This `/profiles/{id}` form **always works**, even when the account has a
/// custom (vanity) URL — Steam redirects it to `/id/{name}`. Getting the pretty
/// vanity form for a `SteamID` isn't available keyless; it needs the `WebAPI`
/// `GetPlayerSummaries`. The reverse — a vanity name back to a `SteamID` — *is*
/// keyless: see [`resolve_vanity_url`].
#[must_use]
pub fn profile_url(steam_id: u64) -> String {
    format!("https://steamcommunity.com/profiles/{steam_id}")
}

/// Resolve a custom (vanity) profile URL name to a `SteamID` — keyless.
///
/// Steam's community site echoes the `SteamID` in the XML view of a vanity
/// profile (`https://steamcommunity.com/id/{name}?xml=1`), so no `WebAPI` key is
/// needed. `name` is the bare vanity (the part after `/id/`), e.g.
/// `"gabelogannewell"`. `Ok(None)` means the site answered 200 with no
/// `steamID64` in the document, i.e. no such vanity exists. The request goes
/// through `proxy` when given.
///
/// # Errors
///
/// [`Error::Network`] if the community site is unreachable or answers with a
/// non-success status (rate limiting and 5xx are common behind rotating
/// proxies), plus client-build errors.
pub async fn resolve_vanity_url(name: &str, proxy: Option<&ProxyConfig>) -> Result<Option<u64>> {
    let url = format!("https://steamcommunity.com/id/{name}?xml=1");
    let response = crate::http::client(proxy)?
        .get(&url)
        .send()
        .await
        .map_err(|e| Error::Network(format!("vanity lookup: {e}")))?;
    if !response.status().is_success() {
        return Err(Error::Network(format!(
            "vanity lookup: HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|e| Error::Network(format!("vanity lookup body: {e}")))?;
    Ok(extract_steam_id64(&body))
}

/// Pull the `<steamID64>…</steamID64>` value out of a community XML document.
fn extract_steam_id64(xml: &str) -> Option<u64> {
    let start = xml.find("<steamID64>")? + "<steamID64>".len();
    let end = xml[start..].find("</steamID64>")? + start;
    xml[start..end].trim().parse().ok()
}

/// Build the full-size avatar URL from a raw avatar hash.
///
/// An empty or all-zero hash maps to Steam's default avatar. Use
/// [`avatar_url_sized`] for other sizes.
#[must_use]
pub fn avatar_url(hash: &[u8]) -> String {
    avatar_url_sized(hash, AvatarSize::Full)
}

/// Avatar image sizes Steam serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarSize {
    /// 32×32 (no suffix).
    Small,
    /// 64×64 (`_medium`).
    Medium,
    /// 184×184 (`_full`).
    Full,
}

/// Build an avatar URL of a given size from a raw avatar hash.
#[must_use]
pub fn avatar_url_sized(hash: &[u8], size: AvatarSize) -> String {
    let hex = hash_to_hex(hash);
    let hex = if hex.is_empty() || hash.iter().all(|&b| b == 0) {
        DEFAULT_AVATAR_HASH.to_string()
    } else {
        hex
    };
    let suffix = match size {
        AvatarSize::Small => "",
        AvatarSize::Medium => "_medium",
        AvatarSize::Full => "_full",
    };
    format!("{AVATAR_HOST}{hex}{suffix}.jpg")
}

/// Download an avatar image by URL, returning the raw JPEG bytes.
///
/// `url` is an avatar URL from [`PlayerSummary::avatar_url`], [`avatar_url`], or
/// [`avatar_url_sized`] — a public Steam CDN link, so no auth is needed. The
/// request goes through `proxy` when given. Write the bytes to a file, or decode
/// them with an image crate.
///
/// # Errors
///
/// [`Error::Network`] if the CDN is unreachable or returns a non-success status.
pub async fn fetch_avatar(url: &str, proxy: Option<&ProxyConfig>) -> Result<Vec<u8>> {
    let response = crate::http::client(proxy)?
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Network(format!("fetch avatar: {e}")))?;
    if !response.status().is_success() {
        return Err(Error::Network(format!(
            "fetch avatar: HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| Error::Network(format!("fetch avatar body: {e}")))?;
    Ok(bytes.to_vec())
}

/// Lowercase-hex encode a byte slice.
fn hash_to_hex(hash: &[u8]) -> String {
    use std::fmt::Write as _;
    hash.iter()
        .fold(String::with_capacity(hash.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Fetch a player's persona summary (name, avatar, status, current game).
///
/// Sends `CMsgClientRequestFriendData` and waits for the `CMsgClientPersonaState`
/// that carries this `steam_id`. Works for any account.
///
/// # Errors
///
/// [`Error::Timeout`] if no persona state arrives, [`Error::WebSocket`] if the
/// session stopped or the write failed, or a transport / decode error.
pub async fn request_player_summary(
    session: &SessionHandle,
    steam_id: u64,
) -> Result<PlayerSummary> {
    // Subscribe before sending so a fast reply can't slip past us.
    let mut events = session.subscribe();

    let request = CMsgClientRequestFriendData {
        persona_state_requested: Some(PERSONA_FLAGS),
        friends: vec![steam_id],
    };
    session
        .notify(EMSG_CLIENT_REQUEST_FRIEND_DATA, &request)
        .await?;

    let wait = async {
        loop {
            match events.recv().await {
                Ok(msg) => {
                    if msg.emsg != EMSG_CLIENT_PERSONA_STATE {
                        continue;
                    }
                    if let Some(summary) = summary_from_persona_state(&msg, steam_id)? {
                        return Ok(summary);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(Error::WebSocket("session stopped".into()))
                }
            }
        }
    };
    timeout(SUMMARY_TIMEOUT, wait)
        .await
        .map_err(|_| Error::Timeout("persona state"))?
}

/// Decode a `CMsgClientPersonaState` and pull out `steam_id`'s entry, if the
/// message both carries [`REQUIRED_SUMMARY_FLAGS`] and mentions `steam_id`.
///
/// `Ok(None)` means "not our answer, keep waiting".
fn summary_from_persona_state(msg: &SteamMessage, steam_id: u64) -> Result<Option<PlayerSummary>> {
    let state = CMsgClientPersonaState::decode(msg.body.as_slice())
        .map_err(|e| Error::Codec(format!("decode persona state: {e}")))?;
    // partial push, not a full summary
    if state.status_flags.unwrap_or(0) & REQUIRED_SUMMARY_FLAGS != REQUIRED_SUMMARY_FLAGS {
        return Ok(None);
    }
    let Some(friend) = state
        .friends
        .into_iter()
        .find(|f| f.friendid == Some(steam_id))
    else {
        return Ok(None);
    };

    let avatar = friend.avatar_hash.unwrap_or_default();
    let game_id = friend.gameid.unwrap_or(0);
    let app_id = friend.game_played_app_id.unwrap_or(0);
    let current_game = if app_id != 0 || game_id != 0 {
        Some(CurrentGame {
            #[allow(clippy::cast_possible_truncation)]
            app_id: if app_id != 0 { app_id } else { game_id as u32 },
            game_id,
            name: friend.game_name.unwrap_or_default(),
        })
    } else {
        None
    };

    Ok(Some(PlayerSummary {
        steam_id,
        persona_name: friend.player_name.unwrap_or_default(),
        profile_url: profile_url(steam_id),
        avatar_url: avatar_url(&avatar),
        avatar_hash: hash_to_hex(&avatar),
        state: PersonaState::from_raw(friend.persona_state.unwrap_or(0)),
        current_game,
    }))
}

/// Fetch a player's public profile fields (real name, location, summary, age).
///
/// Uses the job-correlated `CMsgClientFriendProfileInfo` request.
///
/// # Errors
///
/// [`Error::Remote`] if Steam answered with a non-OK `EResult` (a private or
/// otherwise refused profile), [`Error::WebSocket`] if the session stopped, plus
/// any transport / decode error.
pub async fn request_profile_info(session: &SessionHandle, steam_id: u64) -> Result<ProfileInfo> {
    let request = CMsgClientFriendProfileInfo {
        steamid_friend: Some(steam_id),
    };
    let response: crate::proto::CMsgClientFriendProfileInfoResponse = session
        .request(EMSG_CLIENT_FRIEND_PROFILE_INFO, &request)
        .await?;
    profile_info_from_response(response, steam_id)
}

/// Convert a profile-info response into a [`ProfileInfo`], rejecting a non-OK
/// `EResult` so a refused profile can't pass for an empty one.
fn profile_info_from_response(
    response: crate::proto::CMsgClientFriendProfileInfoResponse,
    steam_id: u64,
) -> Result<ProfileInfo> {
    // proto2 default is Fail
    let code = response.eresult.unwrap_or(eresult::FAIL);
    if code != eresult::OK {
        return Err(Error::Remote(format!("profile info: eresult {code}")));
    }

    // Empty strings mean "not shared"; normalize them to `None`.
    let some_if_set = |s: Option<String>| s.filter(|v| !v.is_empty());
    Ok(ProfileInfo {
        steam_id: response.steamid_friend.unwrap_or(steam_id),
        real_name: some_if_set(response.real_name),
        country: some_if_set(response.country_name),
        state: some_if_set(response.state_name),
        city: some_if_set(response.city_name),
        summary: some_if_set(response.summary),
        created_at: response.time_created.filter(|&t| t != 0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEAM_ID: u64 = 76_561_198_702_770_421;

    #[test]
    fn profile_url_uses_profiles_path() {
        assert_eq!(
            profile_url(STEAM_ID),
            "https://steamcommunity.com/profiles/76561198702770421"
        );
    }

    #[test]
    fn avatar_url_hex_encodes_hash() {
        let hash = [0x01, 0xab, 0xff, 0x00];
        assert_eq!(
            avatar_url(&hash),
            "https://avatars.steamstatic.com/01abff00_full.jpg"
        );
        assert_eq!(
            avatar_url_sized(&hash, AvatarSize::Medium),
            "https://avatars.steamstatic.com/01abff00_medium.jpg"
        );
        assert_eq!(
            avatar_url_sized(&hash, AvatarSize::Small),
            "https://avatars.steamstatic.com/01abff00.jpg"
        );
    }

    #[test]
    fn empty_or_zero_hash_uses_default_avatar() {
        let expected = format!("{AVATAR_HOST}{DEFAULT_AVATAR_HASH}_full.jpg");
        assert_eq!(avatar_url(&[]), expected);
        assert_eq!(avatar_url(&[0, 0, 0, 0, 0]), expected);
    }

    #[test]
    fn persona_state_maps_known_values() {
        assert_eq!(PersonaState::from_raw(0), PersonaState::Offline);
        assert_eq!(PersonaState::from_raw(1), PersonaState::Online);
        assert_eq!(PersonaState::from_raw(7), PersonaState::Invisible);
        assert_eq!(PersonaState::from_raw(99), PersonaState::Unknown(99));
    }

    #[test]
    fn summary_extracts_the_requested_friend() {
        use crate::proto::c_msg_client_persona_state::Friend;
        use crate::proto::{CMsgClientPersonaState, CMsgProtoBufHeader};

        let state = CMsgClientPersonaState {
            status_flags: Some(PERSONA_FLAGS),
            friends: vec![
                Friend {
                    friendid: Some(999),
                    player_name: Some("someone else".into()),
                    ..Default::default()
                },
                Friend {
                    friendid: Some(STEAM_ID),
                    persona_state: Some(1),
                    player_name: Some("roidbot".into()),
                    avatar_hash: Some(vec![0xde, 0xad, 0xbe, 0xef]),
                    game_played_app_id: Some(730),
                    game_name: Some("Counter-Strike 2".into()),
                    ..Default::default()
                },
            ],
        };
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_PERSONA_STATE,
            header: CMsgProtoBufHeader::default(),
            body: state.encode_to_vec(),
        };

        let summary = summary_from_persona_state(&msg, STEAM_ID)
            .unwrap()
            .expect("our friend is present");
        assert_eq!(summary.persona_name, "roidbot");
        assert_eq!(summary.state, PersonaState::Online);
        assert_eq!(summary.avatar_hash, "deadbeef");
        assert!(summary.avatar_url.ends_with("deadbeef_full.jpg"));
        let game = summary.current_game.expect("in a game");
        assert_eq!(game.app_id, 730);
        assert_eq!(game.name, "Counter-Strike 2");
    }

    #[test]
    fn extract_steam_id64_parses_community_xml() {
        let xml = r"<?xml version='1.0'?><profile><steamID64>76561197960287930</steamID64><steamID><![CDATA[Rabscuttle]]></steamID></profile>";
        assert_eq!(extract_steam_id64(xml), Some(76_561_197_960_287_930));
    }

    #[test]
    fn extract_steam_id64_none_when_absent() {
        assert_eq!(
            extract_steam_id64("<response><error>No match</error></response>"),
            None
        );
    }

    #[test]
    fn summary_absent_when_the_player_name_bit_is_missing() {
        use crate::proto::c_msg_client_persona_state::Friend;
        use crate::proto::{CMsgClientPersonaState, CMsgProtoBufHeader};

        // Status only: a "went online" push, no player_name in it.
        let state = CMsgClientPersonaState {
            status_flags: Some(1),
            friends: vec![Friend {
                friendid: Some(STEAM_ID),
                persona_state: Some(1),
                ..Default::default()
            }],
        };
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_PERSONA_STATE,
            header: CMsgProtoBufHeader::default(),
            body: state.encode_to_vec(),
        };
        assert!(summary_from_persona_state(&msg, STEAM_ID)
            .unwrap()
            .is_none());
    }

    #[test]
    fn summary_accepts_a_player_name_only_reply() {
        use crate::proto::c_msg_client_persona_state::Friend;
        use crate::proto::{CMsgClientPersonaState, CMsgProtoBufHeader};

        // PlayerName without Status: what a non-friend / restricted / offline
        // account answers with.
        let state = CMsgClientPersonaState {
            status_flags: Some(2),
            friends: vec![Friend {
                friendid: Some(STEAM_ID),
                player_name: Some("stranger".into()),
                avatar_hash: Some(vec![0xde, 0xad, 0xbe, 0xef]),
                ..Default::default()
            }],
        };
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_PERSONA_STATE,
            header: CMsgProtoBufHeader::default(),
            body: state.encode_to_vec(),
        };

        let summary = summary_from_persona_state(&msg, STEAM_ID)
            .unwrap()
            .expect("a name and avatar is the answer we asked for");
        assert_eq!(summary.persona_name, "stranger");
        assert_eq!(summary.avatar_hash, "deadbeef");
        // no Status bit: absent persona_state reads as Offline
        assert_eq!(summary.state, PersonaState::Offline);
        assert!(summary.current_game.is_none());
    }

    #[test]
    fn profile_info_rejects_non_ok_eresult() {
        use crate::proto::CMsgClientFriendProfileInfoResponse;

        // EResult::Blocked.
        let response = CMsgClientFriendProfileInfoResponse {
            eresult: Some(40),
            ..Default::default()
        };
        let err = profile_info_from_response(response, STEAM_ID).unwrap_err();
        assert!(
            matches!(err, Error::Remote(ref m) if m.contains("eresult 40")),
            "{err}"
        );
    }

    #[test]
    fn profile_info_missing_eresult_is_the_proto2_fail_default() {
        use crate::proto::CMsgClientFriendProfileInfoResponse;

        let response = CMsgClientFriendProfileInfoResponse::default();
        assert_eq!(response.eresult, None);
        let err = profile_info_from_response(response, STEAM_ID).unwrap_err();
        assert!(
            matches!(err, Error::Remote(ref m) if m.contains("eresult 2")),
            "{err}"
        );
    }

    #[test]
    fn profile_info_normalizes_empty_strings() {
        use crate::proto::CMsgClientFriendProfileInfoResponse;

        let response = CMsgClientFriendProfileInfoResponse {
            eresult: Some(eresult::OK),
            real_name: Some("Gabe".into()),
            city_name: Some(String::new()),
            time_created: Some(0),
            ..Default::default()
        };
        let info = profile_info_from_response(response, STEAM_ID).unwrap();
        assert_eq!(info.steam_id, STEAM_ID);
        assert_eq!(info.real_name.as_deref(), Some("Gabe"));
        assert_eq!(info.city, None);
        assert_eq!(info.created_at, None);
    }

    #[test]
    fn summary_absent_when_friend_missing() {
        use crate::proto::{CMsgClientPersonaState, CMsgProtoBufHeader};
        let msg = SteamMessage {
            emsg: EMSG_CLIENT_PERSONA_STATE,
            header: CMsgProtoBufHeader::default(),
            body: CMsgClientPersonaState {
                status_flags: Some(PERSONA_FLAGS),
                friends: vec![],
            }
            .encode_to_vec(),
        };
        assert!(summary_from_persona_state(&msg, STEAM_ID)
            .unwrap()
            .is_none());
    }
}
