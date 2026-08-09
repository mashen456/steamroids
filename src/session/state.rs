//! Session state observation type.

use serde::{Deserialize, Serialize};

/// Where a Steam session is in its lifecycle, from an external observer's
/// point of view.
///
/// This enum is for **observability**: UIs, status reports, structured logs.
/// Published by [`SessionHandle::state`](crate::session::SessionHandle::state)
/// and [`watch_state`](crate::session::SessionHandle::watch_state). It is a
/// report, not a guard: nothing in the type system stops a caller issuing a
/// request while the session is [`Self::Connecting`], so an operation on a
/// session that is not ready fails at runtime instead of failing to compile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[allow(deprecated)]
pub enum SessionState {
    /// No connection attempted, or fully closed.
    #[deprecated(
        since = "0.4.0",
        note = "never emitted; a session that is not connected reports Connecting while it \
                re-establishes, or LoggedOff / Failed once it stops. Match on those instead."
    )]
    Disconnected,
    /// Establishing (or re-establishing) the CM connection: discovery, the
    /// TCP/TLS/WS handshake and the logon round-trip.
    Connecting,
    /// Steam auth flow in progress.
    #[deprecated(
        since = "0.4.0",
        note = "never emitted; the WebAPI auth flow completes before a session exists, so the \
                session reports Connecting for the whole establish path. Match on that instead."
    )]
    Authenticating,
    /// `loggedOn` event received, session is ready to do work.
    LoggedOn {
        /// 64-bit Steam ID of the logged-in account.
        steam_id: u64,
    },
    /// Logged off cleanly or due to a server-side eviction.
    LoggedOff {
        /// Steam's reason field. Useful for matching `LoggedInElsewhere`,
        /// `RateLimitExceeded`, etc.
        reason: String,
    },
    /// Login attempt(s) exhausted without success. Terminal until the caller
    /// resets credentials or backoff.
    Failed {
        /// Human-readable last error.
        error: String,
    },
}

impl SessionState {
    /// `true` while the session can perform application-level operations.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::LoggedOn { .. })
    }

    /// Short tag for log lines and metrics labels.
    #[allow(deprecated)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Authenticating => "authenticating",
            Self::Connecting => "connecting",
            Self::LoggedOn { .. } => "logged_on",
            Self::LoggedOff { .. } => "logged_off",
            Self::Failed { .. } => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_ready_only_when_logged_on() {
        assert!(SessionState::LoggedOn { steam_id: 1 }.is_ready());
        assert!(!SessionState::Connecting.is_ready());
        assert!(!SessionState::LoggedOff { reason: "x".into() }.is_ready());
        assert!(!SessionState::Failed { error: "x".into() }.is_ready());
    }

    #[test]
    #[allow(deprecated)]
    fn labels_cover_every_state() {
        assert_eq!(SessionState::Disconnected.label(), "disconnected");
        assert_eq!(SessionState::Authenticating.label(), "authenticating");
        assert_eq!(SessionState::Connecting.label(), "connecting");
        assert_eq!(SessionState::LoggedOn { steam_id: 1 }.label(), "logged_on");
        assert_eq!(
            SessionState::LoggedOff { reason: "x".into() }.label(),
            "logged_off"
        );
        assert_eq!(SessionState::Failed { error: "x".into() }.label(), "failed");
    }

    #[test]
    fn round_trips_via_serde() {
        let state = SessionState::LoggedOn {
            steam_id: 76_561_198_000_000_000,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: SessionState = serde_json::from_str(&json).unwrap();
        assert_eq!(state, back);
    }
}
