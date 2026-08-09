//! Offline coverage for the proxy handshakes in `transport::proxy`.
//!
//! Both flavours are driven against a real `TcpListener` on `127.0.0.1:0` with
//! the proxy side spoken by hand: HTTP `CONNECT` (RFC 7231 §4.3.6) and SOCKS5
//! (RFC 1928, plus RFC 1929 for the authenticated case). The exact bytes the
//! client puts on the wire are asserted rather than assumed.

use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use steamroids::transport::proxy::{connect_via_proxy, ProxyConfig, ProxyCredentials, ProxyKind};
use steamroids::transport::AsyncStream;
use steamroids::Error;

const TARGET_HOST: &str = "cm.steampowered.com";
const TARGET_PORT: u16 = 27017;
const TARGET: &str = "cm.steampowered.com:27017";
const USERNAME: &str = "bot01";
const PASSWORD: &str = "hunter2";
// base64("bot01:hunter2"), pinned so the assertion doesn't re-derive it
const EXPECTED_BASIC: &str = "Ym90MDE6aHVudGVyMg==";
// socks5 CONNECT granted, bound to 0.0.0.0:0
const SOCKS5_GRANTED: [u8; 10] = [5, 0, 0, 1, 0, 0, 0, 0, 0, 0];

fn creds() -> ProxyCredentials {
    ProxyCredentials {
        username: USERNAME.to_string(),
        password: PASSWORD.to_string(),
    }
}

fn proxy(kind: ProxyKind, port: u16, credentials: Option<ProxyCredentials>) -> ProxyConfig {
    ProxyConfig {
        kind,
        host: "127.0.0.1".to_string(),
        port,
        credentials,
        tls_to_proxy: false,
    }
}

/// Take the `Err` half. `Box<dyn AsyncStream>` isn't `Debug`, so `expect_err`
/// can't be used on a connect result.
fn expect_err(result: Result<Box<dyn AsyncStream>, Error>, why: &str) -> Error {
    match result {
        Ok(_) => panic!("{why}"),
        Err(e) => e,
    }
}

async fn loopback_listener() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

/// Read a CONNECT request head, returning its lines without the trailing CRLF.
/// Panics if the request ends before the blank line that terminates it.
async fn read_request<R: AsyncBufRead + Unpin>(reader: &mut R) -> Vec<String> {
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .await
            .expect("read request line");
        assert_ne!(read, 0, "request ended without a blank line");
        if line == "\r\n" {
            return lines;
        }
        lines.push(line.trim_end_matches("\r\n").to_string());
    }
}

async fn read_exact_n(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).await.expect("read exact");
    buf
}

/// Read the RFC 1928 greeting, returning the method bytes the client offered.
async fn socks5_greeting(stream: &mut TcpStream) -> Vec<u8> {
    let head = read_exact_n(stream, 2).await;
    assert_eq!(head[0], 5, "socks version");
    read_exact_n(stream, usize::from(head[1])).await
}

/// Read the RFC 1929 username/password sub-negotiation.
async fn socks5_userpass(stream: &mut TcpStream) -> (String, String) {
    let head = read_exact_n(stream, 2).await;
    assert_eq!(head[0], 1, "userpass sub-negotiation version");
    let username = read_exact_n(stream, usize::from(head[1])).await;
    let plen = read_exact_n(stream, 1).await[0];
    let password = read_exact_n(stream, usize::from(plen)).await;
    (
        String::from_utf8(username).expect("utf8 username"),
        String::from_utf8(password).expect("utf8 password"),
    )
}

/// Read the RFC 1928 CONNECT request, returning its domain target.
async fn socks5_request(stream: &mut TcpStream) -> (String, u16) {
    let head = read_exact_n(stream, 4).await;
    assert_eq!(head[0], 5, "socks version");
    assert_eq!(head[1], 1, "CONNECT command");
    assert_eq!(head[3], 3, "domain address type");
    let len = read_exact_n(stream, 1).await[0];
    let host = read_exact_n(stream, usize::from(len)).await;
    let port = read_exact_n(stream, 2).await;
    (
        String::from_utf8(host).expect("utf8 host"),
        u16::from_be_bytes([port[0], port[1]]),
    )
}

// ---- HTTP CONNECT ---------------------------------------------------------

#[tokio::test]
async fn connect_writes_the_request_line_and_host_header() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let lines = {
            let mut reader = BufReader::new(&mut stream);
            read_request(&mut reader).await
        };
        // status, blank line and payload in one write: the client must keep the
        // bytes it buffered past the blank line
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\nTUNNELLED")
            .await
            .expect("write reply");
        stream.flush().await.expect("flush");
        lines
    });

    let mut stream = connect_via_proxy(
        &proxy(ProxyKind::HttpConnect, port, None),
        TARGET_HOST,
        TARGET_PORT,
    )
    .await
    .expect("proxy accepted the tunnel");

    let mut payload = [0u8; 9];
    stream
        .read_exact(&mut payload)
        .await
        .expect("tunnel payload");
    assert_eq!(
        &payload, b"TUNNELLED",
        "bytes buffered past the blank line must survive"
    );

    let lines = server.await.expect("server task");
    assert_eq!(lines[0], format!("CONNECT {TARGET} HTTP/1.1"));
    assert_eq!(lines[1], format!("Host: {TARGET}"));
    assert!(
        !lines.iter().any(|l| l.starts_with("Proxy-Authorization:")),
        "no credentials configured, yet one was sent: {lines:?}"
    );
}

#[tokio::test]
async fn connect_sends_base64_basic_proxy_authorization() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let lines = {
            let mut reader = BufReader::new(&mut stream);
            read_request(&mut reader).await
        };
        stream
            .write_all(b"HTTP/1.1 200 OK\r\n\r\n")
            .await
            .expect("write reply");
        lines
    });

    connect_via_proxy(
        &proxy(ProxyKind::HttpConnect, port, Some(creds())),
        TARGET_HOST,
        TARGET_PORT,
    )
    .await
    .expect("proxy accepted the tunnel");

    let lines = server.await.expect("server task");
    assert!(
        lines.contains(&format!("Proxy-Authorization: Basic {EXPECTED_BASIC}")),
        "credentials missing or mis-encoded: {lines:?}"
    );
}

#[tokio::test]
async fn connect_rejects_a_407_challenge() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        {
            let mut reader = BufReader::new(&mut stream);
            read_request(&mut reader).await;
        }
        stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
            )
            .await
            .expect("write reply");
    });

    let err = expect_err(
        connect_via_proxy(
            &proxy(ProxyKind::HttpConnect, port, None),
            TARGET_HOST,
            TARGET_PORT,
        )
        .await,
        "a 407 must not read as an open tunnel",
    );
    match err {
        Error::HttpConnect(text) => assert!(text.contains("407"), "{text}"),
        other => panic!("expected HttpConnect, got {other:?}"),
    }
    server.await.expect("server task");
}

#[tokio::test]
async fn connect_rejects_an_endless_header_block() {
    let (listener, port) = loopback_listener().await;
    let (hold_tx, hold_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        {
            let mut reader = BufReader::new(&mut stream);
            read_request(&mut reader).await;
        }
        let mut reply = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..200 {
            reply.extend_from_slice(b"X-Pad: 1\r\n");
        }
        // never a blank line; the client must bail on its own header cap
        let _ = stream.write_all(&reply).await;
        // hold the socket open so the client can't see EOF instead
        let _ = hold_rx.await;
    });

    let err = expect_err(
        connect_via_proxy(
            &proxy(ProxyKind::HttpConnect, port, None),
            TARGET_HOST,
            TARGET_PORT,
        )
        .await,
        "an unterminated header block must fail",
    );
    match err {
        Error::HttpConnect(text) => assert!(text.contains("too many"), "{text}"),
        other => panic!("expected HttpConnect, got {other:?}"),
    }
    drop(hold_tx);
    server.await.expect("server task");
}

#[tokio::test]
async fn connect_times_out_on_a_proxy_that_stalls_mid_headers() {
    let (listener, port) = loopback_listener().await;
    let (stalled_tx, stalled_rx) = oneshot::channel::<()>();
    let (hold_tx, hold_rx) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        {
            let mut reader = BufReader::new(&mut stream);
            read_request(&mut reader).await;
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\n")
            .await
            .expect("write status");
        stream.flush().await.expect("flush");
        stalled_tx.send(()).expect("signal");
        // hold the socket open: EOF would end the read early, not time out
        let _ = hold_rx.await;
    });

    let client = tokio::spawn(async move {
        connect_via_proxy(
            &proxy(ProxyKind::HttpConnect, port, None),
            TARGET_HOST,
            TARGET_PORT,
        )
        .await
    });
    stalled_rx.await.expect("proxy sent a partial header block");
    // tcp + write are done; only the read deadline is left, so let the paused
    // clock run to it instead of waiting 30s
    tokio::time::pause();

    let err = expect_err(
        client.await.expect("client task"),
        "a stalled proxy must time out",
    );
    match err {
        Error::Timeout(what) => assert_eq!(what, "http connect read headers"),
        other => panic!("expected Timeout, got {other:?}"),
    }
    drop(hold_tx);
    server.await.expect("server task");
}

// ---- SOCKS5 ---------------------------------------------------------------

#[tokio::test]
async fn socks5_negotiates_unauthenticated_and_tunnels() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let methods = socks5_greeting(&mut stream).await;
        assert!(
            methods.contains(&0),
            "no-auth method not offered: {methods:?}"
        );
        stream.write_all(&[5, 0]).await.expect("method reply");
        let target = socks5_request(&mut stream).await;
        stream
            .write_all(&SOCKS5_GRANTED)
            .await
            .expect("connect reply");
        stream.write_all(b"TUNNELLED").await.expect("payload");
        stream.flush().await.expect("flush");
        target
    });

    let mut stream = connect_via_proxy(
        &proxy(ProxyKind::Socks5, port, None),
        TARGET_HOST,
        TARGET_PORT,
    )
    .await
    .expect("socks5 granted the tunnel");
    let mut payload = [0u8; 9];
    stream
        .read_exact(&mut payload)
        .await
        .expect("tunnel payload");
    assert_eq!(&payload, b"TUNNELLED");

    let (host, target_port) = server.await.expect("server task");
    assert_eq!(host, TARGET_HOST);
    assert_eq!(target_port, TARGET_PORT);
}

#[tokio::test]
async fn socks5_sends_the_configured_username_and_password() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let methods = socks5_greeting(&mut stream).await;
        assert!(
            methods.contains(&2),
            "userpass method not offered: {methods:?}"
        );
        stream.write_all(&[5, 2]).await.expect("method reply");
        let userpass = socks5_userpass(&mut stream).await;
        stream.write_all(&[1, 0]).await.expect("auth reply");
        socks5_request(&mut stream).await;
        stream
            .write_all(&SOCKS5_GRANTED)
            .await
            .expect("connect reply");
        stream.flush().await.expect("flush");
        userpass
    });

    connect_via_proxy(
        &proxy(ProxyKind::Socks5, port, Some(creds())),
        TARGET_HOST,
        TARGET_PORT,
    )
    .await
    .expect("socks5 granted the tunnel");

    let (username, password) = server.await.expect("server task");
    assert_eq!(username, USERNAME);
    assert_eq!(password, PASSWORD);
}

#[tokio::test]
async fn socks5_surfaces_a_refused_connect() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        socks5_greeting(&mut stream).await;
        stream.write_all(&[5, 0]).await.expect("method reply");
        socks5_request(&mut stream).await;
        // REP 0x02: connection not allowed by ruleset
        stream
            .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .expect("connect reply");
        stream.flush().await.expect("flush");
    });

    let err = expect_err(
        connect_via_proxy(
            &proxy(ProxyKind::Socks5, port, None),
            TARGET_HOST,
            TARGET_PORT,
        )
        .await,
        "a refused CONNECT must not read as a tunnel",
    );
    assert!(matches!(err, Error::Socks5(_)), "{err:?}");
    server.await.expect("server task");
}

#[tokio::test]
async fn socks5_rejects_an_unoffered_auth_method() {
    let (listener, port) = loopback_listener().await;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        socks5_greeting(&mut stream).await;
        // 0xff: no acceptable methods
        stream.write_all(&[5, 0xff]).await.expect("method reply");
        stream.flush().await.expect("flush");
    });

    let err = expect_err(
        connect_via_proxy(
            &proxy(ProxyKind::Socks5, port, None),
            TARGET_HOST,
            TARGET_PORT,
        )
        .await,
        "a rejected method must fail the handshake",
    );
    assert!(matches!(err, Error::Socks5(_)), "{err:?}");
    server.await.expect("server task");
}

#[tokio::test]
async fn connect_fails_fast_when_nothing_is_listening() {
    // bind then drop frees an ephemeral port, but a concurrent test binary can
    // re-bind it before we dial. retry on a fresh port instead of flaking.
    let mut last = None;
    for _ in 0..8 {
        let (listener, port) = loopback_listener().await;
        drop(listener);

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            connect_via_proxy(
                &proxy(ProxyKind::HttpConnect, port, None),
                TARGET_HOST,
                TARGET_PORT,
            ),
        )
        .await
        .expect("connect to a closed port should not hang");

        // Ok means the port was stolen and something answered; try another
        if let Err(err) = outcome {
            last = Some(err);
            break;
        }
    }

    let err = last.expect("every ephemeral port was re-bound by another listener");
    assert!(matches!(err, Error::Io(_)), "{err:?}");
}
