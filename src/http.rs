//! Shared reqwest client construction.
//!
//! Both the `WebAPI` auth flow ([`crate::auth`]) and CM server discovery
//! ([`crate::session::discover_cm_servers`]) talk HTTPS to Steam, optionally
//! through a proxy. This module centralizes the client + proxy setup so they
//! stay consistent (timeouts, user agent, `https://`-proxy handling).

use std::time::Duration;

use reqwest::{Client, Proxy};

use crate::transport::proxy::{ProxyConfig, ProxyCredentials, ProxyKind};
use crate::{Error, Result};

/// Network operation budget (connect + transfer) for any one HTTPS call.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP-level connect budget. Steam's edge is usually <1s — keep it tight so a
/// dead proxy fails fast instead of stalling the caller.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a reqwest [`Client`] with our standard timeouts and user agent,
/// optionally routed through `proxy`.
pub(crate) fn client(proxy: Option<&ProxyConfig>) -> Result<Client> {
    let mut builder = Client::builder()
        .timeout(TOTAL_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("steamroids/", env!("CARGO_PKG_VERSION")));

    if let Some(p) = proxy {
        builder = builder.proxy(build_proxy(p)?);
    }

    builder
        .build()
        .map_err(|e| Error::InvalidConfig(format!("reqwest client: {e}")))
}

/// Translate a [`ProxyConfig`] into a reqwest [`Proxy`]. `https://` proxies are
/// supported — reqwest TLS-connects to the proxy itself via our rustls backend.
pub(crate) fn build_proxy(cfg: &ProxyConfig) -> Result<Proxy> {
    let scheme = match cfg.kind {
        ProxyKind::Socks5 => "socks5h",
        ProxyKind::HttpConnect if cfg.tls_to_proxy => "https",
        ProxyKind::HttpConnect => "http",
    };
    let url = format!("{scheme}://{}:{}", cfg.host, cfg.port);

    let mut proxy = Proxy::all(&url).map_err(|e| Error::InvalidConfig(format!("proxy: {e}")))?;
    if let Some(ProxyCredentials { username, password }) = cfg.credentials.as_ref() {
        proxy = proxy.basic_auth(username, password);
    }
    Ok(proxy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_proxy_handles_socks5() {
        let cfg = ProxyConfig::parse("socks5://u:p@host:1080").unwrap();
        build_proxy(&cfg).unwrap();
    }

    #[test]
    fn build_proxy_handles_http() {
        let cfg = ProxyConfig::parse("http://u:p@host:8080").unwrap();
        build_proxy(&cfg).unwrap();
    }

    #[test]
    fn build_proxy_accepts_https_to_proxy() {
        // reqwest handles TLS-to-proxy natively; this must build.
        let cfg = ProxyConfig::parse("https://u:p@host:8443").unwrap();
        build_proxy(&cfg).unwrap();
    }

    #[test]
    fn client_builds_without_proxy() {
        client(None).unwrap();
    }
}
