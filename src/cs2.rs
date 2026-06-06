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
//! use steamroids::{cs2, gc::GameCoordinator};
//!
//! let gc = GameCoordinator::attach(session, cs2::APP_ID).await?;
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
use crate::proto::gc::{
    CMsgGccStrike15V2ClientRequestPlayersProfile, CMsgGccStrike15V2PlayersProfile,
};
use crate::{Error, Result};

/// CS2's Steam app id.
pub const APP_ID: u32 = 730;

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
}

/// Request one player's public CS2 profile through `gc`.
///
/// `account_id` is the 32-bit account id (the low 32 bits of a `SteamID`); use
/// [`account_id_from_steam_id`] to convert. The coordinator must be attached to
/// [`APP_ID`] and welcomed (see [`GameCoordinator::wait_ready`]).
///
/// # Errors
///
/// [`Error::InvalidConfig`] if `gc` isn't a CS2 coordinator, [`Error::Timeout`]
/// if the GC doesn't answer, [`Error::Network`] if it answers with no profile,
/// or a transport / decode error.
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
        .request(
            GC_CLIENT_REQUEST_PLAYERS_PROFILE,
            &request,
            GC_PLAYERS_PROFILE,
            PROFILE_TIMEOUT,
        )
        .await?;

    let profile = response
        .account_profiles
        .into_iter()
        .next()
        .ok_or_else(|| Error::Network("GC returned an empty PlayersProfile".into()))?;

    let ranking = profile.ranking;
    Ok(PlayerProfile {
        account_id: profile.account_id.unwrap_or(account_id),
        level: profile.player_level.unwrap_or(0),
        current_xp: profile.player_cur_xp.unwrap_or(0),
        competitive_rank: ranking.as_ref().and_then(|r| r.rank_id),
        competitive_wins: ranking.as_ref().and_then(|r| r.wins),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
