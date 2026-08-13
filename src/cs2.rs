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
    CMsgGccStrike15V2ClientRequestPlayersProfile, CMsgGccStrike15V2MatchmakingGc2ClientHello,
    CMsgGccStrike15V2PlayersProfile, CsoEconGameAccountClient,
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
    /// Raw penalty countdown in seconds, from the most recent
    /// penalty-bearing GC push cached this session (a `ClientWelcome`'s
    /// `game_data2`), **never** from this `PlayersProfile` response itself,
    /// which the GC leaves at `0` here even when a real penalty exists. That
    /// cached push is always the logged-in account's own hello, so this
    /// reads `0` whenever [`Self::account_id`] isn't that account too, not
    /// only when the GC has reported no penalty this session. Prefer
    /// [`Self::penalty`] over reading this directly.
    pub penalty_seconds: u32,
    /// Raw penalty reason code paired with [`Self::penalty_seconds`], gated
    /// the same way. `0` if none. Prefer [`Self::penalty`] over reading this
    /// directly.
    pub penalty_reason: u32,
}

impl PlayerProfile {
    /// Interpret [`Self::penalty_seconds`] / [`Self::penalty_reason`] as a
    /// [`Cs2Penalty`], using the current wall-clock time to resolve whether
    /// `penalty_seconds` is a countdown duration or an already-absolute
    /// expiry (see [`Cs2Penalty::from_gc`]).
    #[must_use]
    pub fn penalty(&self) -> Option<Cs2Penalty> {
        Cs2Penalty::from_gc(self.penalty_reason, self.penalty_seconds, now_unix())
    }
}

/// Request one player's public CS2 profile through `gc`.
///
/// `account_id` is the 32-bit account id (the low 32 bits of a `SteamID`); use
/// [`account_id_from_steam_id`] to convert. The coordinator must be attached to
/// [`APP_ID`] and welcomed (see [`GameCoordinator::wait_ready`]).
///
/// The reply is matched on `account_id`, so a `PlayersProfile` pushed for some
/// other player (or for a concurrent request) is never mistaken for this one.
/// [`PlayerProfile::penalty_seconds`] / [`PlayerProfile::penalty_reason`] on
/// the result are a separate matter: they come from a cache keyed to the
/// logged-in account, not from anything tied to `account_id`, so they only
/// read as non-zero when `account_id` happens to be that same account. See
/// their field docs.
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

    // never profile.penalty_seconds / penalty_reason, see cached_penalty doc.
    let (penalty_seconds, penalty_reason) = cached_penalty(gc, account_id).unwrap_or((0, 0));

    Ok(PlayerProfile {
        account_id,
        level: profile.player_level.unwrap_or(0),
        current_xp: profile.player_cur_xp.unwrap_or(0),
        competitive_rank: ranking.as_ref().and_then(|r| r.rank_id),
        competitive_wins: ranking.as_ref().and_then(|r| r.wins),
        medals,
        featured_medal,
        penalty_seconds,
        penalty_reason,
    })
}

/// Decode the GC pump's cached penalty-bearing push (a `ClientWelcome`'s
/// `game_data2`, cached opaquely by [`crate::gc::GameCoordinator`]'s pump)
/// into the raw `(penalty_seconds, penalty_reason)` pair for `account_id`, or
/// `None` if nothing has been cached yet this GC session, the bytes don't
/// decode, or the cached hello belongs to a different account.
///
/// That last case is the common one for [`request_player_profile`]: the
/// cache is always the *logged-in* account's own hello (nothing else pushes
/// `game_data2`), so a request for some other player's `account_id` must not
/// borrow it and silently describe the wrong player's penalty.
fn cached_penalty(gc: &GameCoordinator, account_id: u32) -> Option<(u32, u32)> {
    let bytes = gc.session().cached_gc_penalty(gc.appid())?;
    let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(bytes.as_slice()).ok()?;
    if hello.account_id != Some(account_id) {
        return None;
    }
    Some((
        hello.penalty_seconds.unwrap_or(0),
        hello.penalty_reason.unwrap_or(0),
    ))
}

/// Extract the 32-bit account id from a 64-bit `SteamID` (its low 32 bits).
#[must_use]
pub fn account_id_from_steam_id(steam_id: u64) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    {
        steam_id as u32
    }
}

/// Seconds threshold separating a countdown duration from an already-absolute
/// Unix expiry in a GC `penalty_seconds` value (see [`Cs2Penalty::from_gc`]).
/// Roughly ten years: no real CS2 cooldown or temporary ban runs anywhere
/// close to that, so a value above it can only be an absolute timestamp
/// already.
const DURATION_VS_TIMESTAMP_THRESHOLD_SECS: u32 = 10 * 365 * 24 * 3600;

/// `penalty_seconds` at or above this is the GC's "permanent" sentinel
/// rather than a genuinely huge but finite absolute timestamp. Generous
/// margin below the true `u32::MAX`: a real absolute timestamp (see
/// [`DURATION_VS_TIMESTAMP_THRESHOLD_SECS`]) is nowhere near the year 2106
/// that `u32::MAX` seconds since the epoch would represent.
// 0x7FFF_FFFF (a common "forever" sentinel elsewhere) is below this
// threshold, so it reads as Active expiring in 2038, not Permanent. unknown
// whether the gc ever actually sends that value here. needs a live check.
const NEAR_U32_MAX_PERMANENT_SECS: u32 = u32::MAX - 65_536;

/// Penalty reason codes observed to mean "in effect with no countdown,
/// ever" rather than "a cooldown that ran out": reasons 8 and 14, Valve's
/// own `SFUI_CooldownExplanationReason_OfficialBan` ("This account is
/// permanently Untrusted"), and reason 10, `ConvictedForCheating`
/// ("Convicted by Overwatch - Majorly Disruptive"). This list only governs
/// the `penalty_seconds == 0` branch of [`Cs2Penalty::from_gc`].
///
/// Reasons 22 and 23 (`VacNetCulprit` / `VacNetAffiliate` in Valve's naming)
/// are deliberately **not** in this list. Both are
/// `SFUI_CooldownExplanationReason_*` keys, i.e. cooldowns, and Valve's own
/// `Expired_Cooldown` string ("Subsequent cooldowns may be longer")
/// describes escalation, not permanence. A live capture (2026-08-13, real
/// account) had reason 23 arrive with `penalty_seconds = 386_347` (about 4.5
/// days), independently confirmed by that account's GCPD page, and resolved
/// as [`Cs2Penalty::Active`], never touching this list. With
/// `penalty_seconds == 0` a `VACnet` reason instead falls into
/// [`Cs2Penalty::ExpiredUnacknowledged`], the same honest "cannot tell from
/// the GC alone" bucket as any other uncatalogued reason; GCPD's
/// `Acknowledged` column resolves it from the other side.
///
/// Not exhaustive beyond that: any other reason code seen with
/// `penalty_seconds == 0` is treated as [`Cs2Penalty::ExpiredUnacknowledged`]
/// too, not because it is known to be one, but because the GC alone cannot
/// tell a genuinely permanent, uncatalogued reason apart from an ordinary
/// cooldown stuck awaiting acknowledgement.
///
/// `vac_banned` (field 6 of the same hello) is a separate flag entirely. A
/// `VACnet`-flagged cooldown is not a VAC ban and does not imply one: `VACnet` is
/// Valve's gameplay-flagging system, distinct from a full account VAC ban.
/// The same live capture had `penalty_reason = 23` alongside `vac_banned = 0`
/// on the same account, which is exactly what that distinction predicts. See
/// [`vac_banned`]'s doc.
///
/// The reason keys and their English strings above are Valve's own
/// (`csgo_english.txt`, `SFUI_CooldownExplanationReason_*`), but the numeric
/// codes are not written down in the protos anywhere; they come from a
/// third-party tool and prior operational observation of live accounts, not
/// from Valve. See [`penalty_reason_text`] for the full mapping and its
/// provenance.
const PERMANENT_REASONS: [u32; 3] = [8, 10, 14];

/// A CS2 account penalty, interpreted from the Game Coordinator's raw
/// `penalty_seconds` / `penalty_reason` pair via [`Self::from_gc`].
///
/// [`Self::Permanent`] and [`Self::ExpiredUnacknowledged`] can be
/// indistinguishable on the wire: both show as `penalty_reason` set,
/// `penalty_seconds == 0`. For the reason codes in `PERMANENT_REASONS`
/// that's resolved outright; for anything else, the GC alone cannot tell
/// "flagged forever" from "a cooldown that ran out and the client hasn't
/// acknowledged the expiry yet" (Steam only clears the reason once the
/// client acknowledges it).
///
/// [`crate::gcpd::Cs2Cooldown`] carries the other half of that
/// disambiguation: its `expires_at_unix` is the same countdown GCPD's own
/// page reports, and `acknowledged` says whether the account has cleared
/// it. Pair the two: a GC-reported [`Self::ExpiredUnacknowledged`] alongside
/// a GCPD cooldown whose `acknowledged` is `false` is the same event seen
/// from both sides. If GCPD shows no cooldown table for the account at all,
/// reading the GC-reported penalty as the permanent case instead is a
/// projection this crate assumes, not a verified fact: it depends on GCPD
/// keeping no row for a cooldown that expired but is still awaiting
/// acknowledgement, which has not been confirmed live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cs2Penalty {
    /// In effect indefinitely: either a reason code in `PERMANENT_REASONS`,
    /// or `penalty_seconds` reported near `u32::MAX`, the GC's sentinel for
    /// "forever". No expiry to display.
    Permanent {
        /// Raw GC penalty reason code.
        reason: u32,
    },
    /// Actively counting down: `expires_at_unix` is always still ahead of
    /// the `now_unix` [`Self::from_gc`] was given. An absolute timestamp
    /// that already passed resolves to [`Self::ExpiredUnacknowledged`]
    /// instead, never a past-dated `Active`.
    Active {
        /// Raw GC penalty reason code.
        reason: u32,
        /// Unix timestamp (UTC) the penalty expires.
        expires_at_unix: i64,
    },
    /// The countdown reached zero, but Steam has not cleared the reason code
    /// yet, pending client acknowledgement of the expiry. This is also where
    /// a `VACnet` reason (`VacNetCulprit` / `VacNetAffiliate`, 22/23) lands
    /// once its cooldown has run out: those are ordinary cooldowns, not
    /// permanent convictions, so they resolve here rather than to
    /// [`Self::Permanent`]. **Or**, for a reason code outside
    /// `PERMANENT_REASONS`, this is a permanent conviction this crate
    /// doesn't catalogue. See the type-level docs: the GC alone cannot tell
    /// these apart.
    ExpiredUnacknowledged {
        /// Raw GC penalty reason code.
        reason: u32,
    },
}

impl Cs2Penalty {
    /// Interpret a GC hello's raw `penalty_reason` / `penalty_seconds` pair.
    /// `now_unix` is the current Unix time; it matters only when `seconds`
    /// turns out to be a countdown duration rather than an absolute
    /// timestamp (see below), to compute the resulting expiry.
    ///
    /// None of these rules are derivable from the protos. They come from a
    /// working implementation and prior operational observation of live
    /// accounts:
    ///
    /// - No penalty (`None`) only when **both** fields are `0`. Checking
    ///   `seconds` alone misses a penalty stuck at `0` seconds with a reason
    ///   set, a known bug in other tools.
    /// - `seconds` at or above `NEAR_U32_MAX_PERMANENT_SECS` (near
    ///   `u32::MAX`) is [`Self::Permanent`], regardless of `reason`.
    /// - Otherwise `seconds == 0` with `reason` set is [`Self::Permanent`]
    ///   for a reason in `PERMANENT_REASONS`, or
    ///   [`Self::ExpiredUnacknowledged`] for anything else.
    /// - Otherwise `seconds > 0` is [`Self::Active`]. Above
    ///   `DURATION_VS_TIMESTAMP_THRESHOLD_SECS` (roughly ten years) it's
    ///   already an absolute Unix expiry; at or below it, it's a countdown
    ///   duration added to `now_unix`. If the resulting expiry is already at
    ///   or before `now_unix`, it's [`Self::ExpiredUnacknowledged`] instead:
    ///   an absolute timestamp that already passed is the same
    ///   expired-but-unacknowledged state `seconds == 0` reports, just
    ///   arriving through the timestamp path rather than that one.
    #[must_use]
    pub fn from_gc(reason: u32, seconds: u32, now_unix: i64) -> Option<Self> {
        if reason == 0 && seconds == 0 {
            return None;
        }
        if seconds >= NEAR_U32_MAX_PERMANENT_SECS {
            return Some(Self::Permanent { reason });
        }
        if seconds == 0 {
            return Some(if PERMANENT_REASONS.contains(&reason) {
                Self::Permanent { reason }
            } else {
                Self::ExpiredUnacknowledged { reason }
            });
        }
        let expires_at_unix = if seconds > DURATION_VS_TIMESTAMP_THRESHOLD_SECS {
            i64::from(seconds)
        } else {
            now_unix + i64::from(seconds)
        };
        // an absolute timestamp already in the past is the expired case
        // arriving via this path instead of seconds == 0.
        if expires_at_unix <= now_unix {
            return Some(Self::ExpiredUnacknowledged { reason });
        }
        Some(Self::Active {
            reason,
            expires_at_unix,
        })
    }
}

/// Human-readable text for a raw CS2 penalty reason code, in Valve's own
/// words verbatim.
///
/// The strings are Valve's own localisation, copied without paraphrase from
/// `csgo_english.txt`'s `SFUI_CooldownExplanationReason_*` keys, e.g.
/// `OfficialBan` -> "This account is permanently Untrusted",
/// `VacNetAffiliate` -> "You partied with a player whose gameplay has been
/// flagged by VAC as irregular".
///
/// **Only the strings are Valve-sourced. The reason-code-to-key numbering is
/// not.** There is no numeric enum for `SFUI_CooldownExplanationReason_*` in
/// the vendored protos (checked: `CooldownExplanation`, `PenaltyReason`, and
/// `VacNet` do not appear anywhere under `protos/`), so which integer means
/// `OfficialBan` versus `Kicked` comes from a third-party tool
/// (`csgo-checker`) and prior operational observation of live accounts, not
/// from Valve. Treat the numbering as unverified, with one exception: reason
/// 23 is corroborated by a live capture (2026-08-13, real account, see
/// `LIVE_PENALTY_HELLO_PROBE` in this module's tests) that arrived with a
/// multi-day countdown consistent with `VacNetAffiliate`, a cooldown reason,
/// never resolving as permanent.
///
/// Returns `None` for a reason code not in this mapping, rather than
/// guessing at an unmapped value.
#[must_use]
pub fn penalty_reason_text(reason: u32) -> Option<&'static str> {
    Some(match reason {
        1 => "You were kicked from the last match",
        2 => "You killed too many teammates",
        3 => "You killed a teammate at round start",
        4 => "You failed to reconnect to the last match",
        5 => "You abandoned the last match",
        6 => "You did too much damage to your teammates",
        7 => "You did too much damage to your teammates at round start",
        8 | 14 => "This account is permanently Untrusted",
        9 => "You were kicked from too many recent matches",
        10 => "Convicted by Overwatch - Majorly Disruptive",
        11 => "Convicted by Overwatch - Minorly Disruptive",
        16 => "You failed to connect by match start",
        17 => "You have kicked too many teammates in recent matches",
        18..=20 => {
            "Congratulations on your recent competitive wins... wait for \
            matchmaking servers to calibrate your Skill Group placement"
        }
        21 => "You have received significantly more griefing reports than most players",
        22 => "VAC has flagged your gameplay as irregular",
        23 => "You partied with a player whose gameplay has been flagged by VAC as irregular",
        _ => return None,
    })
}

/// Current Unix time in seconds, or `0` if the clock reads before the epoch
/// or (theoretically) past `i64::MAX` seconds.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
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

/// The logged-in account's current CS2 penalty state, read from the Game
/// Coordinator's welcome cache.
///
/// This is the coherent home for this data: like [`has_prime`] and
/// [`elevated_state`], a `ClientWelcome`'s `game_data2` is inherently about
/// the session's own account, never an arbitrary player, so unlike
/// [`PlayerProfile::penalty`] it needs no `account_id` to get right. Reads
/// the same cached bytes [`SessionHandle::cached_gc_penalty`] keeps; see
/// [`Cs2Penalty::from_gc`] for the interpretation rules.
///
/// Returns `None` when the account has no penalty at all, i.e. both
/// `penalty_reason` and `penalty_seconds` are `0` (see
/// [`Cs2Penalty::from_gc`]), by far the most common `None` case, since most
/// accounts are clean. A caller must not read `None` as "don't know" and
/// retry-loop waiting for it to resolve on a healthy account. `None` also
/// covers the ordinary "no data yet" causes: no welcome carrying
/// `game_data2` has arrived yet this GC session, including if [`attach`] was
/// never called at all, or the cached bytes don't decode. [`vac_banned`]
/// reads the same cached bytes but does *not* fold "clean" into `None`; see
/// its doc. Never blocks and never asks the GC for anything.
#[must_use]
pub fn penalty(session: &SessionHandle) -> Option<Cs2Penalty> {
    let bytes = session.cached_gc_penalty(APP_ID)?;
    let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(bytes.as_slice()).ok()?;
    Cs2Penalty::from_gc(
        hello.penalty_reason.unwrap_or(0),
        hello.penalty_seconds.unwrap_or(0),
        now_unix(),
    )
}

/// Whether the logged-in account is VAC banned, read from the same cached
/// welcome as [`penalty`] (field 6, `vac_banned`, of the same hello).
///
/// This settles the VAC case directly instead of inferring it from
/// [`Cs2Penalty`]'s reason codes 22/23 (`VacNetCulprit` / `VacNetAffiliate`
/// in Valve's naming), which are a different thing entirely: `VACnet` is
/// Valve's gameplay-flagging system and produces a cooldown, not a ban. A
/// VAC ban is a separate, permanent account state tracked by this flag
/// instead, so a `VACnet`-flagged cooldown should not be expected to move
/// `vac_banned` at all.
///
/// A live capture (2026-08-13, real account) confirms exactly that
/// distinction: `penalty_reason = 23` (`VacNetAffiliate`) arrived with an
/// active countdown *and* `vac_banned = 0` in the same hello, [`penalty`]
/// returning `Some(Cs2Penalty::Active { reason: 23, .. })` for that account
/// while this function returned `Some(false)` at the same instant. That is
/// the expected shape given the two are unrelated, not a contradiction: a
/// `VACnet` flag from partying with a flagged player is not, by itself, a VAC
/// ban.
///
/// Reads the same cached welcome [`penalty`] does, but **not** under the same
/// `None` conditions: [`penalty`] folds "the account is clean" into `None`
/// too (see its doc), while this function has a real `bool` for that case and
/// returns `Some(false)`. `None` here means only "no welcome yet, or the
/// cached bytes don't decode".
#[must_use]
pub fn vac_banned(session: &SessionHandle) -> Option<bool> {
    let bytes = session.cached_gc_penalty(APP_ID)?;
    let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(bytes.as_slice()).ok()?;
    Some(hello.vac_banned.unwrap_or(0) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::gc::GcMessage;
    use crate::proto::gc::CMsgProtoBufHeader as GcHeader;
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

    #[tokio::test]
    async fn profile_penalty_comes_from_the_cached_welcome_not_the_profile_response() {
        const WANTED: u32 = 100;
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        // Seed the cache exactly as the GC pump would from a welcome's
        // game_data2 (see gc::coordinator): the cached hello is for the
        // same account being requested, so the gate in cached_penalty lets
        // it through.
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            account_id: Some(WANTED),
            penalty_seconds: Some(10),
            penalty_reason: Some(5),
            ..Default::default()
        };
        session.set_cached_gc_penalty(APP_ID, Some(hello.encode_to_vec()));

        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        // The PlayersProfile response itself carries different penalty
        // fields on its account entry (it reuses the hello's message shape),
        // simulating what the real GC never actually sends there. These
        // must be ignored entirely in favour of the cached welcome.
        let reply = GcMessage {
            appid: APP_ID,
            msgtype: GC_PLAYERS_PROFILE,
            header: GcHeader::default(),
            body: CMsgGccStrike15V2PlayersProfile {
                account_profiles: vec![CMsgGccStrike15V2MatchmakingGc2ClientHello {
                    account_id: Some(WANTED),
                    player_level: Some(1),
                    penalty_seconds: Some(999),
                    penalty_reason: Some(99),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let fake = fake_gc(commands, replies, vec![reply]);

        let profile = request_player_profile(&gc, WANTED).await.expect("profile");

        assert_eq!(profile.penalty_seconds, 10);
        assert_eq!(profile.penalty_reason, 5);
        fake.await.expect("fake GC");
    }

    #[tokio::test]
    async fn profile_penalty_is_not_borrowed_from_another_accounts_cached_welcome() {
        // The cached welcome is always the *logged-in* account's own hello.
        // Requesting some stranger's profile must never describe them using
        // it, even though the profile lookup itself succeeds.
        const LOGGED_IN_ACCOUNT: u32 = 100;
        const OTHER_PLAYER: u32 = 200;
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            account_id: Some(LOGGED_IN_ACCOUNT),
            penalty_seconds: Some(10),
            penalty_reason: Some(5),
            ..Default::default()
        };
        session.set_cached_gc_penalty(APP_ID, Some(hello.encode_to_vec()));

        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        let fake = fake_gc(commands, replies, vec![profiles_reply(&[OTHER_PLAYER])]);

        let profile = request_player_profile(&gc, OTHER_PLAYER)
            .await
            .expect("profile");

        assert_eq!(profile.penalty_seconds, 0);
        assert_eq!(profile.penalty_reason, 0);
        assert!(
            profile.penalty().is_none(),
            "a stranger's profile must not carry the logged-in account's penalty"
        );
        fake.await.expect("fake GC");
    }

    #[tokio::test]
    async fn profile_penalty_is_zero_when_nothing_cached() {
        const WANTED: u32 = 55;
        let (session, commands, _events, _snapshots) = SessionHandle::for_test(7);
        let (gc, replies, _ready) = GameCoordinator::for_test(session, APP_ID);
        // The response's own account entry carries nonzero penalty fields
        // (which the real GC never sends), so a regression that fell back
        // to reading the response when the cache is empty would still be
        // caught here rather than passing by coincidence.
        let reply = GcMessage {
            appid: APP_ID,
            msgtype: GC_PLAYERS_PROFILE,
            header: GcHeader::default(),
            body: CMsgGccStrike15V2PlayersProfile {
                account_profiles: vec![CMsgGccStrike15V2MatchmakingGc2ClientHello {
                    account_id: Some(WANTED),
                    player_level: Some(40),
                    penalty_seconds: Some(777),
                    penalty_reason: Some(88),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let fake = fake_gc(commands, replies, vec![reply]);

        let profile = request_player_profile(&gc, WANTED).await.expect("profile");

        assert_eq!(profile.penalty_seconds, 0);
        assert_eq!(profile.penalty_reason, 0);
        assert!(profile.penalty().is_none());
        fake.await.expect("fake GC");
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

    // --- Cs2Penalty::from_gc: rules 1-5, see the type's doc comment for
    // where they come from. Each test below is named for the rule it pins.

    #[test]
    fn rule1_no_penalty_only_when_both_fields_are_zero() {
        assert_eq!(Cs2Penalty::from_gc(0, 0, 1_000), None);
    }

    #[test]
    fn rule1_reason_alone_is_still_a_penalty() {
        // Checking penalty_seconds alone (a known bug in other tools) would
        // read this as "no penalty". reason 5 is not a known permanent code,
        // so it lands as ExpiredUnacknowledged, not None.
        assert_eq!(
            Cs2Penalty::from_gc(5, 0, 1_000),
            Some(Cs2Penalty::ExpiredUnacknowledged { reason: 5 })
        );
    }

    #[test]
    fn rule1_seconds_alone_without_reason_is_still_active() {
        // Pins the other half of the OR: replacing the guard with
        // `if reason == 0 { return None }` would wrongly read this as "no
        // penalty" even though seconds is set.
        assert_eq!(
            Cs2Penalty::from_gc(0, 3_600, 1_000_000),
            Some(Cs2Penalty::Active {
                reason: 0,
                expires_at_unix: 1_003_600,
            })
        );
    }

    #[test]
    fn rule2_zero_seconds_known_permanent_reason_is_permanent() {
        // A permanent conviction that never had a countdown.
        assert_eq!(
            Cs2Penalty::from_gc(8, 0, 1_000),
            Some(Cs2Penalty::Permanent { reason: 8 })
        );
    }

    #[test]
    fn rule2_zero_seconds_unknown_reason_is_expired_unacknowledged() {
        // A cooldown that ran its course but Steam hasn't cleared the
        // reason pending client acknowledgement.
        assert_eq!(
            Cs2Penalty::from_gc(3, 0, 1_000),
            Some(Cs2Penalty::ExpiredUnacknowledged { reason: 3 })
        );
    }

    #[test]
    fn rule3_small_seconds_is_a_duration_added_to_now() {
        assert_eq!(
            Cs2Penalty::from_gc(1, 3_600, 1_000_000),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 1_003_600,
            })
        );
    }

    #[test]
    fn rule3_large_seconds_is_an_absolute_timestamp_not_added_to_now() {
        // Above the ~10-year threshold: used as-is, ignoring now_unix
        // entirely (a wildly different now_unix must not change the result).
        assert_eq!(
            Cs2Penalty::from_gc(1, 2_000_000_000, 1),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 2_000_000_000,
            })
        );
        assert_eq!(
            Cs2Penalty::from_gc(1, 2_000_000_000, 999_999_999),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 2_000_000_000,
            })
        );
    }

    #[test]
    fn rule3_duration_vs_timestamp_threshold_boundary() {
        // At the threshold: still a countdown duration added to now_unix.
        assert_eq!(
            Cs2Penalty::from_gc(1, DURATION_VS_TIMESTAMP_THRESHOLD_SECS, 1_000),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 1_000 + i64::from(DURATION_VS_TIMESTAMP_THRESHOLD_SECS),
            })
        );
        // One second over: an absolute timestamp instead, ignoring now_unix.
        assert_eq!(
            Cs2Penalty::from_gc(1, DURATION_VS_TIMESTAMP_THRESHOLD_SECS + 1, 1_000),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: i64::from(DURATION_VS_TIMESTAMP_THRESHOLD_SECS + 1),
            })
        );
    }

    #[test]
    fn rule3_absolute_expiry_already_past_is_expired_unacknowledged() {
        // seconds is an absolute timestamp (above the duration threshold)
        // that already lies at or before now_unix. Active's own contract is
        // an expiry still ahead, so this must resolve to
        // ExpiredUnacknowledged instead, the same as the seconds == 0 path.
        assert_eq!(
            Cs2Penalty::from_gc(1, 2_000_000_000, 2_000_000_000),
            Some(Cs2Penalty::ExpiredUnacknowledged { reason: 1 }),
            "exactly at now_unix"
        );
        assert_eq!(
            Cs2Penalty::from_gc(1, 2_000_000_000, 2_000_000_001),
            Some(Cs2Penalty::ExpiredUnacknowledged { reason: 1 }),
            "before now_unix"
        );
    }

    #[test]
    fn rule4_near_u32_max_seconds_is_permanent() {
        assert_eq!(
            Cs2Penalty::from_gc(1, u32::MAX, 1_000),
            Some(Cs2Penalty::Permanent { reason: 1 })
        );
        // "Near", not just the exact sentinel value.
        assert_eq!(
            Cs2Penalty::from_gc(1, u32::MAX - 100, 1_000),
            Some(Cs2Penalty::Permanent { reason: 1 })
        );
    }

    #[test]
    fn rule4_permanent_threshold_does_not_swallow_a_real_absolute_timestamp() {
        // A genuinely huge but finite absolute expiry, far below u32::MAX,
        // must stay Active, not get misclassified as permanent.
        assert_eq!(
            Cs2Penalty::from_gc(1, 2_000_000_000, 1_000),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 2_000_000_000,
            })
        );
    }

    #[test]
    fn rule4_near_u32_max_threshold_boundary() {
        // At the threshold: permanent regardless of reason.
        assert_eq!(
            Cs2Penalty::from_gc(1, NEAR_U32_MAX_PERMANENT_SECS, 1_000),
            Some(Cs2Penalty::Permanent { reason: 1 })
        );
        // One below: falls through to the absolute-timestamp branch
        // instead, since it's still far above DURATION_VS_TIMESTAMP_THRESHOLD_SECS.
        assert_eq!(
            Cs2Penalty::from_gc(1, NEAR_U32_MAX_PERMANENT_SECS - 1, 1_000),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: i64::from(NEAR_U32_MAX_PERMANENT_SECS - 1),
            })
        );
    }

    #[test]
    fn rule5_permanent_reason_codes_are_8_10_and_14() {
        for reason in [8, 10, 14] {
            assert_eq!(
                Cs2Penalty::from_gc(reason, 0, 1_000),
                Some(Cs2Penalty::Permanent { reason }),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn rule5_vacnet_reason_codes_22_and_23_are_not_permanent() {
        // VacNetCulprit (22) and VacNetAffiliate (23) are cooldowns
        // (SFUI_CooldownExplanationReason_*), not permanent convictions:
        // Valve's own Expired_Cooldown wording ("Subsequent cooldowns may be
        // longer") describes escalation, not permanence. With seconds == 0
        // they fall into the same honest "cannot tell from the GC alone"
        // bucket as any other uncatalogued reason.
        for reason in [22, 23] {
            assert_eq!(
                Cs2Penalty::from_gc(reason, 0, 1_000),
                Some(Cs2Penalty::ExpiredUnacknowledged { reason }),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn rule5_vacnet_affiliate_reason_23_with_zero_seconds_is_expired_unacknowledged() {
        // Reason 23 is the one code in this list corroborated by a live
        // capture (see LIVE_PENALTY_HELLO_PROBE below), which arrived with a
        // multi-day countdown, not a permanent state. With seconds == 0 it
        // must resolve the same way an ordinary expired cooldown does.
        assert_eq!(
            Cs2Penalty::from_gc(23, 0, 1_000),
            Some(Cs2Penalty::ExpiredUnacknowledged { reason: 23 })
        );
    }

    #[test]
    fn penalty_reason_text_uses_valves_wording_verbatim() {
        assert_eq!(
            penalty_reason_text(8),
            Some("This account is permanently Untrusted")
        );
        assert_eq!(
            penalty_reason_text(14),
            Some("This account is permanently Untrusted")
        );
        assert_eq!(
            penalty_reason_text(23),
            Some("You partied with a player whose gameplay has been flagged by VAC as irregular")
        );
        assert_eq!(
            penalty_reason_text(22),
            Some("VAC has flagged your gameplay as irregular")
        );
        assert_eq!(
            penalty_reason_text(1),
            Some("You were kicked from the last match")
        );
        assert_eq!(
            penalty_reason_text(10),
            Some("Convicted by Overwatch - Majorly Disruptive")
        );
        // 18-20 (SkillGroupCalibration) all share the same string.
        for reason in [18, 19, 20] {
            assert_eq!(
                penalty_reason_text(reason),
                Some(
                    "Congratulations on your recent competitive wins... wait for \
                    matchmaking servers to calibrate your Skill Group placement"
                ),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn penalty_reason_text_is_none_for_an_unmapped_code() {
        // 0 (no penalty), and gaps in the third-party numbering (12, 13, 15).
        for reason in [0, 12, 13, 15, 999] {
            assert_eq!(penalty_reason_text(reason), None, "reason {reason}");
        }
    }

    #[test]
    fn player_profile_penalty_delegates_to_cs2_penalty_from_gc() {
        // now-independent case (an absolute timestamp) so the assertion
        // doesn't race real wall-clock time.
        let profile = PlayerProfile {
            account_id: 1,
            level: 0,
            current_xp: 0,
            competitive_rank: None,
            competitive_wins: None,
            medals: Vec::new(),
            featured_medal: None,
            penalty_seconds: 2_000_000_000,
            penalty_reason: 1,
        };
        assert_eq!(
            profile.penalty(),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 2_000_000_000,
            })
        );

        let clean = PlayerProfile {
            penalty_seconds: 0,
            penalty_reason: 0,
            ..profile
        };
        assert_eq!(clean.penalty(), None);
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

    #[test]
    fn penalty_is_none_before_any_welcome() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        assert!(penalty(&session).is_none());
    }

    #[test]
    fn penalty_reads_the_cached_welcome_for_the_logged_in_account() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            account_id: Some(7),
            penalty_seconds: Some(2_000_000_000),
            penalty_reason: Some(1),
            ..Default::default()
        };
        session.set_cached_gc_penalty(APP_ID, Some(hello.encode_to_vec()));

        assert_eq!(
            penalty(&session),
            Some(Cs2Penalty::Active {
                reason: 1,
                expires_at_unix: 2_000_000_000,
            })
        );
    }

    #[test]
    fn penalty_is_none_for_a_different_app() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            penalty_seconds: Some(10),
            penalty_reason: Some(5),
            ..Default::default()
        };
        session.set_cached_gc_penalty(570, Some(hello.encode_to_vec()));

        assert!(penalty(&session).is_none());
    }

    #[test]
    fn vac_banned_is_none_before_any_welcome() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        assert!(vac_banned(&session).is_none());
    }

    #[test]
    fn vac_banned_true_when_flag_is_nonzero() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello {
            vac_banned: Some(1),
            ..Default::default()
        };
        session.set_cached_gc_penalty(APP_ID, Some(hello.encode_to_vec()));

        assert_eq!(vac_banned(&session), Some(true));
    }

    #[test]
    fn vac_banned_false_when_flag_is_absent_or_zero() {
        let (session, _commands, _events, _snapshots) = SessionHandle::for_test(7);
        let hello = CMsgGccStrike15V2MatchmakingGc2ClientHello::default();
        session.set_cached_gc_penalty(APP_ID, Some(hello.encode_to_vec()));

        assert_eq!(vac_banned(&session), Some(false));
    }

    /// Reconstruction standing in for the 292-byte `game_data2` blob
    /// (`CMsgGccStrike15V2MatchmakingGc2ClientHello`) captured live on
    /// 2026-08-13 from a real account under an active CS2 competitive
    /// cooldown. The raw capture is not committed here: beyond `account_id`
    /// it carried a long run of medals/rankings submessages tied to a real,
    /// identifiable account, and this repository is public.
    ///
    /// These 14 bytes are hand-encoded from the wire format directly (field
    /// numbers and wire types from `cstrike15_gcmessages.proto`, not by
    /// calling this type's own `encode_to_vec`, so a proto regen that
    /// silently moved `penalty_seconds` off field 4 would still be caught
    /// here) and carry only the four fields the tests below assert on. The
    /// field *values* are live-observed, not invented: `account_id =
    /// 1_205_873_838`, `penalty_seconds = 386_347` (about 4.5 days),
    /// `penalty_reason = 23` (`VacNetAffiliate`), `vac_banned = 0`. The account's own
    /// GCPD cooldown page independently confirmed the expiry this decodes
    /// to (`2026-08-17 23:54:16 GMT`).
    const LIVE_PENALTY_HELLO_PROBE: [u8; 14] = [
        0x08, 0xae, 0xd9, 0x80, 0xbf, 0x04, 0x20, 0xab, 0xca, 0x17, 0x28, 0x17, 0x30, 0x00,
    ];

    #[test]
    fn live_penalty_probe_bytes_decode_to_the_captured_fields() {
        let hello =
            CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(LIVE_PENALTY_HELLO_PROBE.as_slice())
                .expect("live penalty probe bytes decode as the hello type");
        assert_eq!(hello.account_id, Some(1_205_873_838));
        assert_eq!(hello.penalty_seconds, Some(386_347));
        assert_eq!(hello.penalty_reason, Some(23));
        assert_eq!(hello.vac_banned, Some(0));
    }

    #[test]
    fn live_penalty_probe_resolves_to_an_active_countdown() {
        let hello =
            CMsgGccStrike15V2MatchmakingGc2ClientHello::decode(LIVE_PENALTY_HELLO_PROBE.as_slice())
                .expect("live penalty probe bytes decode as the hello type");
        // now_unix from the same live capture.
        let penalty = Cs2Penalty::from_gc(
            hello.penalty_reason.unwrap_or(0),
            hello.penalty_seconds.unwrap_or(0),
            1_786_624_508,
        );
        assert_eq!(
            penalty,
            Some(Cs2Penalty::Active {
                reason: 23,
                expires_at_unix: 1_787_010_855,
            })
        );
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
