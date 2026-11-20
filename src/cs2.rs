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

use crate::gc::GameCoordinator;
use prost::Message as _;

use crate::proto::gc::{
    CMsgGccStrike15V2ClientRequestPlayersProfile, CMsgGccStrike15V2MatchmakingClient2GcHello,
    CMsgGccStrike15V2MatchmakingGc2ClientHello, CMsgGccStrike15V2PlayersProfile,
    CsoEconGameAccountClient, CsoPersonaDataPublic,
};
use crate::session::SessionHandle;
use crate::{Error, Result};

/// CS2's Steam app id.
pub const APP_ID: u32 = 730;

/// CS2 GC protocol version sent in the `ClientHello`: the game's
/// `ClientVersion` from `game/csgo/steam.inf` (1.42.0.0 = 2000948). CS2's GC
/// rejects a version-less hello with a fatal logon error, and an outdated one
/// is still welcomed but gets no matchmaking session: no
/// `MatchmakingGC2ClientHello`, so [`request_account_status`] times out. Bump it
/// after a CS2 update, or pass the current build to [`attach_with_version`].
pub const GC_HELLO_VERSION: u32 = 2_000_948;

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
    attach_with_version(session, GC_HELLO_VERSION).await
}

/// [`attach`] with an explicit `ClientHello` version — the current CS2
/// `ClientVersion` (from `steam.inf`) when the caller tracks game updates
/// itself, so a CS2 patch doesn't need a library release.
///
/// # Errors
///
/// As [`attach`].
pub async fn attach_with_version(
    session: SessionHandle,
    client_version: u32,
) -> Result<GameCoordinator> {
    GameCoordinator::attach(session, APP_ID, client_version).await
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

// Own-account status: `MatchmakingClient2GCHello` → `MatchmakingGC2ClientHello`.
const GC_MATCHMAKING_CLIENT_HELLO: u32 = 9109;
const GC_MATCHMAKING_HELLO: u32 = 9110;

/// SO cache type ids (`CSOEconGameAccountClient`, `CSOPersonaDataPublic`).
const SO_TYPE_GAME_ACCOUNT_CLIENT: i32 = 7;
const SO_TYPE_PERSONA_DATA_PUBLIC: i32 = 2002;

/// `CSOEconGameAccountClient.elevated_state` of a Prime account.
const ELEVATED_STATE_PRIME: u32 = 5;

/// How long [`request_account_status`] waits for a `MatchmakingGC2ClientHello`
/// it asked for before reporting the hello's fields as unknown.
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// The logged-on account's own CS2 status: Prime, matchmaking cooldown, VAC,
/// level and ranking. Built by [`request_account_status`].
///
/// Two sources, either of which may be missing:
/// - **Prime** and the trade ban come from the GC's shared-object cache sent
///   with the welcome (`None` if it carried neither SO type).
/// - **Cooldown, VAC, level, XP, rank, commendations** come from the
///   `MatchmakingGC2ClientHello`. CS2's GC stopped sending it (unsolicited or on
///   request) with an update in October 2026 — observed live — so these are
///   `None` (unknown, *not* "no cooldown") whenever it doesn't arrive.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct AccountStatus {
    /// 32-bit account id of the logged-on account.
    pub account_id: u32,
    /// Prime status from the GC's shared-object cache; `None` if the GC sent
    /// neither `CSOPersonaDataPublic` nor `CSOEconGameAccountClient`.
    pub prime: Option<bool>,
    /// Remaining matchmaking cooldown in seconds (`Some(0)` = none).
    pub penalty_seconds: Option<u32>,
    /// Reason code of the cooldown (CS2's `penalty_reason`, `Some(0)` = none).
    pub penalty_reason: Option<u32>,
    /// The GC reports a VAC ban for CS2.
    pub vac_banned: Option<bool>,
    /// CS2 profile level.
    pub level: Option<i32>,
    /// XP within the level.
    pub current_xp: Option<i32>,
    /// Competitive rank id, if any.
    pub competitive_rank: Option<u32>,
    /// Competitive wins, if known.
    pub competitive_wins: Option<u32>,
    /// Commendations: friendly, teaching, leader.
    pub commendations: Option<(u32, u32, u32)>,
    /// End of a trade ban (unix seconds), from `CSOEconGameAccountClient`.
    pub trade_ban_expiration: Option<u32>,
}

impl AccountStatus {
    /// Whether the `MatchmakingGC2ClientHello` arrived, i.e. cooldown, VAC and
    /// level are known.
    #[must_use]
    pub fn has_matchmaking_hello(&self) -> bool {
        self.penalty_seconds.is_some()
    }
}

/// Request the logged-on account's own status through `gc` (CS2). Waits for the
/// welcome, then combines the shared-object cache (Prime, trade ban; see
/// [`GameCoordinator::so_cache_objects`]) with the `MatchmakingGC2ClientHello`
/// if there is one: the copy the GC pushed (see
/// [`GameCoordinator::last_message`]), else a `MatchmakingClient2GCHello`
/// request with a short wait. A hello that never comes is not an error — its
/// fields stay `None` (see [`AccountStatus`]).
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `gc` isn't a CS2 coordinator, [`Error::Timeout`]
/// if the GC never welcomes us, or a transport / decode error.
pub async fn request_account_status(gc: &GameCoordinator) -> Result<AccountStatus> {
    if gc.appid() != APP_ID {
        return Err(Error::InvalidConfig(format!(
            "GameCoordinator is attached to app {}, not CS2 ({APP_ID})",
            gc.appid()
        )));
    }
    gc.wait_ready(PROFILE_TIMEOUT).await?;
    let pushed = || {
        gc.last_message(GC_MATCHMAKING_HELLO).map(|b| {
            CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(b.as_slice())
                .map_err(|e| Error::Codec(format!("decode MatchmakingGC2ClientHello: {e}")))
        })
    };
    let hello = match pushed() {
        Some(h) => Some(h?),
        // Not pushed: ask, and accept a push too if it lands meanwhile
        // (`request` subscribes before it sends).
        None => match gc
            .request(
                GC_MATCHMAKING_CLIENT_HELLO,
                &CMsgGccStrike15V2MatchmakingClient2GcHello::default(),
                GC_MATCHMAKING_HELLO,
                HELLO_WAIT,
            )
            .await
        {
            Ok(h) => Some(h),
            // Pushed between our cache check and the request's subscribe, or not at all.
            Err(Error::Timeout(_)) => pushed().transpose()?,
            Err(e) => return Err(e),
        },
    };

    let game_account = gc
        .so_cache_objects(SO_TYPE_GAME_ACCOUNT_CLIENT)
        .last()
        .and_then(|b| CsoEconGameAccountClient::decode(b.as_slice()).ok());
    let persona = gc
        .so_cache_objects(SO_TYPE_PERSONA_DATA_PUBLIC)
        .last()
        .and_then(|b| CsoPersonaDataPublic::decode(b.as_slice()).ok());
    Ok(account_status(
        account_id_from_steam_id(gc.session().steam_id()),
        hello.as_ref(),
        game_account.as_ref(),
        persona.as_ref(),
    ))
}

/// Assemble an [`AccountStatus`] from the GC's hello (if any) and the two SO
/// objects. Prime prefers `CSOPersonaDataPublic.elevated_state`, then
/// `CSOEconGameAccountClient.elevated_state == 5`.
fn account_status(
    account_id: u32,
    hello: Option<&CMsgGccStrike15V2MatchmakingGc2ClientHello>,
    game_account: Option<&CsoEconGameAccountClient>,
    persona: Option<&CsoPersonaDataPublic>,
) -> AccountStatus {
    let prime = persona.and_then(|p| p.elevated_state).or_else(|| {
        game_account
            .and_then(|g| g.elevated_state)
            .map(|s| s == ELEVATED_STATE_PRIME)
    });
    let ranking = hello.and_then(|h| h.ranking.as_ref());
    AccountStatus {
        account_id,
        prime,
        penalty_seconds: hello.map(|h| h.penalty_seconds.unwrap_or(0)),
        penalty_reason: hello.map(|h| h.penalty_reason.unwrap_or(0)),
        vac_banned: hello.map(|h| h.vac_banned.unwrap_or(0) != 0),
        level: hello.map(|h| h.player_level.unwrap_or(0)),
        current_xp: hello.map(|h| h.player_cur_xp.unwrap_or(0)),
        competitive_rank: ranking.and_then(|r| r.rank_id),
        competitive_wins: ranking.and_then(|r| r.wins),
        commendations: hello.map(|h| {
            let c = h.commendation.as_ref();
            (
                c.and_then(|c| c.cmd_friendly).unwrap_or(0),
                c.and_then(|c| c.cmd_teaching).unwrap_or(0),
                c.and_then(|c| c.cmd_leader).unwrap_or(0),
            )
        }),
        trade_ban_expiration: game_account
            .and_then(|g| g.trade_ban_expiration)
            .filter(|&t| t != 0),
    }
}

/// Extract the 32-bit account id from a 64-bit `SteamID` (its low 32 bits).
#[must_use]
pub fn account_id_from_steam_id(steam_id: u64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        steam_id as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(GC_MATCHMAKING_CLIENT_HELLO, 9109);
        assert_eq!(GC_MATCHMAKING_HELLO, 9110);
    }

    #[tokio::test]
    async fn account_status_reads_hello_and_so_cache() {
        use crate::proto::gc::{PlayerCommendationInfo, PlayerRankingInfo};

        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        gc.set_so_cache_for_test(vec![
            (
                SO_TYPE_GAME_ACCOUNT_CLIENT,
                CsoEconGameAccountClient {
                    elevated_state: Some(0),
                    trade_ban_expiration: Some(1_800_000_000),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
            (
                SO_TYPE_PERSONA_DATA_PUBLIC,
                CsoPersonaDataPublic {
                    elevated_state: Some(true),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
        ]);
        let hello = GcMessage {
            appid: APP_ID,
            msgtype: GC_MATCHMAKING_HELLO,
            header: GcHeader::default(),
            body: CMsgGccStrike15V2MatchmakingGc2ClientHello {
                account_id: Some(42),
                penalty_seconds: Some(1_800),
                penalty_reason: Some(4),
                vac_banned: Some(0),
                player_level: Some(12),
                ranking: Some(PlayerRankingInfo {
                    rank_id: Some(9),
                    wins: Some(31),
                    ..Default::default()
                }),
                commendation: Some(PlayerCommendationInfo {
                    cmd_friendly: Some(3),
                    cmd_teaching: Some(2),
                    cmd_leader: Some(1),
                }),
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let fake = fake_gc(commands, replies, vec![hello]);

        let s = request_account_status(&gc).await.expect("status");

        // Persona says Prime; it wins over the game account's elevated_state 0.
        assert_eq!(s.prime, Some(true));
        assert_eq!(
            (
                s.account_id,
                s.penalty_seconds,
                s.penalty_reason,
                s.vac_banned
            ),
            // account id from the session (SteamID 7), not from the hello
            (7, Some(1_800), Some(4), Some(false))
        );
        assert_eq!(
            (s.level, s.competitive_rank, s.competitive_wins),
            (Some(12), Some(9), Some(31))
        );
        assert_eq!(s.commendations, Some((3, 2, 1)));
        assert!(s.has_matchmaking_hello());
        assert_eq!(s.trade_ban_expiration, Some(1_800_000_000));
        fake.await.expect("fake GC");
    }

    #[tokio::test]
    async fn account_status_uses_the_hello_the_gc_pushed_after_welcome() {
        let (session, mut commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, _replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        gc.set_last_message_for_test(
            GC_MATCHMAKING_HELLO,
            CMsgGccStrike15V2MatchmakingGc2ClientHello {
                account_id: Some(42),
                penalty_seconds: Some(60),
                ..Default::default()
            }
            .encode_to_vec(),
        );

        let s = request_account_status(&gc).await.expect("status");

        assert_eq!((s.account_id, s.penalty_seconds), (7, Some(60)));
        assert!(
            commands.try_recv().is_err(),
            "no MatchmakingClient2GCHello sent when the GC already pushed the hello"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn account_status_without_hello_keeps_prime_and_leaves_the_rest_unknown() {
        // CS2's GC no longer sends MatchmakingGC2ClientHello (seen live, Oct 2026):
        // the request goes out, nothing comes back, Prime still comes from the SO cache.
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        gc.set_so_cache_for_test(vec![(
            SO_TYPE_GAME_ACCOUNT_CLIENT,
            CsoEconGameAccountClient {
                elevated_state: Some(ELEVATED_STATE_PRIME),
                ..Default::default()
            }
            .encode_to_vec(),
        )]);
        let fake = fake_gc(commands, replies, vec![]);

        let s = request_account_status(&gc)
            .await
            .expect("status without hello");

        assert_eq!((s.account_id, s.prime), (7, Some(true)));
        assert!(!s.has_matchmaking_hello());
        assert_eq!(
            (s.penalty_seconds, s.vac_banned, s.level),
            (None, None, None)
        );
        fake.await.expect("fake GC");
    }

    #[test]
    fn prime_falls_back_to_game_account_and_stays_unknown_without_cache() {
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            vac_banned: Some(1),
            ..Default::default()
        };
        let prime = |state| CsoEconGameAccountClient {
            elevated_state: Some(state),
            ..Default::default()
        };

        assert_eq!(
            account_status(1, Some(&hello), Some(&prime(5)), None).prime,
            Some(true)
        );
        assert_eq!(
            account_status(1, Some(&hello), Some(&prime(0)), None).prime,
            Some(false)
        );
        let none = account_status(1, Some(&hello), None, None);
        assert_eq!(
            (none.prime, none.vac_banned, none.trade_ban_expiration),
            (None, Some(true), None)
        );
    }
}
