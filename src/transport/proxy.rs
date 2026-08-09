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
//! socks5://[2001:db8::1]:1080     // IPv6 literal
//! ```

use std::time::Duration;

use base64::Engine;
use rustls_pki_types::ServerName;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, timeout_at, Instant};
use tokio_socks::tcp::Socks5Stream;
use tracing::trace;

use crate::error::Error;
use crate::transport::{authority, tls_connector, unbracket, AsyncStream};

const PROXY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
// cap connect reply headers
const MAX_CONNECT_HEADERS: usize = 64;

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
    /// IPv6 literals are stored bare, without the URL's square brackets.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Optional auth.
    pub credentials: Option<ProxyCredentials>,
    /// `true` if the URL scheme was `https://` — the TCP leg to the proxy
    /// itself is TLS-wrapped before the HTTP `CONNECT` handshake. The upstream's
    /// own TLS (e.g. `wss`) then layers on top. Only meaningful for
    /// [`ProxyKind::HttpConnect`]; SOCKS5 ignores it.
    pub tls_to_proxy: bool,
}

impl ProxyConfig {
    /// Parse a proxy URL.
    ///
    /// A port written in the URL is always kept as written. When the URL omits
    /// one, the default is 1080 for `socks5`/`socks5h`, 8080 for `http` and 443
    /// for `https`. Userinfo is percent-decoded as UTF-8, and either half may be
    /// empty (`socks5://:pass@host` still authenticates).
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
        let host = unbracket(
            url.host_str()
                .ok_or_else(|| Error::InvalidUrl("proxy URL missing host".into()))?,
        )
        .to_string();
        let port = resolve_port(&url, url_str)?;
        let username = percent_decode(url.username());
        let password = percent_decode(url.password().unwrap_or(""));
        let credentials = if username.is_empty() && password.is_empty() {
            None
        } else {
            Some(ProxyCredentials { username, password })
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
        format!(
            "{}://{}",
            self.scheme_label(),
            authority(&self.host, self.port)
        )
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

/// Resolve the proxy port, preferring whatever the URL actually wrote.
///
/// `url::Url` drops a written port that matches its scheme default (`:80` on
/// `http`), so `Url::port()` alone cannot tell that from an omitted port. `raw`
/// is re-scanned to separate the two; the defaults apply only when it really
/// was omitted.
fn resolve_port(url: &url::Url, raw: &str) -> Result<u16, Error> {
    if let Some(port) = url.port() {
        return Ok(port);
    }
    let port = if has_explicit_port(raw) {
        url.port_or_known_default()
    } else {
        match url.scheme() {
            "socks5" | "socks5h" => Some(1080),
            "http" => Some(8080),
            "https" => Some(443),
            _ => None,
        }
    };
    port.ok_or_else(|| Error::InvalidUrl("proxy URL missing port".into()))
}

// port written in raw authority
fn has_explicit_port(raw: &str) -> bool {
    let Some((_, rest)) = raw.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // skip an ipv6 literal so its inner colons don't read as a port
    let tail = host_port.rsplit_once(']').map_or(host_port, |(_, t)| t);
    tail.split_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
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
        target = %authority(target_host, target_port),
        "starting proxy connect"
    );

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
    let target_addr = authority(target_host, target_port);

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
    let proxy_addr = authority(&proxy.host, proxy.port);
    let tcp = timeout(PROXY_HANDSHAKE_TIMEOUT, TcpStream::connect(&proxy_addr))
        .await
        .map_err(|_| Error::Timeout("http connect tcp"))??;

    // For an https:// proxy, TLS-wrap the leg to the proxy itself before the
    // CONNECT handshake. The upstream's own TLS (e.g. wss) layers on top later.
    let mut stream: Box<dyn AsyncStream> = if proxy.tls_to_proxy {
        let connector = tls_connector();
        let sni = ServerName::try_from(proxy.host.clone())
            .map_err(|e| Error::Tls(format!("invalid proxy sni: {e}")))?;
        let tls = timeout(PROXY_HANDSHAKE_TIMEOUT, connector.connect(sni, tcp))
            .await
            .map_err(|_| Error::Timeout("proxy tls handshake"))?
            .map_err(|e| Error::Tls(e.to_string()))?;
        Box::new(tls)
    } else {
        Box::new(tcp)
    };

    let auth_header = proxy.credentials.as_ref().map(|c| {
        let blob = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", c.username, c.password));
        format!("Proxy-Authorization: Basic {blob}\r\n")
    });

    let target = authority(target_host, target_port);
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{auth}\r\n",
        auth = auth_header.as_deref().unwrap_or(""),
    );

    timeout(PROXY_HANDSHAKE_TIMEOUT, async {
        stream.write_all(request.as_bytes()).await?;
        stream.flush().await?;
        Ok::<_, std::io::Error>(())
    })
    .await
    .map_err(|_| Error::Timeout("http connect write"))??;

    // one deadline for status + headers, not per line
    let deadline = Instant::now() + PROXY_HANDSHAKE_TIMEOUT;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    timeout_at(deadline, reader.read_line(&mut status_line))
        .await
        .map_err(|_| Error::Timeout("http connect read status"))??;

    let status_line = status_line.trim_end();
    if !is_ok_status(status_line) {
        return Err(Error::HttpConnect(format!("proxy refused: {status_line}")));
    }

    // Drain headers until the empty line that ends them.
    for _ in 0..MAX_CONNECT_HEADERS {
        let mut line = String::new();
        let bytes = timeout_at(deadline, reader.read_line(&mut line))
            .await
            .map_err(|_| Error::Timeout("http connect read headers"))??;
        if bytes == 0 || line == "\r\n" || line == "\n" {
            // keep BufReader, not into_inner: may hold bytes past blank line
            return Ok(Box::new(reader));
        }
    }

    Err(Error::HttpConnect(
        "proxy sent too many CONNECT response headers".into(),
    ))
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
    // decode to bytes, not chars: the escapes spell out UTF-8, and char::from
    // would read each one as latin-1
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    fn parses_https_connect_sets_tls_to_proxy() {
        let cfg = ProxyConfig::parse("https://u:p@host:8443").unwrap();
        assert_eq!(cfg.kind, ProxyKind::HttpConnect);
        assert!(cfg.tls_to_proxy, "https:// scheme must flag TLS-to-proxy");
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = ProxyConfig::parse("ftp://host:21").unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn keeps_explicitly_written_ports() {
        // url::Url drops a port equal to its scheme default, so :80 on http
        // used to fall through to the 8080 default. Every one of these is
        // written in the URL and must survive verbatim.
        for (url, expected) in [
            ("socks5://host:1080", 1080u16),
            ("socks5h://host:1080", 1080),
            ("socks5://host:9050", 9050),
            ("http://host:8080", 8080),
            ("http://host:80", 80),
            ("https://host:443", 443),
            ("https://host:8443", 8443),
            ("http://user:pass@host:80", 80),
        ] {
            let cfg = ProxyConfig::parse(url).unwrap();
            assert_eq!(cfg.port, expected, "{url} lost its explicit port");
        }
    }

    #[test]
    fn applies_scheme_defaults_only_when_port_omitted() {
        for (url, expected) in [
            ("socks5://host", 1080u16),
            ("socks5h://host", 1080),
            ("http://host", 8080),
            ("https://host", 443),
            ("http://user:pass@host", 8080),
            ("http://host/some/path", 8080),
        ] {
            let cfg = ProxyConfig::parse(url).unwrap();
            assert_eq!(cfg.port, expected, "{url} got the wrong default port");
        }
    }

    #[test]
    fn has_explicit_port_reads_the_raw_authority() {
        assert!(has_explicit_port("http://host:80"));
        assert!(has_explicit_port("http://user:pass@host:80"));
        assert!(has_explicit_port("socks5://[::1]:1080"));
        assert!(has_explicit_port("http://host:8080/path"));
        assert!(!has_explicit_port("http://host"));
        assert!(!has_explicit_port("http://user:pass@host"));
        // an ipv6 literal's own colons are not a port
        assert!(!has_explicit_port("socks5://[::1]"));
        assert!(!has_explicit_port("http://host/a:1"));
        assert!(!has_explicit_port("http://host?q=a:1"));
    }

    #[test]
    fn strips_brackets_from_ipv6_host() {
        let cfg = ProxyConfig::parse("socks5://[2001:db8::1]:1080").unwrap();
        assert_eq!(cfg.host, "2001:db8::1");
        assert_eq!(cfg.port, 1080);
        // re-bracketed when it goes back into a host:port string
        assert_eq!(cfg.display(), "socks5://[2001:db8::1]:1080");
    }

    #[test]
    fn socks5_proxy_addr_resolves_for_ipv6() {
        use std::net::{SocketAddr, ToSocketAddrs};

        // connect_socks5 hands (host, port) straight to the resolver, which
        // parses a bare literal but DNS-looks-up a bracketed one.
        let cfg = ProxyConfig::parse("socks5://[::1]:1080").unwrap();
        let addrs: Vec<_> = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .expect("bracketed host would not resolve")
            .collect();
        assert_eq!(
            addrs,
            vec![SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 1080))]
        );
    }

    #[test]
    fn ipv4_and_dns_hosts_are_not_bracketed() {
        assert_eq!(
            ProxyConfig::parse("http://127.0.0.1:8080")
                .unwrap()
                .display(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            ProxyConfig::parse("socks5://host:1080").unwrap().display(),
            "socks5://host:1080"
        );
    }

    #[test]
    fn decodes_percent_escapes_as_utf8() {
        // url percent-encodes userinfo as UTF-8; decoding byte-per-char would
        // mangle anything outside ASCII into latin-1.
        let cfg = ProxyConfig::parse("socks5://us%C3%A9r:p%C3%A4ssw%C3%B6rd@host:1080").unwrap();
        let creds = cfg.credentials.unwrap();
        assert_eq!(creds.username, "usér");
        assert_eq!(creds.password, "pässwörd");
    }

    #[test]
    fn percent_decode_round_trips_non_ascii() {
        for raw in ["pässwörd", "пароль", "密码", "emoji🔒", "plain"] {
            let encoded: String = raw
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            assert_eq!(percent_decode(&encoded), raw, "round trip failed: {raw}");
        }
    }

    #[test]
    fn percent_decode_passes_through_bad_escapes() {
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("a%2"), "a%2");
    }

    #[test]
    fn keeps_password_only_credentials() {
        let cfg = ProxyConfig::parse("socks5://:pass@host:1080").unwrap();
        let creds = cfg.credentials.expect("password-only userinfo dropped");
        assert_eq!(creds.username, "");
        assert_eq!(creds.password, "pass");
    }

    #[test]
    fn keeps_username_only_credentials() {
        let cfg = ProxyConfig::parse("socks5://user@host:1080").unwrap();
        let creds = cfg.credentials.expect("username-only userinfo dropped");
        assert_eq!(creds.username, "user");
        assert_eq!(creds.password, "");
    }

    #[test]
    fn empty_userinfo_is_no_credentials() {
        assert!(ProxyConfig::parse("socks5://host:1080")
            .unwrap()
            .credentials
            .is_none());
        assert!(ProxyConfig::parse("socks5://@host:1080")
            .unwrap()
            .credentials
            .is_none());
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
