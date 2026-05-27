//! Session state observation type.

use serde::{Deserialize, Serialize};

/// Where a Steam session is in its lifecycle, from an external observer's
/// point of view.
///
/// This enum is for **observability** — UIs, status reports, structured logs.
/// The internal session machine in `0.1.x` is enforced via typestate
/// (`Session<Connecting>` cannot call `request_profile`), so this enum is
/// the projection of that state for code outside the typestate world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    /// No connection attempted, or fully closed.
    Disconnected,
    /// TCP/TLS/WS handshake in progress.
    Connecting,
    /// Steam auth flow in progress (`BeginAuthSession` → `PollAuthSession`).
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
    pub fn label(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Authenticating => "authenticating",
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
        assert!(!SessionState::Disconnected.is_ready());
        assert!(!SessionState::Connecting.is_ready());
        assert!(!SessionState::Authenticating.is_ready());
        assert!(!SessionState::LoggedOff { reason: "x".into() }.is_ready());
        assert!(!SessionState::Failed { error: "x".into() }.is_ready());
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
