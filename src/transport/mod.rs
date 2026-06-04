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

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::TlsConnector;

/// Marker trait for "anything we can build a WebSocket on top of."
///
/// `Box<dyn AsyncStream>` is the type-erased stream we pass around so that the
/// proxy and direct paths can share a single signature.
pub trait AsyncStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + ?Sized> AsyncStream for T {}

/// Build a rustls [`TlsConnector`] trusting the webpki root set.
///
/// Shared by the WSS transport ([`websocket`]) and the TLS-to-proxy
/// (`https://`) leg in [`proxy`], so both speak TLS the same way.
pub(crate) fn tls_connector() -> TlsConnector {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}
