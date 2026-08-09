//! Crate-wide error type and Steam's `EResult` codes.

use thiserror::Error;

/// Steam's `EResult` codes: the crate's single source of truth.
///
/// The vendored protos carry no `EResult` enum, so the values are transcribed
/// from `SteamKit`'s `Resources/SteamLanguage/eresult.steamd`
/// (<https://github.com/SteamRE/SteamKit>), which is the canonical listing to
/// re-verify against. Only the codes the crate actually branches on are named.
///
/// Steam sends them as protobuf `int32` fields (message bodies and the routing
/// header) and as the `x-eresult` header on `WebAPI` responses.
pub(crate) mod eresult {
    /// `EResult::OK` (1).
    pub(crate) const OK: i32 = 1;
    /// `EResult::Fail` (2): the proto2 default for every `eresult` field.
    pub(crate) const FAIL: i32 = 2;
    /// `EResult::NoConnection` (3).
    pub(crate) const NO_CONNECTION: i32 = 3;
    /// `EResult::InvalidPassword` (5).
    pub(crate) const INVALID_PASSWORD: i32 = 5;
    /// `EResult::Busy` (10).
    pub(crate) const BUSY: i32 = 10;
    /// `EResult::Timeout` (16).
    pub(crate) const TIMEOUT: i32 = 16;
    /// `EResult::ServiceUnavailable` (20).
    pub(crate) const SERVICE_UNAVAILABLE: i32 = 20;
    /// `EResult::DuplicateRequest` (29): the friendship, or our pending request
    /// for it, already exists.
    pub(crate) const DUPLICATE_REQUEST: i32 = 29;
    /// `EResult::TryAnotherCM` (48). **Not 42**, which is `NoMatch`. This is the
    /// code Valve sends most often when load-balancing a session off a CM.
    pub(crate) const TRY_ANOTHER_CM: i32 = 48;
    /// `EResult::AccountLogonDenied` (63): Steam Guard email code required.
    pub(crate) const ACCOUNT_LOGON_DENIED: i32 = 63;
    /// `EResult::RateLimitExceeded` (84).
    pub(crate) const RATE_LIMIT_EXCEEDED: i32 = 84;
    /// `EResult::AccountLoginDeniedNeedTwoFactor` (85).
    pub(crate) const ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR: i32 = 85;
    /// `EResult::AccountLoginDeniedThrottle` (87).
    pub(crate) const ACCOUNT_LOGIN_DENIED_THROTTLE: i32 = 87;
    /// `EResult::TwoFactorCodeMismatch` (88).
    pub(crate) const TWO_FACTOR_CODE_MISMATCH: i32 = 88;
}

/// Every fallible operation in `steamroids` produces one of these.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// I/O failure on a socket or stream.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The supplied URL was malformed or missing a required field.
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    /// URL parse failed.
    #[error("url parse: {0}")]
    UrlParse(#[from] url::ParseError),

    /// A configuration value was missing or malformed.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// SOCKS5 handshake or relay failure.
    #[error("socks5: {0}")]
    Socks5(String),

    /// HTTP-CONNECT proxy handshake failure (status line, headers, etc.).
    #[error("http connect: {0}")]
    HttpConnect(String),

    /// TLS-layer failure (certificate, handshake, protocol).
    #[error("tls: {0}")]
    Tls(String),

    /// WebSocket handshake or framing failure.
    #[error("websocket: {0}")]
    WebSocket(String),

    /// A Steam message frame was malformed (bad length prefix, non-protobuf
    /// payload, or an undecodable header). See [`crate::codec`].
    #[error("codec: {0}")]
    Codec(String),

    /// Base64 decoding failed for a secret or token.
    #[error("base64: {0}")]
    Base64(#[from] base64::DecodeError),

    /// HMAC key derivation failed (typically wrong key length).
    #[error("hmac: invalid key length")]
    HmacKey,

    /// Network-layer failure talking to Steam's `WebAPI` (DNS, TCP,
    /// TLS handshake, proxy CONNECT, timeout). Distinct from
    /// [`Error::AuthRejected`] so callers can tell "Steam said no" from
    /// "we never reached Steam".
    #[error("network: {0}")]
    Network(String),

    /// Steam reported a permanent auth failure (bad password, banned, etc.).
    #[error("auth rejected: {0}")]
    AuthRejected(String),

    /// Steam refused a CM logon for a *transient* reason (`TryAnotherCM`,
    /// `ServiceUnavailable`, `NoConnection`, rate limiting, …). The credentials
    /// are fine: a fresh connection, through a fresh proxy exit, is expected to
    /// succeed. Distinct from [`Error::AuthRejected`] so the session driver
    /// keeps reconnecting instead of killing the session for good.
    #[error("cm logon retryable: {0}")]
    LogonRetryable(String),

    /// Steam processed a request but returned a non-OK `EResult` (e.g. an
    /// `AddFriend` that was rejected, a GC request that failed). The string
    /// carries the operation and result for diagnostics.
    #[error("steam request failed: {0}")]
    Remote(String),

    /// Steam reported a transient auth failure (rate-limited, retry later).
    #[error("auth rate-limited: {0}")]
    AuthRateLimited(String),

    /// A caller-supplied [`TokenStore`](crate::auth::TokenStore) returned an
    /// error while loading or saving a refresh token.
    #[error("token store: {0}")]
    TokenStore(String),

    /// An operation was attempted in the wrong session state.
    #[error("invalid session state: expected {expected}, was {actual}")]
    InvalidState {
        /// The state the operation requires.
        expected: &'static str,
        /// The state the session is currently in.
        actual: &'static str,
    },

    /// An operation timed out before producing a result.
    #[error("timeout: {0}")]
    Timeout(&'static str),

    /// A code path the public API exposes but the underlying implementation
    /// is not landed yet. The string names the missing piece.
    ///
    /// This is a deliberate sentinel — examples and integration code can
    /// surface a clear "wired but not finished" signal instead of panicking.
    /// Nothing in the crate returns it today.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

impl From<tokio_socks::Error> for Error {
    fn from(value: tokio_socks::Error) -> Self {
        Self::Socks5(value.to_string())
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(value: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(value.to_string())
    }
}
