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
use reqwest::Client;
use tracing::debug;
use url::Url;

use crate::auth::signin::SignInOutcome;
use crate::error::eresult;
use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

const BASE_URL: &str = "https://api.steampowered.com/IAuthenticationService/";

/// Backoff we suggest on a rate-limited / throttled sign-in. Steam's auth
/// responses carry no retry-after field, so this is our own fixed default.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

/// Steam's `EResult` enum value, lifted from the `x-eresult` header.
///
/// Wrapped so calling code is forced to acknowledge the integer is a Steam
/// result code and not a generic HTTP status. The values come from
/// [`crate::error::eresult`], the crate's single `EResult` table; only the
/// codes this flow branches on are named here, and everything else is treated
/// as "unknown failure".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EResult(pub i32);

impl EResult {
    pub(crate) const OK: Self = Self(eresult::OK);
    pub(crate) const INVALID_PASSWORD: Self = Self(eresult::INVALID_PASSWORD);
    pub(crate) const ACCOUNT_LOGON_DENIED: Self = Self(eresult::ACCOUNT_LOGON_DENIED);
    pub(crate) const RATE_LIMIT_EXCEEDED: Self = Self(eresult::RATE_LIMIT_EXCEEDED);
    pub(crate) const ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR: Self =
        Self(eresult::ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR);
    pub(crate) const ACCOUNT_LOGIN_DENIED_THROTTLE: Self =
        Self(eresult::ACCOUNT_LOGIN_DENIED_THROTTLE);
    pub(crate) const TWO_FACTOR_CODE_MISMATCH: Self = Self(eresult::TWO_FACTOR_CODE_MISMATCH);
}

/// HTTP verb to use for a call.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HttpMethod {
    /// GET with the protobuf as `?input_protobuf_encoded=...` in the query.
    Get,
    /// POST with the protobuf in the form-urlencoded body.
    Post,
}

/// Translate a non-OK [`EResult`] into a [`SignInOutcome`] when the mapping is
/// well-defined, or `None` when the caller should raise an error instead.
///
/// This is used by the **password** sign-in flow (and its Steam Guard / poll
/// steps). The refresh-token flow no longer hits the `WebAPI` — `SteamClient`
/// tokens are validated locally and redeemed over the CM — so its rejections
/// don't pass through here.
pub(crate) fn map_non_ok_eresult(er: EResult) -> Option<SignInOutcome> {
    match er {
        EResult::INVALID_PASSWORD => Some(SignInOutcome::InvalidCredentials),
        EResult::ACCOUNT_LOGON_DENIED => Some(SignInOutcome::NeedsEmailGuardCode {
            email_domain: String::new(),
        }),
        EResult::ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR => Some(SignInOutcome::NeedsMobileGuardCode),
        // Distinct from "we never sent one": Steam saw a code and refused it.
        EResult::TWO_FACTOR_CODE_MISMATCH => Some(SignInOutcome::GuardCodeRejected),
        EResult::RATE_LIMIT_EXCEEDED | EResult::ACCOUNT_LOGIN_DENIED_THROTTLE => {
            Some(SignInOutcome::RateLimited {
                retry_hint: Some(RATE_LIMIT_BACKOFF),
            })
        }
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
        Ok(Self {
            http: crate::http::client(proxy)?,
        })
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
            .map_err(|e| Error::Network(format!("{method}: {e}")))?;

        // x-eresult is documented as always present on auth-service responses;
        // its absence almost certainly means we hit a reverse-proxy / captive
        // portal instead of Steam, so surface that loudly instead of
        // pretending we got `EResult(0)`.
        let er_header = response.headers().get("x-eresult").ok_or_else(|| {
            Error::AuthRejected(format!("{method}: response missing x-eresult header"))
        })?;
        let er_value: i32 = er_header
            .to_str()
            .map_err(|_| Error::AuthRejected(format!("{method}: x-eresult not ascii")))?
            .parse()
            .map_err(|_| Error::AuthRejected(format!("{method}: x-eresult not an integer")))?;
        let er = EResult(er_value);

        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Network(format!("{method} read body: {e}")))?;

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
        assert!(map_non_ok_eresult(EResult::OK).is_none());
    }

    #[test]
    fn maps_invalid_password() {
        let o = map_non_ok_eresult(EResult::INVALID_PASSWORD).unwrap();
        assert!(matches!(o, SignInOutcome::InvalidCredentials));
    }

    #[test]
    fn account_logon_denied_maps_to_email_guard() {
        let o = map_non_ok_eresult(EResult::ACCOUNT_LOGON_DENIED).unwrap();
        assert!(matches!(o, SignInOutcome::NeedsEmailGuardCode { .. }));
    }

    #[test]
    fn need_two_factor_maps_to_mobile_guard() {
        let o = map_non_ok_eresult(EResult::ACCOUNT_LOGIN_DENIED_NEED_TWO_FACTOR).unwrap();
        assert!(matches!(o, SignInOutcome::NeedsMobileGuardCode));
    }

    #[test]
    fn code_mismatch_is_not_a_missing_secret() {
        // 88 means Steam saw our TOTP and refused it; reporting it as
        // "supply a shared secret" sends the caller down the wrong path.
        let o = map_non_ok_eresult(EResult::TWO_FACTOR_CODE_MISMATCH).unwrap();
        assert!(matches!(o, SignInOutcome::GuardCodeRejected));
    }

    #[test]
    fn rate_limit_carries_a_hint() {
        let o = map_non_ok_eresult(EResult::RATE_LIMIT_EXCEEDED).unwrap();
        match o {
            SignInOutcome::RateLimited { retry_hint } => {
                assert_eq!(retry_hint, Some(RATE_LIMIT_BACKOFF));
            }
            _ => panic!("expected RateLimited"),
        }
    }

    #[test]
    fn login_denied_throttle_is_rate_limited_not_an_error() {
        // 87 is transient; leaving it unmapped turned a throttle into a hard
        // Error::AuthRejected.
        let o = map_non_ok_eresult(EResult::ACCOUNT_LOGIN_DENIED_THROTTLE).unwrap();
        assert!(matches!(o, SignInOutcome::RateLimited { .. }));
    }

    #[test]
    fn unknown_eresult_does_not_map() {
        // 42 is NoMatch, a code this flow never branches on. The caller is
        // expected to convert it into Error::AuthRejected.
        assert!(map_non_ok_eresult(EResult(42)).is_none());
    }
}
