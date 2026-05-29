//! Thin HTTP client for the Steam `IAuthenticationService` `WebAPI`.
//!
//! All sign-in calls go to `https://api.steampowered.com/IAuthenticationService/<Method>/v1/`.
//! Requests are protobufs (base64'd into a query string for GET or an
//! `application/x-www-form-urlencoded` body for POST); responses are raw
//! binary protobufs. Steam carries the result code in an `x-eresult` header
//! rather than in the protobuf payload.

use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use prost::Message;
use reqwest::{Client, Proxy};
use tracing::debug;
use url::Url;

use crate::auth::signin::SignInOutcome;
use crate::transport::proxy::{ProxyConfig, ProxyCredentials, ProxyKind};
use crate::{Error, Result};

const BASE_URL: &str = "https://api.steampowered.com/IAuthenticationService/";

/// Network operation budget (connect + transfer) for any one Steam `WebAPI` call.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// TCP-level connect budget. Steam's edge is usually <1s; 30s is forgiving.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Steam's `EResult` enum value, lifted from the `x-eresult` header.
///
/// Wrapped so calling code is forced to acknowledge the integer is a Steam
/// result code and not a generic HTTP status. Only the codes we actually
/// branch on are named; everything else is treated as "unknown failure".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EResult(pub i32);

impl EResult {
    pub(crate) const OK: Self = Self(1);
    pub(crate) const INVALID_PASSWORD: Self = Self(5);
    pub(crate) const ACCESS_DENIED: Self = Self(15);
    pub(crate) const EXPIRED: Self = Self(27);
    pub(crate) const ACCOUNT_LOGON_DENIED: Self = Self(63);
    pub(crate) const RATE_LIMIT_EXCEEDED: Self = Self(84);
    pub(crate) const ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR: Self = Self(85);
    pub(crate) const TWO_FACTOR_CODE_MISMATCH: Self = Self(88);
}

/// HTTP verb to use for a call.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HttpMethod {
    /// GET with the protobuf as `?input_protobuf_encoded=...` in the query.
    Get,
    /// POST with the protobuf in the form-urlencoded body.
    Post,
}

/// The flow context the `EResult` mapping is being applied in.
///
/// `EResult` 5 (`InvalidPassword`) means "bad password" during the password
/// flow but "the refresh token Steam was handed is no good" during the
/// refresh flow. Same wire code, different user-facing outcome.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FlowKind {
    Password,
    RefreshToken,
}

/// Translate a non-OK [`EResult`] into a [`SignInOutcome`] when the mapping
/// is well-defined for the current flow, or `None` when the caller should
/// raise an error instead.
pub(crate) fn map_non_ok_eresult(er: EResult, flow: FlowKind) -> Option<SignInOutcome> {
    // Codes that depend on the flow context.
    match (er, flow) {
        (EResult::INVALID_PASSWORD, FlowKind::Password) => {
            return Some(SignInOutcome::InvalidCredentials);
        }
        (
            EResult::INVALID_PASSWORD | EResult::ACCESS_DENIED | EResult::EXPIRED,
            FlowKind::RefreshToken,
        ) => return Some(SignInOutcome::TokenRejected),
        _ => {}
    }

    // Codes that are flow-agnostic.
    match er {
        EResult::ACCOUNT_LOGON_DENIED => Some(SignInOutcome::NeedsEmailGuardCode {
            email_domain: String::new(),
        }),
        EResult::ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR | EResult::TWO_FACTOR_CODE_MISMATCH => {
            Some(SignInOutcome::NeedsMobileGuardCode)
        }
        EResult::RATE_LIMIT_EXCEEDED => Some(SignInOutcome::RateLimited {
            retry_hint: Some(Duration::from_secs(60)),
        }),
        _ => None,
    }
}

/// Build the canonical URL for a method name on the auth service.
pub(crate) fn url_for(method: &str) -> Result<Url> {
    let raw = format!("{BASE_URL}{method}/v1/");
    Url::parse(&raw).map_err(Error::from)
}

/// HTTP client pre-configured for the Steam `WebAPI`.
pub(crate) struct WebApiClient {
    http: Client,
}

impl WebApiClient {
    /// Construct a fresh client, optionally routed through `proxy`.
    pub(crate) fn new(proxy: Option<&ProxyConfig>) -> Result<Self> {
        let mut builder = Client::builder()
            .timeout(TOTAL_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent("steamroids/0.0.1");

        if let Some(p) = proxy {
            builder = builder.proxy(build_proxy(p)?);
        }

        let http = builder
            .build()
            .map_err(|e| Error::InvalidConfig(format!("reqwest client: {e}")))?;
        Ok(Self { http })
    }

    /// Dispatch one auth call. Encodes `req` to base64, sends it, parses the
    /// `x-eresult` header, and decodes the response body as `Resp`.
    ///
    /// On non-200 HTTP statuses we still try to read the body — Steam returns
    /// non-200 with a meaningful `x-eresult` for some failures.
    pub(crate) async fn call<Req, Resp>(
        &self,
        method: &str,
        http_method: HttpMethod,
        req: &Req,
    ) -> Result<(EResult, Resp)>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let url = url_for(method)?;
        let payload_b64 = B64.encode(req.encode_to_vec());

        let request = match http_method {
            HttpMethod::Get => self
                .http
                .get(url.clone())
                .query(&[("input_protobuf_encoded", payload_b64.as_str())]),
            HttpMethod::Post => self
                .http
                .post(url.clone())
                .form(&[("input_protobuf_encoded", payload_b64.as_str())]),
        };

        let response = request
            .send()
            .await
            .map_err(|e| Error::AuthRejected(format!("http {method}: {e}")))?;

        let er_value = response
            .headers()
            .get("x-eresult")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0);
        let er = EResult(er_value);

        let body = response
            .bytes()
            .await
            .map_err(|e| Error::AuthRejected(format!("http {method} read body: {e}")))?;

        debug!(
            target: "steamroids::auth::webapi",
            method,
            %url,
            eresult = er.0,
            body_len = body.len(),
            "webapi call complete"
        );

        // An empty body for an OK result is fine (some endpoints return no
        // payload); prost decodes the empty slice to the default message.
        let resp = Resp::decode(body.as_ref())
            .map_err(|e| Error::AuthRejected(format!("decode {method} response: {e}")))?;

        Ok((er, resp))
    }
}

fn build_proxy(cfg: &ProxyConfig) -> Result<Proxy> {
    if cfg.tls_to_proxy {
        return Err(Error::InvalidConfig(
            "https:// proxies (TLS-to-proxy) not yet supported for WebAPI".into(),
        ));
    }

    let scheme = match cfg.kind {
        ProxyKind::Socks5 => "socks5h",
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
    fn url_for_builds_canonical_path() {
        let u = url_for("BeginAuthSessionViaCredentials").unwrap();
        assert_eq!(
            u.as_str(),
            "https://api.steampowered.com/IAuthenticationService/BeginAuthSessionViaCredentials/v1/"
        );
    }

    #[test]
    fn eresult_ok_is_one() {
        // Sanity-check the constant — keep this trivial test as a regression
        // guard in case someone "tidies up" the EResult constants.
        assert_eq!(EResult::OK, EResult(1));
    }

    #[test]
    fn ok_does_not_map_to_outcome() {
        // The mapping table is for non-OK only. OK is handled in-line by
        // each flow's success path.
        assert!(map_non_ok_eresult(EResult::OK, FlowKind::Password).is_none());
        assert!(map_non_ok_eresult(EResult::OK, FlowKind::RefreshToken).is_none());
    }

    #[test]
    fn password_flow_maps_invalid_password() {
        let o = map_non_ok_eresult(EResult::INVALID_PASSWORD, FlowKind::Password).unwrap();
        assert!(matches!(o, SignInOutcome::InvalidCredentials));
    }

    #[test]
    fn refresh_flow_maps_invalid_password_to_token_rejected() {
        let o = map_non_ok_eresult(EResult::INVALID_PASSWORD, FlowKind::RefreshToken).unwrap();
        assert!(matches!(o, SignInOutcome::TokenRejected));
    }

    #[test]
    fn refresh_flow_maps_expired_and_access_denied() {
        let a = map_non_ok_eresult(EResult::ACCESS_DENIED, FlowKind::RefreshToken).unwrap();
        let b = map_non_ok_eresult(EResult::EXPIRED, FlowKind::RefreshToken).unwrap();
        assert!(matches!(a, SignInOutcome::TokenRejected));
        assert!(matches!(b, SignInOutcome::TokenRejected));
    }

    #[test]
    fn account_logon_denied_maps_to_email_guard() {
        let o = map_non_ok_eresult(EResult::ACCOUNT_LOGON_DENIED, FlowKind::Password).unwrap();
        assert!(matches!(o, SignInOutcome::NeedsEmailGuardCode { .. }));
    }

    #[test]
    fn need_two_factor_and_mismatch_map_to_mobile_guard() {
        let a = map_non_ok_eresult(
            EResult::ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR,
            FlowKind::Password,
        )
        .unwrap();
        let b = map_non_ok_eresult(EResult::TWO_FACTOR_CODE_MISMATCH, FlowKind::Password).unwrap();
        assert!(matches!(a, SignInOutcome::NeedsMobileGuardCode));
        assert!(matches!(b, SignInOutcome::NeedsMobileGuardCode));
    }

    #[test]
    fn rate_limit_carries_a_hint() {
        let o = map_non_ok_eresult(EResult::RATE_LIMIT_EXCEEDED, FlowKind::Password).unwrap();
        match o {
            SignInOutcome::RateLimited { retry_hint } => assert!(retry_hint.is_some()),
            _ => panic!("expected RateLimited"),
        }
    }

    #[test]
    fn unknown_eresult_does_not_map() {
        // The caller is expected to convert this into Error::AuthRejected.
        assert!(map_non_ok_eresult(EResult(42), FlowKind::Password).is_none());
    }

    #[test]
    fn build_proxy_handles_socks5() {
        let cfg = ProxyConfig::parse("socks5://u:p@host:1080").unwrap();
        // Just assert it builds — reqwest::Proxy has no public fields to compare.
        build_proxy(&cfg).unwrap();
    }

    #[test]
    fn build_proxy_handles_http() {
        let cfg = ProxyConfig::parse("http://u:p@host:8080").unwrap();
        build_proxy(&cfg).unwrap();
    }

    #[test]
    fn build_proxy_rejects_https_to_proxy() {
        let cfg = ProxyConfig::parse("https://u:p@host:8443").unwrap();
        let err = build_proxy(&cfg).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }
}
