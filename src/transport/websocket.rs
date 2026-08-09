//! WebSocket transport with optional proxy tunnelling and TLS.
//!
//! The output is a [`WebSocketStream`] over a type-erased
//! [`AsyncStream`] so the proxy and direct paths share one signature. Callers
//! don't have to thread generics through their session structs.

use std::time::Duration;

use rustls_pki_types::ServerName;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use crate::error::Error;
use crate::transport::proxy::{connect_via_proxy, ProxyConfig};
use crate::transport::{authority, tls_connector, unbracket, AsyncStream};

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
// proxy connect is multi-phase (tcp, tls, CONNECT, reply), so looser than direct
const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(45);
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const WS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Type alias for the WebSocket we produce. The inner stream is type-erased so
/// the proxy and direct paths converge on one type.
pub type SteamWebSocket = WebSocketStream<Box<dyn AsyncStream>>;

/// Open a WebSocket (WS or WSS) to `url`, optionally tunneled through `proxy`.
///
/// Steps:
///
/// 1. Establish TCP — direct or via the proxy.
/// 2. If the URL is `wss://`, do a TLS handshake using webpki roots.
/// 3. Do the WebSocket client handshake.
pub async fn connect_ws(url: &str, proxy: Option<&ProxyConfig>) -> Result<SteamWebSocket, Error> {
    let parsed = url::Url::parse(url)?;
    let host = unbracket(
        parsed
            .host_str()
            .ok_or_else(|| Error::InvalidUrl("ws url missing host".into()))?,
    )
    .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| Error::InvalidUrl("ws url missing port".into()))?;
    let scheme = parsed.scheme();
    let is_wss = match scheme {
        "wss" => true,
        "ws" => false,
        other => return Err(Error::InvalidUrl(format!("unsupported ws scheme: {other}"))),
    };

    debug!(%host, port, %scheme, "ws connect");

    // 1. TCP — proxy or direct
    let tcp: Box<dyn AsyncStream> = if let Some(p) = proxy {
        timeout(PROXY_CONNECT_TIMEOUT, connect_via_proxy(p, &host, port))
            .await
            .map_err(|_| Error::Timeout("proxy connect"))??
    } else {
        let addr = authority(&host, port);
        let stream = timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| Error::Timeout("tcp connect"))??;
        Box::new(stream)
    };

    // 2. TLS (if wss)
    let stream: Box<dyn AsyncStream> = if is_wss {
        let connector = tls_connector();
        let server_name = ServerName::try_from(host.clone())
            .map_err(|e| Error::Tls(format!("invalid sni: {e}")))?;
        let tls_stream = timeout(TLS_HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .map_err(|_| Error::Timeout("tls handshake"))?
            .map_err(|e| Error::Tls(e.to_string()))?;
        Box::new(tls_stream)
    } else {
        tcp
    };

    // 3. WS handshake
    let request = url
        .into_client_request()
        .map_err(|e| Error::WebSocket(format!("bad request: {e}")))?;
    let (ws, _resp) = timeout(
        WS_HANDSHAKE_TIMEOUT,
        tokio_tungstenite::client_async_with_config(request, stream, None),
    )
    .await
    .map_err(|_| Error::Timeout("ws handshake"))??;

    Ok(ws)
}
