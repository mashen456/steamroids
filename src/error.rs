//! Crate-wide error type.

use thiserror::Error;

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

    /// Steam reported a transient auth failure (rate-limited, retry later).
    #[error("auth rate-limited: {0}")]
    AuthRateLimited(String),

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
    /// Remove the variant once 0.1.x is feature-complete.
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
