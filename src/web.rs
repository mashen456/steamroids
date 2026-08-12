//! Steam web session support.
//!
//! A logged-on CM session can mint a web access token for the same account,
//! which authenticates requests to `steamcommunity.com` and the store without
//! a second login. This is what the Steam client itself does so its embedded
//! browser is signed in.
//!
//! The exchange must ride the CM session: Steam refuses
//! `GenerateAccessTokenForApp` over the plain `WebAPI` for the
//! `SteamClient`-platform refresh tokens this crate issues, answering
//! `AccessDenied`. See [`crate::auth`] for the platform split.

use std::sync::Arc;

use crate::auth::RefreshToken;
use crate::proto::{
    CAuthenticationAccessTokenGenerateForAppRequest,
    CAuthenticationAccessTokenGenerateForAppResponse,
};
use crate::ratelimit::RateLimiter;
use crate::session::SessionHandle;
use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

// k_ETokenRenewalType_None: leave the refresh token as it is
const TOKEN_RENEWAL_NONE: i32 = 0;

/// Exchange this session's refresh token for a web access token.
///
/// `refresh_token` must be the token this session logged on with. Pass the
/// same `proxy` the session was spawned with: web requests made through the
/// returned [`WebSession`] then leave via the same exit as the CM session they
/// authenticate as, which is what a per-account proxy deployment needs.
///
/// # Errors
///
/// [`Error::Remote`] if Steam rejects the exchange or returns no token, plus
/// any transport error from the underlying session call.
pub async fn request_web_token(
    session: &SessionHandle,
    refresh_token: &RefreshToken,
    proxy: Option<&ProxyConfig>,
) -> Result<WebSession> {
    let req = CAuthenticationAccessTokenGenerateForAppRequest {
        refresh_token: Some(refresh_token.expose().to_string()),
        steamid: Some(session.steam_id()),
        renewal_type: Some(TOKEN_RENEWAL_NONE),
    };
    let resp: CAuthenticationAccessTokenGenerateForAppResponse = session
        .call_service("Authentication", "GenerateAccessTokenForApp", 1, &req)
        .await?;

    let access_token = resp.access_token.filter(|t| !t.is_empty()).ok_or_else(|| {
        Error::Remote("GenerateAccessTokenForApp returned no access token".into())
    })?;

    Ok(WebSession {
        steam_id: session.steam_id(),
        access_token,
        session_id: None,
        proxy: proxy.cloned(),
        rate_limiter: None,
    })
}

/// An authenticated Steam web session.
///
/// `Debug` is implemented by hand to keep the access token out of logs and
/// traces, matching [`RefreshToken`]'s redaction.
#[derive(Clone)]
pub struct WebSession {
    steam_id: u64,
    access_token: String,
    session_id: Option<String>,
    proxy: Option<ProxyConfig>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl std::fmt::Debug for WebSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSession")
            .field("steam_id", &self.steam_id)
            .field("access_token", &"<redacted>")
            .field("session_id", &self.session_id)
            .field("proxy", &self.proxy)
            .field("rate_limiter", &self.rate_limiter.is_some())
            .finish()
    }
}

impl WebSession {
    /// The account this session authenticates as.
    pub fn steam_id(&self) -> u64 {
        self.steam_id
    }

    /// The minted web access token.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Attach a `sessionid` cookie.
    ///
    /// Only needed for state-changing POSTs, which Steam guards with a CSRF
    /// token that must match this cookie. Plain GETs (profile pages, GCPD)
    /// authenticate on `steamLoginSecure` alone. The value is caller-supplied
    /// so this crate takes no random-number dependency; any opaque string
    /// works as long as the same value goes in the form field.
    #[must_use]
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Pace requests from this session through a shared limiter.
    ///
    /// Steam rate-limits by exit IP, so share one limiter across every session
    /// that leaves through the same proxy.
    #[must_use]
    pub fn with_rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// The value for a `Cookie:` request header, authenticating as this account.
    ///
    /// [`Self::get`] sends this header through the session's proxy for you. A
    /// caller driving its own HTTP client instead must route it through that
    /// same proxy, or the request leaves via a different exit than the CM
    /// session it authenticates as.
    ///
    /// ```
    /// # use steamroids::web::WebSession;
    /// # fn demo(web: &WebSession) {
    /// let cookie = web.cookie_header();
    /// assert!(cookie.starts_with("steamLoginSecure="));
    /// # }
    /// ```
    #[must_use]
    pub fn cookie_header(&self) -> String {
        // steamLoginSecure is "<steamid64>||<access token>", url-encoded
        let raw = format!("{}||{}", self.steam_id, self.access_token);
        let encoded: String = url::form_urlencoded::byte_serialize(raw.as_bytes()).collect();
        match &self.session_id {
            Some(sid) => format!("steamLoginSecure={encoded}; sessionid={sid}"),
            None => format!("steamLoginSecure={encoded}"),
        }
    }

    // shared client build so get() and its test agree on proxy handling
    fn http_client(&self) -> Result<reqwest::Client> {
        crate::http::client(self.proxy.as_ref())
    }

    /// Fetch `url` authenticated as this account.
    ///
    /// The request carries this session's `steamLoginSecure` cookie and leaves
    /// through the same proxy the session was built with, so a web request and
    /// the CM session it authenticates as share an exit.
    ///
    /// Returns the response body. A non-success HTTP status is an error rather
    /// than a body. This is a transport-level guard, not a signed-out check:
    /// `reqwest` follows redirects, so a signed-out request lands on the login
    /// page as a normal 200 and callers who care must inspect the body.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] on a transport failure or a non-success status, and
    /// [`Error::InvalidConfig`] if the proxy configuration is unusable.
    pub async fn get(&self, url: &str) -> Result<String> {
        if let Some(limiter) = &self.rate_limiter {
            limiter.acquire().await;
        }

        let response = self
            .http_client()?
            .get(url)
            .header(reqwest::header::COOKIE, self.cookie_header())
            .send()
            .await
            .map_err(|e| Error::Network(format!("web get {url}: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Network(format!("web get {url}: HTTP {status}")));
        }

        response
            .text()
            .await
            .map_err(|e| Error::Network(format!("web get {url}: body: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use prost::Message;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;
    use crate::codec::SteamMessage;
    use crate::proto::CMsgProtoBufHeader;
    use crate::session::driver::{Command, EMSG_SERVICE_METHOD_RESPONSE};

    #[tokio::test]
    async fn request_web_token_calls_generate_access_token_for_app() {
        let (handle, mut commands, _events, _snapshots) =
            SessionHandle::for_test(76_561_198_000_000_001);

        let task = tokio::spawn(async move {
            request_web_token(
                &handle,
                &RefreshToken::new("stored-refresh-token".to_string()),
                None,
            )
            .await
        });

        let Some(Command::Request {
            job_name,
            body,
            reply,
            ..
        }) = commands.recv().await
        else {
            panic!("expected a Request command");
        };
        assert_eq!(
            job_name.as_deref(),
            Some("Authentication.GenerateAccessTokenForApp#1")
        );

        let sent = CAuthenticationAccessTokenGenerateForAppRequest::decode(body.as_slice())
            .expect("decode request");
        assert_eq!(sent.refresh_token.as_deref(), Some("stored-refresh-token"));
        assert_eq!(sent.steamid, Some(76_561_198_000_000_001));
        // k_ETokenRenewalType_None: do not rotate the refresh token
        assert_eq!(sent.renewal_type, Some(0));

        let resp = CAuthenticationAccessTokenGenerateForAppResponse {
            access_token: Some("minted-access-token".to_string()),
            refresh_token: None,
        };
        reply
            .send(Ok(SteamMessage {
                emsg: EMSG_SERVICE_METHOD_RESPONSE,
                header: CMsgProtoBufHeader::default(),
                body: resp.encode_to_vec(),
            }))
            .expect("send reply");

        let web = task.await.expect("task").expect("request_web_token");
        assert_eq!(web.access_token(), "minted-access-token");
        assert_eq!(web.steam_id(), 76_561_198_000_000_001);
    }

    #[tokio::test]
    async fn request_web_token_errors_when_steam_returns_no_token() {
        let (handle, mut commands, _events, _snapshots) = SessionHandle::for_test(5);

        let task = tokio::spawn(async move {
            request_web_token(&handle, &RefreshToken::new("t".to_string()), None).await
        });

        let Some(Command::Request { reply, .. }) = commands.recv().await else {
            panic!("expected a Request command");
        };
        let resp = CAuthenticationAccessTokenGenerateForAppResponse {
            access_token: None,
            refresh_token: None,
        };
        reply
            .send(Ok(SteamMessage {
                emsg: EMSG_SERVICE_METHOD_RESPONSE,
                header: CMsgProtoBufHeader::default(),
                body: resp.encode_to_vec(),
            }))
            .expect("send reply");

        let err = task.await.expect("task").unwrap_err();
        assert!(matches!(err, Error::Remote(_)), "{err:?}");
    }

    #[test]
    fn cookie_header_encodes_the_pipe_separator() {
        let web = WebSession {
            steam_id: 76_561_198_000_000_001,
            access_token: "eyJhbGci.eyJzdWIi.sig-part_x".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        };
        assert_eq!(
            web.cookie_header(),
            "steamLoginSecure=76561198000000001%7C%7CeyJhbGci.eyJzdWIi.sig-part_x"
        );
    }

    #[test]
    fn cookie_header_appends_a_session_id_when_set() {
        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        }
        .with_session_id("abc123");
        assert_eq!(
            web.cookie_header(),
            "steamLoginSecure=1%7C%7Ctok; sessionid=abc123"
        );
    }

    #[test]
    fn debug_does_not_leak_the_access_token() {
        let web = WebSession {
            steam_id: 1,
            access_token: "super-secret".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        };
        assert!(!format!("{web:?}").contains("super-secret"));
    }

    #[tokio::test]
    async fn get_speaks_socks5_to_the_configured_proxy() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();

        let proxy = ProxyConfig::parse(&format!("socks5://127.0.0.1:{port}")).expect("parse proxy");
        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: Some(proxy),
            rate_limiter: None,
        };

        // watch what the client actually sends the "proxy": a real SOCKS5
        // greeting opens with version byte 0x05. bounded so a dropped or
        // never-made connection can't hang the test.
        let observed = tokio::spawn(async move {
            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_secs(5), listener.accept()).await
            else {
                return None;
            };
            let mut byte = [0u8; 1];
            match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte)).await {
                Ok(Ok(n)) if n > 0 => Some(byte[0]),
                _ => None,
            }
        });

        // this never completes: we aren't implementing a real SOCKS5 server.
        // bounded so giving up here doesn't stall the test either.
        let _ =
            tokio::time::timeout(Duration::from_secs(5), web.get("http://example.invalid/")).await;

        assert_eq!(
            observed.await.expect("listener task"),
            Some(0x05),
            "expected a SOCKS5 greeting (version byte 0x05) through the configured proxy"
        );
    }

    #[tokio::test]
    async fn get_surfaces_a_transport_failure() {
        // 127.0.0.1:1 has nothing listening, so the request fails at connect
        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        };
        let err = web.get("http://127.0.0.1:1/").await.unwrap_err();
        assert!(matches!(err, Error::Network(_)), "{err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn get_waits_on_an_attached_rate_limiter() {
        // burn the first slot so the next acquire must wait
        let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(30)));
        limiter.acquire().await;

        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: Some(Arc::clone(&limiter)),
        };

        let start = tokio::time::Instant::now();
        // connect refused, but the limiter must be consulted BEFORE the request
        let _ = web.get("http://127.0.0.1:1/").await;
        assert!(start.elapsed() >= Duration::from_secs(30));
    }

    #[tokio::test]
    async fn get_without_a_limiter_does_not_wait() {
        // real bound listener, not "connect refused" -- refusal timing is
        // os-dependent and under start_paused races reqwest's own connect
        // timeout. no pause needed: no limiter means no sleep, so real
        // wall-clock time must stay fast regardless.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            {
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    let read = reader.read_line(&mut line).await.expect("read request");
                    assert_ne!(read, 0, "request ended without a blank line");
                    if line == "\r\n" {
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write reply");
            stream.flush().await.expect("flush");
        });

        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        };
        let start = std::time::Instant::now();
        let _ = web.get(&format!("http://127.0.0.1:{port}/")).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn get_surfaces_a_non_success_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            {
                // drain full request head, not just the line: unread bytes at
                // close can rst the reply away
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    let read = reader.read_line(&mut line).await.expect("read request");
                    assert_ne!(read, 0, "request ended without a blank line");
                    if line == "\r\n" {
                        break;
                    }
                }
            }
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write reply");
            stream.flush().await.expect("flush");
        });

        let web = WebSession {
            steam_id: 1,
            access_token: "tok".to_string(),
            session_id: None,
            proxy: None,
            rate_limiter: None,
        };
        let err = web
            .get(&format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap_err();
        match err {
            Error::Network(text) => assert!(text.contains("404"), "{text}"),
            other => panic!("expected Network, got {other:?}"),
        }
        server.await.expect("server task");
    }
}
