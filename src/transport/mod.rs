//! Transport layer — TCP, TLS, WebSocket, and proxies.
//!
//! Everything here is Steam-protocol-agnostic. The Steam framing on top is in
//! [`crate::codec`].
//!
//! Public surface:
//! - [`ProxyConfig`]: describe a SOCKS5, HTTP-CONNECT, or `https://`
//!   (TLS-to-proxy) proxy
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

/// Strip the brackets off an IPv6 URL host (`[::1]` becomes `::1`).
///
/// `url::Url::host_str` keeps the brackets, but SNI and `ToSocketAddrs` both
/// want the bare address. Non-IPv6 hosts pass through untouched.
pub(crate) fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// Format a `host:port` authority, re-bracketing a bare IPv6 literal.
///
/// The inverse of [`unbracket`], for the places that need a wire- or
/// URL-shaped `host:port` again.
pub(crate) fn authority(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

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

#[cfg(test)]
mod tests {
    use super::{authority, unbracket};

    #[test]
    fn unbracket_strips_only_ipv6_brackets() {
        assert_eq!(unbracket("[::1]"), "::1");
        assert_eq!(unbracket("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(unbracket("host"), "host");
        assert_eq!(unbracket("127.0.0.1"), "127.0.0.1");
        // not a bracketed pair
        assert_eq!(unbracket("[oops"), "[oops");
        assert_eq!(unbracket("oops]"), "oops]");
    }

    #[test]
    fn authority_rebrackets_ipv6_only() {
        assert_eq!(authority("::1", 1080), "[::1]:1080");
        assert_eq!(authority("2001:db8::1", 443), "[2001:db8::1]:443");
        assert_eq!(authority("host", 8080), "host:8080");
        assert_eq!(authority("127.0.0.1", 80), "127.0.0.1:80");
    }

    #[test]
    fn unbracket_then_authority_round_trips() {
        for host in ["[::1]", "host", "127.0.0.1"] {
            let bare = unbracket(host);
            assert_eq!(authority(bare, 1080), format!("{host}:1080"));
        }
    }
}
