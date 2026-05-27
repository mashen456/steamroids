//! Confirm a proxy works by tunneling a small HTTP/1.1 GET through it.
//!
//! ```bash
//! PROXY_URL="socks5://user:pass@host:1080" cargo run --example 02_proxy_test
//! ```
//!
//! Hits `http://httpbin.org/ip` and prints the JSON. If the proxy is working,
//! the `origin` field should be the proxy's exit IP — not your local one.

use steamroids::transport::proxy::{connect_via_proxy, ProxyConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,steamroids=debug".into()),
        )
        .init();

    let proxy_url =
        std::env::var("PROXY_URL").expect("set PROXY_URL to a socks5:// or http:// URL");

    let proxy = ProxyConfig::parse(&proxy_url).expect("parse proxy URL");
    println!("Using proxy: {}", proxy.display());

    let mut stream = connect_via_proxy(&proxy, "httpbin.org", 80)
        .await
        .expect("proxy connect");

    let request = b"GET /ip HTTP/1.1\r\nHost: httpbin.org\r\nConnection: close\r\n\r\n";
    stream.write_all(request).await.expect("write request");
    stream.flush().await.expect("flush");

    let mut buf = Vec::with_capacity(2048);
    stream.read_to_end(&mut buf).await.expect("read response");

    let body = String::from_utf8_lossy(&buf);
    println!("{body}");
}
