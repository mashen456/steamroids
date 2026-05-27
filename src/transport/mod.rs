//! Transport layer — TCP, TLS, WebSocket, and proxies.
//!
//! Everything here is Steam-protocol-agnostic. The Steam framing on top is in
//! `codec` (added in `0.1.x`).
//!
//! Public surface:
//! - [`ProxyConfig`] — describe a SOCKS5 or HTTP-CONNECT proxy
//! - [`connect_ws`] — open a WSS connection, optionally tunneled through a proxy

pub mod proxy;
pub mod websocket;

pub use proxy::{ProxyConfig, ProxyCredentials, ProxyKind};
pub use websocket::connect_ws;

use tokio::io::{AsyncRead, AsyncWrite};

/// Marker trait for "anything we can build a WebSocket on top of."
///
/// `Box<dyn AsyncStream>` is the type-erased stream we pass around so that the
/// proxy and direct paths can share a single signature.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> AsyncStream for T {}
