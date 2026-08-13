//! CS2 (app 730) Game Coordinator helpers — the first consumer of [`crate::gc`].
//!
//! Everything here is a thin, idiomatic layer over the generic
//! [`GameCoordinator`]: attach to CS2's GC, then request a player's public
//! profile and get back a [`PlayerProfile`] with no protobuf types leaking
//! across the API boundary.
//!
//! ```no_run
//! # async fn demo(session: steamroids::session::SessionHandle) -> steamroids::Result<()> {
//! use std::time::Duration;
//! use steamroids::cs2;
//!
//! let gc = cs2::attach(session).await?;
//! gc.wait_ready(Duration::from_secs(10)).await?;
//!
//! let account_id = cs2::account_id_from_steam_id(76_561_198_000_000_000);
//! let profile = cs2::request_player_profile(&gc, account_id).await?;
//! println!("level {} ({} XP)", profile.level, profile.current_xp);
//! # Ok(())
//! # }
//! ```

use std::time::Duration;

use prost::Message as _;

use crate::gc::GameCoordinator;
use crate::proto::gc::{
    CMsgGccStrike15V2ClientRequestPlayersProfile, CMsgGccStrike15V2PlayersProfile,
    CsoEconGameAccountClient,
};
use crate::session::SessionHandle;
use crate::{Error, Result};

/// CS2's Steam app id.
pub const APP_ID: u32 = 730;

/// CS2 GC protocol version sent in the `ClientHello`. CS2's GC rejects a
/// version-less hello with a fatal logon error, so this must be a current value.
/// Bump it if Steam starts rejecting logons after a CS2 update.
pub const GC_HELLO_VERSION: u32 = 2_000_244;

/// Attach to the CS2 Game Coordinator over `session`.
///
/// Thin wrapper over [`GameCoordinator::attach`] that supplies [`APP_ID`] and
/// [`GC_HELLO_VERSION`]. Follow with [`GameCoordinator::wait_ready`] before
/// requesting profiles.
///
/// # Errors
///
/// Propagates the initial launch send if the session has already stopped.
pub async fn attach(session: SessionHandle) -> Result<GameCoordinator> {
    GameCoordinator::attach(session, APP_ID, GC_HELLO_VERSION).await
}

// CS2 GC message types (ECsgoGCMsg), from `protos/csgo/cstrike15_gcmessages.proto`.
const GC_CLIENT_REQUEST_PLAYERS_PROFILE: u32 = 9127;
const GC_PLAYERS_PROFILE: u32 = 9128;

/// How long to wait for the GC to answer a profile request.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(15);

/// Detail level requested from the GC. `32` mirrors the official client and is
/// enough to populate level, XP, and ranking.
const REQUEST_LEVEL: u32 = 32;

/// A CS2 player's public profile summary, as returned by the Game Coordinator.
///
/// Only the broadly useful fields are surfaced; the raw GC message carries far
/// more. Anything the GC omitted is `0` / `None`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PlayerProfile {
    /// 32-bit account id (the low 32 bits of the player's `SteamID`).
    pub account_id: u32,
    /// CS2 profile level (the in-game "XP level").
    pub level: i32,
    /// Current XP within the level.
    pub current_xp: i32,
    /// Competitive rank id (`MatchMaking` rank), if the player has one.
    pub competitive_rank: Option<u32>,
    /// Competitive wins, if known.
    pub competitive_wins: Option<u32>,
    /// Displayed medals/coins as item definition indexes, in display order.
    /// Resolve each to an icon via the econ items manifest. Empty if none.
    pub medals: Vec<u32>,
    /// The featured (showcased) medal's defindex, if the player set one.
    pub featured_medal: Option<u32>,
}

/// Request one player's public CS2 profile through `gc`.
///
/// `account_id` is the 32-bit account id (the low 32 bits of a `SteamID`); use
/// [`account_id_from_steam_id`] to convert. The coordinator must be attached to
/// [`APP_ID`] and welcomed (see [`GameCoordinator::wait_ready`]).
///
/// The reply is matched on `account_id`, so a `PlayersProfile` pushed for some
/// other player (or for a concurrent request) is never mistaken for this one.
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `gc` isn't a CS2 coordinator, [`Error::Timeout`]
/// if no profile for `account_id` arrives in time (which is also how an unknown
/// account reads, since the GC answers it with an empty, unattributable
/// `PlayersProfile`), [`Error::Network`] if a matched reply somehow carries no
/// such profile, or a transport / decode error.
pub async fn request_player_profile(
    gc: &GameCoordinator,
    account_id: u32,
) -> Result<PlayerProfile> {
    if gc.appid() != APP_ID {
        return Err(Error::InvalidConfig(format!(
            "GameCoordinator is attached to app {}, not CS2 ({APP_ID})",
            gc.appid()
        )));
    }

    let request = CMsgGccStrike15V2ClientRequestPlayersProfile {
        account_id: Some(account_id),
        request_level: Some(REQUEST_LEVEL),
        ..Default::default()
    };

    let response: CMsgGccStrike15V2PlayersProfile = gc
        .request_matching(
            GC_CLIENT_REQUEST_PLAYERS_PROFILE,
            &request,
            GC_PLAYERS_PROFILE,
            PROFILE_TIMEOUT,
            |r: &CMsgGccStrike15V2PlayersProfile| {
                r.account_profiles
                    .iter()
                    .any(|p| p.account_id == Some(account_id))
            },
        )
        .await?;

    let profile = response
        .account_profiles
        .into_iter()
        .find(|p| p.account_id == Some(account_id))
        .ok_or_else(|| {
            Error::Network(format!(
                "GC PlayersProfile carried no profile for account {account_id}"
            ))
        })?;

    let ranking = profile.ranking;
    let (medals, featured_medal) = profile
        .medals
        .map(|m| (m.display_items_defidx, m.featured_display_item_defidx))
        .unwrap_or_default();
    Ok(PlayerProfile {
        account_id,
        level: profile.player_level.unwrap_or(0),
        current_xp: profile.player_cur_xp.unwrap_or(0),
        competitive_rank: ranking.as_ref().and_then(|r| r.rank_id),
        competitive_wins: ranking.as_ref().and_then(|r| r.wins),
        medals,
        featured_medal,
    })
}

/// Extract the 32-bit account id from a 64-bit `SteamID` (its low 32 bits).
#[must_use]
pub fn account_id_from_steam_id(steam_id: u64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        steam_id as u32
    }
}

/// SO `type_id` for `CSOEconGameAccountClient`
/// (`protos/csgo/base_gcmessages.proto:101`), the GC `SharedObject` that
/// carries `elevated_state`, Valve's internal name for CS2 Prime.
///
/// Determined live on 2026-08-13 against a CS2-owning test account: of the 8
/// distinct SO `type_id`s the account's `ClientWelcome` carried, only
/// `type_id` 7 held a single blob whose wire-level field numbers
/// (1, 12, 13, 14, 15) with matching wire types (varint, fixed32, varint,
/// varint, varint) exactly fingerprint this message's six-field layout, and
/// no other `type_id` observed carried fields 14 or 15 at all. Field 12
/// (`bonus_xp_timestamp_refresh`) decoded to a plausible recent Unix
/// timestamp, confirming the byte boundaries; the sibling public flag,
/// `CSOPersonaDataPublic.elevated_state` at `type_id` 2, independently agreed
/// the account was elevated. See the commit that introduced this constant for
/// the raw probe output.
const SO_TYPE_ECON_GAME_ACCOUNT_CLIENT: i32 = 7;

/// Whether the account has CS2 Prime status ("elevated" in Valve's own field
/// naming), read from the CS2 Game Coordinator's `SharedObject` cache.
///
/// Prime is `CSOEconGameAccountClient.elevated_state`
/// (`protos/csgo/base_gcmessages.proto:106`), a per-account `SharedObject`
/// the GC's `ClientWelcome` delivers unprompted once [`attach`] has been welcomed
/// (see [`GameCoordinator::wait_ready`]). This is a method, not a
/// subscription: it reads the cache [`SessionHandle::cached_so_objects`] keeps
/// from that welcome and returns immediately. It never asks the GC for
/// anything and never blocks.
///
/// Returns `None` if no welcome carrying this `SharedObject` has arrived yet
/// this GC session, including if [`attach`] was never called at all.
///
/// `!= 0` is a projection this crate assumes rather than one Valve documents:
/// the only account observed live had `elevated_state == 5`, and
/// `CSOPersonaDataPublic.elevated_state` (a *different*, public SO the client
/// otherwise shows as a plain Prime flag) is declared `bool`, while this
/// field is `uint32`. That mismatch shows Valve keeps a wider private state
/// and collapses it to a boolean for display; this function guesses the same
/// collapse from one data point. See [`elevated_state`] to read the raw code
/// instead, e.g. to tell a lapsed or pending nonzero state apart from active
/// Prime.
pub fn has_prime(session: &SessionHandle) -> Option<bool> {
    Some(elevated_state(session)? != 0)
}

/// The account's raw `CSOEconGameAccountClient.elevated_state` code, without
/// [`has_prime`]'s `!= 0` collapse to a boolean.
///
/// Reads the same cached welcome [`has_prime`] does, under the same `None`
/// conditions. Exists so a caller that needs to resolve the ambiguity in
/// [`has_prime`]'s boolean projection (a lapsed or pending nonzero state, for
/// instance) can inspect the underlying code without forking the crate.
///
/// The sibling field `elevated_timestamp` (field 15) also decoded to `5` in
/// the live probe that established this constant's value, which is not a
/// plausible Unix timestamp; that, and not merely one nonzero sample, is what
/// points to these being small integer codes rather than a flag-plus-time
/// pair.
pub fn elevated_state(session: &SessionHandle) -> Option<u32> {
    let blobs = session.cached_so_objects(APP_ID, SO_TYPE_ECON_GAME_ACCOUNT_CLIENT)?;
    let blob = blobs.first()?;
    let account = CsoEconGameAccountClient::decode(blob.as_slice()).ok()?;
    Some(account.elevated_state.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::gc::GcMessage;
    use crate::proto::gc::{
        CMsgGccStrike15V2MatchmakingGc2ClientHello, CMsgProtoBufHeader as GcHeader,
    };
    use crate::session::driver::Command;

    /// A `PlayersProfile` reply carrying one profile per account id.
    fn profiles_reply(ids: &[u32]) -> GcMessage {
        GcMessage {
            appid: APP_ID,
            msgtype: GC_PLAYERS_PROFILE,
            header: GcHeader::default(),
            body: CMsgGccStrike15V2PlayersProfile {
                account_profiles: ids
                    .iter()
                    .map(|&id| CMsgGccStrike15V2MatchmakingGc2ClientHello {
                        account_id: Some(id),
                        player_level: Some(40),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }
            .encode_to_vec(),
        }
    }

    /// Ack the outbound `ClientToGC`, then push `replies` back at the caller.
    fn fake_gc(
        mut commands: tokio::sync::mpsc::Receiver<Command>,
        replies: tokio::sync::broadcast::Sender<GcMessage>,
        queued: Vec<GcMessage>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            match commands.recv().await.expect("request sent") {
                Command::Notify { ack, .. } => ack.send(Ok(())).expect("ack delivered"),
                _ => panic!("expected a Notify"),
            }
            for msg in queued {
                let _ = replies.send(msg);
            }
        })
    }

    #[tokio::test]
    async fn profile_request_picks_the_requested_account() {
        const WANTED: u32 = 742_504_693;
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        let fake = fake_gc(
            commands,
            replies,
            vec![profiles_reply(&[1]), profiles_reply(&[2, WANTED])],
        );

        let profile = request_player_profile(&gc, WANTED).await.expect("profile");

        assert_eq!(profile.account_id, WANTED);
        assert_eq!(profile.level, 40);
        fake.await.expect("fake GC");
    }

    #[tokio::test(start_paused = true)]
    async fn profile_request_never_returns_another_account_as_ours() {
        const WANTED: u32 = 5;
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        let fake = fake_gc(commands, replies, vec![profiles_reply(&[9])]);

        let err = request_player_profile(&gc, WANTED)
            .await
            .expect_err("no profile for WANTED");

        assert!(matches!(err, Error::Timeout(_)), "{err:?}");
        fake.await.expect("fake GC");
    }

    #[tokio::test(start_paused = true)]
    async fn profile_request_rejects_a_non_cs2_coordinator() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, _ready) = GameCoordinator::for_test(session, 570);

        let err = request_player_profile(&gc, 1).await.expect_err("wrong app");

        assert!(matches!(err, Error::InvalidConfig(_)), "{err:?}");
    }

    #[test]
    fn account_id_takes_low_32_bits() {
        // An individual SteamID is 0x0110_0001_0000_0000 + account_id, so the
        // account id is exactly the low 32 bits.
        const STEAM_ID: u64 = 76_561_198_702_770_421;
        const BASE: u64 = 0x0110_0001_0000_0000;
        assert_eq!(
            account_id_from_steam_id(STEAM_ID),
            u32::try_from(STEAM_ID - BASE).unwrap()
        );
        assert_eq!(account_id_from_steam_id(STEAM_ID), 742_504_693);
    }

    #[test]
    fn message_types_match_the_proto() {
        // Guards against an accidental edit drifting from cstrike15_gcmessages.proto.
        assert_eq!(GC_CLIENT_REQUEST_PLAYERS_PROFILE, 9127);
        assert_eq!(GC_PLAYERS_PROFILE, 9128);
    }

    fn seed_econ_game_account(session: &SessionHandle, account: CsoEconGameAccountClient) {
        let mut objects = HashMap::new();
        objects.insert(
            SO_TYPE_ECON_GAME_ACCOUNT_CLIENT,
            vec![account.encode_to_vec()],
        );
        session.replace_so_cache(APP_ID, objects);
    }

    #[test]
    fn has_prime_is_none_before_any_welcome() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        assert!(has_prime(&session).is_none());
    }

    #[test]
    fn has_prime_true_when_elevated_state_is_nonzero() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        seed_econ_game_account(
            &session,
            CsoEconGameAccountClient {
                elevated_state: Some(5),
                ..Default::default()
            },
        );
        assert_eq!(has_prime(&session), Some(true));
    }

    #[test]
    fn has_prime_false_when_elevated_state_is_absent_or_zero() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        seed_econ_game_account(&session, CsoEconGameAccountClient::default());
        assert_eq!(has_prime(&session), Some(false));
    }

    #[test]
    fn has_prime_is_none_for_a_different_app() {
        // A welcome for some other app's GC must not answer CS2's Prime check.
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let mut objects = HashMap::new();
        objects.insert(
            SO_TYPE_ECON_GAME_ACCOUNT_CLIENT,
            vec![CsoEconGameAccountClient {
                elevated_state: Some(1),
                ..Default::default()
            }
            .encode_to_vec()],
        );
        session.replace_so_cache(570, objects);

        assert!(has_prime(&session).is_none());
    }

    #[test]
    fn elevated_state_is_none_before_any_welcome() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        assert!(elevated_state(&session).is_none());
    }

    #[test]
    fn elevated_state_reads_the_raw_code_not_a_bool() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        seed_econ_game_account(
            &session,
            CsoEconGameAccountClient {
                elevated_state: Some(5),
                ..Default::default()
            },
        );
        assert_eq!(elevated_state(&session), Some(5));
    }

    #[test]
    fn elevated_state_defaults_absent_to_zero() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        seed_econ_game_account(&session, CsoEconGameAccountClient::default());
        assert_eq!(elevated_state(&session), Some(0));
    }

    /// Exact wire bytes of the live `CSOEconGameAccountClient` blob captured
    /// during the 2026-08-13 probe that determined
    /// [`SO_TYPE_ECON_GAME_ACCOUNT_CLIENT`]. Decodes to
    /// `additional_backpack_slots = 0`, `bonus_xp_timestamp_refresh =
    /// 1_783_472_400` (a plausible mid-2026 timestamp, which is what
    /// confirmed byte alignment during the probe), `bonus_xp_usedflags =
    /// 16`, `elevated_state = 5`, `elevated_timestamp = 5`.
    ///
    /// Every other test here round-trips through the current prost type, so
    /// a proto regen that silently moved `elevated_state` off field 14 would
    /// leave them all green. Pinning the raw bytes catches that.
    const LIVE_ECON_GAME_ACCOUNT_CLIENT_PROBE: [u8; 13] = [
        0x08, 0x00, 0x65, 0x10, 0xa1, 0x4d, 0x6a, 0x68, 0x10, 0x70, 0x05, 0x78, 0x05,
    ];

    #[test]
    fn live_probe_bytes_decode_to_elevated_state_five() {
        let account =
            CsoEconGameAccountClient::decode(LIVE_ECON_GAME_ACCOUNT_CLIENT_PROBE.as_slice())
                .expect("live probe bytes decode as CsoEconGameAccountClient");
        assert_eq!(account.elevated_state, Some(5));
    }

    #[test]
    fn live_probe_bytes_read_as_has_prime_true() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let mut objects = HashMap::new();
        objects.insert(
            SO_TYPE_ECON_GAME_ACCOUNT_CLIENT,
            vec![LIVE_ECON_GAME_ACCOUNT_CLIENT_PROBE.to_vec()],
        );
        session.replace_so_cache(APP_ID, objects);

        assert_eq!(has_prime(&session), Some(true));
    }

    #[test]
    fn so_type_econ_game_account_client_matches_the_live_probe() {
        // Guards against an accidental edit drifting from the live-determined
        // value (see the constant's doc comment for how it was established).
        assert_eq!(SO_TYPE_ECON_GAME_ACCOUNT_CLIENT, 7);
    }
}
