//! Open a WSS connection to a public echo server, optionally through a proxy.
//!
//! ```bash
//! # Direct
//! cargo run --example 03_ws_echo
//!
//! # Via proxy
//! PROXY_URL="socks5://user:pass@host:1080" cargo run --example 03_ws_echo
//! ```
//!
//! Sends `"hello"` and prints whatever comes back. Public echo server used:
//! `wss://echo.websocket.events`. If this works through your proxy, the proxy
//! is good for Steam's WSS endpoints too — same TLS+WS plumbing.

use futures_util::{SinkExt, StreamExt};
use steamroids::transport::connect_ws;
use steamroids::transport::proxy::ProxyConfig;
use tokio_tungstenite::tungstenite::Message;

const ECHO_URL: &str = "wss://echo.websocket.events";

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,steamroids=debug".into()),
        )
        .init();

    let proxy = std::env::var("PROXY_URL")
        .ok()
        .map(|url| ProxyConfig::parse(&url).expect("parse proxy URL"));

    if let Some(p) = &proxy {
        println!("Using proxy: {}", p.display());
    } else {
        println!("Direct connection (no proxy)");
    }

    let mut ws = connect_ws(ECHO_URL, proxy.as_ref())
        .await
        .expect("ws connect");

    // The echo server greets you with a welcome message — read and discard it.
    if let Some(Ok(welcome)) = ws.next().await {
        println!("server: {welcome:?}");
    }

    ws.send(Message::Text("hello from steamroids".into()))
        .await
        .expect("send");
    if let Some(Ok(reply)) = ws.next().await {
        println!("echoed: {reply:?}");
    }

    ws.close(None).await.ok();
}
