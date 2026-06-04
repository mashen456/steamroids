//! Proxy configuration and connection helpers.
//!
//! Supports SOCKS5 and HTTP-CONNECT. Both auth and unauth flavours.
//!
//! ## URL forms accepted
//!
//! ```text
//! socks5://host:1080
//! socks5://user:pass@host:1080
//! http://user:pass@host:8080      // HTTP CONNECT proxy
//! https://user:pass@host:8443     // HTTPS frontend to a CONNECT proxy
//! ```

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_socks::tcp::Socks5Stream;
use tracing::trace;

use crate::error::Error;
use crate::transport::AsyncStream;

const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Which proxy protocol to speak to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyKind {
    /// SOCKS5, RFC 1928 / RFC 1929 (username/password auth).
    Socks5,
    /// HTTP `CONNECT` method, RFC 7231 §4.3.6. Treated as plaintext —
    /// transport between client and proxy is unencrypted unless the URL
    /// scheme was `https` (see [`ProxyConfig::tls_to_proxy`]).
    HttpConnect,
}

/// Username/password pair for proxy auth.
///
/// `Debug` is implemented by hand so the password stays out of logs and traces
/// (this type is embedded in [`ProxyConfig`], which several public types print
/// in their own `Debug` output).
#[derive(Clone, Serialize, Deserialize)]
pub struct ProxyCredentials {
    /// Proxy username.
    pub username: String,
    /// Proxy password.
    pub password: String,
}

impl std::fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyCredentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Address and credentials for an upstream proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Which proxy protocol to speak.
    pub kind: ProxyKind,
    /// Proxy host (not the upstream target — that's passed at connect time).
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Optional auth.
    pub credentials: Option<ProxyCredentials>,
    /// `true` if the URL scheme was `https://` — the TCP leg to the proxy
    /// itself should be TLS-wrapped. We don't act on this yet (would require
    /// TLS-to-proxy-then-TLS-to-upstream); reserved for a later release.
    pub tls_to_proxy: bool,
}

impl ProxyConfig {
    /// Parse a proxy URL.
    pub fn parse(url_str: &str) -> Result<Self, Error> {
        let url = url::Url::parse(url_str)?;
        let kind = match url.scheme() {
            "socks5" | "socks5h" => ProxyKind::Socks5,
            "http" | "https" => ProxyKind::HttpConnect,
            other => {
                return Err(Error::InvalidConfig(format!(
                    "unsupported proxy scheme: {other}"
                )))
            }
        };
        let host = url
            .host_str()
            .ok_or_else(|| Error::InvalidUrl("proxy URL missing host".into()))?
            .to_string();
        let port = url
            .port()
            .or_else(|| match url.scheme() {
                "socks5" | "socks5h" => Some(1080),
                "http" => Some(8080),
                "https" => Some(443),
                _ => None,
            })
            .ok_or_else(|| Error::InvalidUrl("proxy URL missing port".into()))?;
        let credentials = if url.username().is_empty() {
            None
        } else {
            Some(ProxyCredentials {
                username: percent_decode(url.username()),
                password: percent_decode(url.password().unwrap_or("")),
            })
        };
        Ok(Self {
            kind,
            host,
            port,
            credentials,
            tls_to_proxy: url.scheme() == "https",
        })
    }

    /// Display-friendly identifier — host:port without credentials.
    pub fn display(&self) -> String {
        format!("{}://{}:{}", self.scheme_label(), self.host, self.port)
    }

    fn scheme_label(&self) -> &'static str {
        match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::HttpConnect => {
                if self.tls_to_proxy {
                    "https"
                } else {
                    "http"
                }
            }
        }
    }
}

/// Establish a TCP connection to `(target_host, target_port)` through the
/// supplied proxy. Returns a stream that behaves like a direct TCP connection.
pub async fn connect_via_proxy(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<Box<dyn AsyncStream>, Error> {
    trace!(
        proxy = %proxy.display(),
        target = %format!("{target_host}:{target_port}"),
        "starting proxy connect"
    );

    if proxy.tls_to_proxy {
        // Would need to TLS-wrap the TCP leg to the proxy itself before doing
        // the proxy handshake. Out of scope for 0.0.x — most providers expose
        // a plain TCP/SOCKS5 endpoint anyway.
        return Err(Error::InvalidConfig(
            "https:// proxies (TLS-to-proxy) not yet supported; use http:// or socks5://".into(),
        ));
    }

    match proxy.kind {
        ProxyKind::Socks5 => connect_socks5(proxy, target_host, target_port).await,
        ProxyKind::HttpConnect => connect_http(proxy, target_host, target_port).await,
    }
}

async fn connect_socks5(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<Box<dyn AsyncStream>, Error> {
    let proxy_addr = (proxy.host.as_str(), proxy.port);
    let target_addr = format!("{target_host}:{target_port}");

    let stream = timeout(PROXY_HANDSHAKE_TIMEOUT, async {
        match &proxy.credentials {
            Some(creds) => {
                Socks5Stream::connect_with_password(
                    proxy_addr,
                    target_addr.as_str(),
                    &creds.username,
                    &creds.password,
                )
                .await
            }
            None => Socks5Stream::connect(proxy_addr, target_addr.as_str()).await,
        }
    })
    .await
    .map_err(|_| Error::Timeout("socks5 handshake"))??;

    Ok(Box::new(stream))
}

async fn connect_http(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<Box<dyn AsyncStream>, Error> {
    let proxy_addr = format!("{}:{}", proxy.host, proxy.port);
    let mut stream = timeout(PROXY_HANDSHAKE_TIMEOUT, TcpStream::connect(&proxy_addr))
        .await
        .map_err(|_| Error::Timeout("http connect tcp"))??;

    let auth_header = proxy.credentials.as_ref().map(|c| {
        let blob = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", c.username, c.password));
        format!("Proxy-Authorization: Basic {blob}\r\n")
    });

    let request = format!(
        "CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n{auth}\r\n",
        host = target_host,
        port = target_port,
        auth = auth_header.as_deref().unwrap_or(""),
    );

    timeout(PROXY_HANDSHAKE_TIMEOUT, async {
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|_| Error::Timeout("http connect write"))??;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    timeout(PROXY_HANDSHAKE_TIMEOUT, reader.read_line(&mut status_line))
        .await
        .map_err(|_| Error::Timeout("http connect read status"))??;

    let status_line = status_line.trim_end();
    if !is_ok_status(status_line) {
        return Err(Error::HttpConnect(format!("proxy refused: {status_line}")));
    }

    // Drain headers until the empty line that ends them.
    loop {
        let mut line = String::new();
        let bytes = timeout(PROXY_HANDSHAKE_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| Error::Timeout("http connect read headers"))??;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    Ok(Box::new(reader.into_inner()))
}

fn is_ok_status(status_line: &str) -> bool {
    status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code == "200")
}

fn percent_decode(input: &str) -> String {
    // Best-effort: if the URL crate already decoded, we get the literal back.
    // We don't carry the `percent-encoding` crate for this small need.
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push(char::from((h << 4) | l));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_socks5_with_auth() {
        let cfg = ProxyConfig::parse("socks5://user:pass@host:1080").unwrap();
        assert_eq!(cfg.kind, ProxyKind::Socks5);
        assert_eq!(cfg.host, "host");
        assert_eq!(cfg.port, 1080);
        let creds = cfg.credentials.unwrap();
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password, "pass");
    }

    #[test]
    fn parses_socks5_without_auth_defaults_port() {
        let cfg = ProxyConfig::parse("socks5://host").unwrap();
        assert_eq!(cfg.port, 1080);
        assert!(cfg.credentials.is_none());
    }

    #[test]
    fn parses_http_connect() {
        let cfg = ProxyConfig::parse("http://u:p@host:8080").unwrap();
        assert_eq!(cfg.kind, ProxyKind::HttpConnect);
        assert!(!cfg.tls_to_proxy);
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = ProxyConfig::parse("ftp://host:21").unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn debug_redacts_proxy_password() {
        // The config's Debug delegates into ProxyCredentials' Debug, so the
        // password must not appear anywhere in the printed config.
        let cfg = ProxyConfig::parse("socks5://user:supersecretpw@host:1080").unwrap();
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("supersecretpw"),
            "proxy password leaked: {dbg}"
        );
        // Host and username are fine to show.
        assert!(dbg.contains("host"));
        assert!(dbg.contains("user"));
    }
}
