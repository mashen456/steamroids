//! High-level sign-in entry point.
//!
//! This is the public face of the auth flow — most consumers should not need
//! to touch `transport::*` or `proto::*` directly. Two modes:
//!
//! 1. **Password (+ optional Steam mobile 2FA shared secret).** First-time
//!    login or whenever the refresh token is missing / expired.
//! 2. **Refresh token.** Reuses a token previously returned by mode 1, skips
//!    the 2FA round-trip.
//!
//! Both modes accept an optional [`ProxyConfig`].
//!
//! # How it works
//!
//! Internally we drive Steam's `IAuthenticationService` `WebAPI`:
//!
//! - **Password flow:** `GetPasswordRSAPublicKey` → RSA-encrypt the password →
//!   `BeginAuthSessionViaCredentials` → (optional) `UpdateAuthSessionWithSteamGuardCode`
//!   with a TOTP code derived from the shared secret → poll
//!   `PollAuthSessionStatus` until we get a refresh token.
//! - **Refresh-token flow:** decode the `SteamID` from the token's JWT payload
//!   and call `GenerateAccessTokenForApp` to mint a fresh access token.
//!
//! No Connection Manager / `ClientLogon` WSS connection is established here —
//! that lands in `0.2.x` once we have the `EMsg` envelope codec.
//!
//! # Example
//!
//! ```no_run
//! use steamroids::auth::{SignIn, SignInOutcome};
//!
//! # async fn run() -> steamroids::Result<()> {
//! let outcome = SignIn::with_password("bot01", "hunter2")
//!     .shared_secret("base64SharedSecret==")
//!     .execute()
//!     .await?;
//!
//! if let SignInOutcome::Success { steam_id, .. } = outcome {
//!     println!("logged in as {steam_id}");
//! }
//! # Ok(()) }
//! ```

use std::fmt;
use std::time::Duration;

use tokio::time::sleep;
use tracing::debug;

use crate::auth::jwt::steam_id_from_refresh_token;
use crate::auth::rsa_pw::encrypt_password;
use crate::auth::webapi::{map_non_ok_eresult, EResult, FlowKind, HttpMethod, WebApiClient};
use crate::auth::{Credentials, RefreshToken};
use crate::proto::{
    CAuthenticationAccessTokenGenerateForAppRequest,
    CAuthenticationAccessTokenGenerateForAppResponse,
    CAuthenticationBeginAuthSessionViaCredentialsRequest,
    CAuthenticationBeginAuthSessionViaCredentialsResponse, CAuthenticationDeviceDetails,
    CAuthenticationGetPasswordRsaPublicKeyRequest, CAuthenticationGetPasswordRsaPublicKeyResponse,
    CAuthenticationPollAuthSessionStatusRequest, CAuthenticationPollAuthSessionStatusResponse,
    CAuthenticationUpdateAuthSessionWithSteamGuardCodeRequest,
    CAuthenticationUpdateAuthSessionWithSteamGuardCodeResponse,
};
use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

// EAuthSessionGuardType — values we care about on the response side.
const GUARD_TYPE_NONE: i32 = 1;
const GUARD_TYPE_EMAIL_CODE: i32 = 2;
const GUARD_TYPE_DEVICE_CODE: i32 = 3;

// EAuthTokenPlatformType_WebBrowser
const PLATFORM_WEB_BROWSER: i32 = 2;
// ESessionPersistence_Persistent
const SESSION_PERSISTENT: i32 = 1;
// ETokenRenewalType_None
const TOKEN_RENEWAL_NONE: i32 = 0;

// Polling defaults — Steam returns a per-session `interval` (seconds) but
// some responses leave it zero. Anchor on the bounds steam-user uses.
const POLL_DEFAULT_INTERVAL_SECS: u64 = 5;
const POLL_MAX_ATTEMPTS: usize = 24;

/// Outcome of a [`SignIn::execute`] call.
///
/// Distinguishes "the flow finished cleanly but the answer is no" from "the
/// flow itself broke". Use a `match` to handle every variant explicitly —
/// the enum is `#[non_exhaustive]` so additional cases can be added without
/// a breaking change (e.g. CAPTCHA, device-confirmation prompts).
///
/// `Debug` is implemented by hand: the `Success` variant carries the refresh
/// and access tokens, both of which are redacted so the outcome can be logged
/// safely.
#[derive(Clone)]
#[non_exhaustive]
pub enum SignInOutcome {
    /// Steam accepted the credentials and issued tokens.
    Success {
        /// 64-bit Steam ID of the authenticated account.
        steam_id: u64,
        /// Long-lived refresh token. Persist this and reuse via
        /// [`SignIn::with_refresh_token`] to skip 2FA on later logins.
        refresh_token: RefreshToken,
        /// Short-lived access token, when Steam returns one. Used for
        /// `WebAPI` calls; absent if only `ClientLogon` was performed.
        access_token: Option<String>,
    },
    /// 2FA required and no `shared_secret` was supplied. Caller must add one
    /// (mobile authenticator) or fall back to email-Guard handling.
    NeedsMobileGuardCode,
    /// Email-based Steam Guard is required. The two-letter domain is what
    /// Steam reveals about the recipient address.
    NeedsEmailGuardCode {
        /// Hint Steam returns about which mailbox to check (often `"…@gm…"`).
        email_domain: String,
    },
    /// Username/password rejected. Permanent — no point retrying with the
    /// same input.
    InvalidCredentials,
    /// Refresh token was rejected by Steam (expired or revoked). Re-run the
    /// password flow.
    TokenRejected,
    /// Steam threw a transient rate limit. Caller should back off.
    RateLimited {
        /// Steam's hint at when to retry, if it provided one.
        retry_hint: Option<std::time::Duration>,
    },
}

impl fmt::Debug for SignInOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success {
                steam_id,
                access_token,
                // `refresh_token`'s own Debug is already redacted, but we don't
                // even reach for it here — redact uniformly.
                refresh_token: _,
            } => f
                .debug_struct("Success")
                .field("steam_id", steam_id)
                .field("refresh_token", &"<redacted>")
                .field("access_token", &access_token.as_ref().map(|_| "<redacted>"))
                .finish(),
            Self::NeedsMobileGuardCode => f.write_str("NeedsMobileGuardCode"),
            Self::NeedsEmailGuardCode { email_domain } => f
                .debug_struct("NeedsEmailGuardCode")
                .field("email_domain", email_domain)
                .finish(),
            Self::InvalidCredentials => f.write_str("InvalidCredentials"),
            Self::TokenRejected => f.write_str("TokenRejected"),
            Self::RateLimited { retry_hint } => f
                .debug_struct("RateLimited")
                .field("retry_hint", retry_hint)
                .finish(),
        }
    }
}

/// Builder for a single sign-in attempt.
///
/// Construct via [`SignIn::with_password`] or [`SignIn::with_refresh_token`],
/// add optional config, then call [`SignIn::execute`].
#[derive(Debug, Clone)]
#[must_use = "SignIn does nothing until .execute() is awaited"]
pub struct SignIn {
    credentials: Credentials,
    proxy: Option<ProxyConfig>,
}

impl SignIn {
    /// Start a sign-in with username + password. Add a 2FA shared secret with
    /// [`Self::shared_secret`] if the account has the Steam mobile
    /// authenticator enabled.
    pub fn with_password(account: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::password(account, password, None),
            proxy: None,
        }
    }

    /// Start a sign-in with a previously-obtained refresh token. Skips 2FA.
    pub fn with_refresh_token(token: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::refresh_token(token),
            proxy: None,
        }
    }

    /// Attach the Steam mobile authenticator shared secret (base64).
    ///
    /// No-op for [`Self::with_refresh_token`] flows. We don't reject the
    /// configuration there because callers may build the request generically;
    /// the secret is simply unused.
    pub fn shared_secret(mut self, secret: impl Into<String>) -> Self {
        if let Credentials::Password(p) = &mut self.credentials {
            p.shared_secret = Some(secret.into());
        }
        self
    }

    /// Route the connection (and any HTTP-bearing handshake steps) through a
    /// SOCKS5 or HTTP-CONNECT proxy. Parse via
    /// [`ProxyConfig::parse`](crate::transport::proxy::ProxyConfig::parse).
    pub fn proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Drive the configured flow to completion (or to a definitive outcome).
    ///
    /// # Errors
    ///
    /// - Transport / proxy errors propagate as-is.
    /// - Steam-side rejections of the inputs come back as
    ///   [`SignInOutcome::InvalidCredentials`] / [`SignInOutcome::TokenRejected`]
    ///   — not as `Err`.
    /// - Unexpected non-OK `EResult` codes (anything not in the documented
    ///   mapping) come back as [`Error::AuthRejected`].
    pub async fn execute(self) -> Result<SignInOutcome> {
        match &self.credentials {
            Credentials::Password(_) => self.execute_password_flow().await,
            Credentials::RefreshToken(_) => self.execute_refresh_token_flow().await,
        }
    }

    async fn execute_password_flow(self) -> Result<SignInOutcome> {
        let Credentials::Password(creds) = &self.credentials else {
            unreachable!("dispatched by execute()");
        };

        let client = WebApiClient::new(self.proxy.as_ref())?;

        // 1. Fetch the RSA public key + RSA-encrypt the password.
        let RsaKeyResponse {
            mod_hex,
            exp_hex,
            timestamp,
        } = match fetch_rsa_key(&client, &creds.account_name).await? {
            EarlyExit::Continue(v) => v,
            EarlyExit::Outcome(o) => return Ok(o),
        };
        let encrypted_b64 = encrypt_password(&creds.password, &mod_hex, &exp_hex)?;

        // 2. Begin the auth session.
        let begin =
            match begin_session(&client, &creds.account_name, encrypted_b64, timestamp).await? {
                EarlyExit::Continue(v) => v,
                EarlyExit::Outcome(o) => return Ok(o),
            };

        // 3. Handle whichever Steam Guard variant Steam asked for.
        match resolve_guard(&client, &begin, creds.shared_secret.as_deref()).await? {
            EarlyExit::Continue(()) => {}
            EarlyExit::Outcome(o) => return Ok(o),
        }

        // 4. Poll until Steam emits a refresh token.
        poll_for_token(&client, &begin).await
    }

    async fn execute_refresh_token_flow(self) -> Result<SignInOutcome> {
        let Credentials::RefreshToken(token) = &self.credentials else {
            unreachable!("dispatched by execute()");
        };

        let steam_id = steam_id_from_refresh_token(token.expose())?;
        let client = WebApiClient::new(self.proxy.as_ref())?;

        let req = CAuthenticationAccessTokenGenerateForAppRequest {
            refresh_token: Some(token.expose().to_string()),
            steamid: Some(steam_id),
            renewal_type: Some(TOKEN_RENEWAL_NONE),
        };
        let (er, resp): (_, CAuthenticationAccessTokenGenerateForAppResponse) = client
            .call("GenerateAccessTokenForApp", HttpMethod::Post, &req)
            .await?;
        if er != EResult::OK {
            if let Some(outcome) = map_non_ok_eresult(er, FlowKind::RefreshToken) {
                return Ok(outcome);
            }
            return Err(Error::AuthRejected(format!(
                "GenerateAccessTokenForApp eresult {}",
                er.0
            )));
        }

        let access_token = resp.access_token.filter(|s| !s.is_empty());
        // With `renewal_type = TOKEN_RENEWAL_NONE` Steam does not rotate the
        // refresh token, but if a server-side policy ever flips it on, persist
        // whatever Steam returned in place of the old one. Falling back to the
        // input keeps the caller's persisted state consistent on no-rotation.
        let refresh_token = resp
            .refresh_token
            .filter(|s| !s.is_empty())
            .map_or_else(|| token.clone(), RefreshToken::new);
        Ok(SignInOutcome::Success {
            steam_id,
            refresh_token,
            access_token,
        })
    }
}

/// Either continue the flow with `T`, or short-circuit out with a final
/// [`SignInOutcome`] (e.g. when Steam reports `InvalidPassword` mid-flow).
enum EarlyExit<T> {
    Continue(T),
    Outcome(SignInOutcome),
}

/// Lower-half of the RSA public-key response we actually use.
struct RsaKeyResponse {
    mod_hex: String,
    exp_hex: String,
    timestamp: u64,
}

/// State we need from the `BeginAuthSessionViaCredentials` response.
struct BeginSession {
    client_id: u64,
    request_id: Vec<u8>,
    session_steamid: u64,
    poll_interval: Duration,
    guards: Vec<i32>,
    /// Steam's `associated_message` hint on the email-code allowed-confirmation
    /// entry, when present. Carries the masked domain (e.g. `"gm…"`).
    email_domain_hint: Option<String>,
}

async fn fetch_rsa_key(
    client: &WebApiClient,
    account_name: &str,
) -> Result<EarlyExit<RsaKeyResponse>> {
    let req = CAuthenticationGetPasswordRsaPublicKeyRequest {
        account_name: Some(account_name.to_string()),
    };
    let (er, resp): (_, CAuthenticationGetPasswordRsaPublicKeyResponse) = client
        .call("GetPasswordRSAPublicKey", HttpMethod::Get, &req)
        .await?;
    if er != EResult::OK {
        if let Some(outcome) = map_non_ok_eresult(er, FlowKind::Password) {
            return Ok(EarlyExit::Outcome(outcome));
        }
        return Err(Error::AuthRejected(format!(
            "GetPasswordRSAPublicKey eresult {}",
            er.0
        )));
    }

    let mod_hex = resp
        .publickey_mod
        .ok_or_else(|| Error::AuthRejected("rsa response: missing modulus".into()))?;
    let exp_hex = resp
        .publickey_exp
        .ok_or_else(|| Error::AuthRejected("rsa response: missing exponent".into()))?;
    let timestamp = resp
        .timestamp
        .ok_or_else(|| Error::AuthRejected("rsa response: missing timestamp".into()))?;
    Ok(EarlyExit::Continue(RsaKeyResponse {
        mod_hex,
        exp_hex,
        timestamp,
    }))
}

async fn begin_session(
    client: &WebApiClient,
    account_name: &str,
    encrypted_password_b64: String,
    encryption_timestamp: u64,
) -> Result<EarlyExit<BeginSession>> {
    let req = CAuthenticationBeginAuthSessionViaCredentialsRequest {
        device_friendly_name: Some("steamroids".into()),
        account_name: Some(account_name.to_string()),
        encrypted_password: Some(encrypted_password_b64),
        encryption_timestamp: Some(encryption_timestamp),
        remember_login: Some(true),
        platform_type: Some(PLATFORM_WEB_BROWSER),
        persistence: Some(SESSION_PERSISTENT),
        website_id: Some("Community".into()),
        device_details: Some(CAuthenticationDeviceDetails {
            device_friendly_name: Some("steamroids".into()),
            platform_type: Some(PLATFORM_WEB_BROWSER),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (er, resp): (_, CAuthenticationBeginAuthSessionViaCredentialsResponse) = client
        .call("BeginAuthSessionViaCredentials", HttpMethod::Post, &req)
        .await?;
    if er != EResult::OK {
        if let Some(outcome) = map_non_ok_eresult(er, FlowKind::Password) {
            return Ok(EarlyExit::Outcome(outcome));
        }
        return Err(Error::AuthRejected(format!(
            "BeginAuthSessionViaCredentials eresult {}",
            er.0
        )));
    }

    let client_id = resp
        .client_id
        .ok_or_else(|| Error::AuthRejected("BeginAuth response: missing client_id".into()))?;
    let request_id = resp
        .request_id
        .ok_or_else(|| Error::AuthRejected("BeginAuth response: missing request_id".into()))?;
    let session_steamid = resp
        .steamid
        .ok_or_else(|| Error::AuthRejected("BeginAuth response: missing steamid".into()))?;
    let poll_interval = compute_poll_interval(resp.interval);
    let mut guards: Vec<i32> = Vec::with_capacity(resp.allowed_confirmations.len());
    let mut email_domain_hint: Option<String> = None;
    for c in &resp.allowed_confirmations {
        let Some(ct) = c.confirmation_type else {
            continue;
        };
        guards.push(ct);
        // Steam stuffs the email-domain hint into `associated_message` on the
        // email-code entry. We carry it through so the user sees "…@gm…".
        if ct == GUARD_TYPE_EMAIL_CODE && email_domain_hint.is_none() {
            email_domain_hint = c.associated_message.clone().filter(|s| !s.is_empty());
        }
    }
    debug!(
        target: "steamroids::auth::signin",
        ?guards,
        client_id,
        "BeginAuthSessionViaCredentials accepted"
    );
    Ok(EarlyExit::Continue(BeginSession {
        client_id,
        request_id,
        session_steamid,
        poll_interval,
        guards,
        email_domain_hint,
    }))
}

async fn resolve_guard(
    client: &WebApiClient,
    begin: &BeginSession,
    shared_secret: Option<&str>,
) -> Result<EarlyExit<()>> {
    if begin.guards.contains(&GUARD_TYPE_DEVICE_CODE) {
        let Some(secret) = shared_secret else {
            return Ok(EarlyExit::Outcome(SignInOutcome::NeedsMobileGuardCode));
        };
        let code = crate::auth::totp::generate_auth_code(secret, None)?;
        let update_req = CAuthenticationUpdateAuthSessionWithSteamGuardCodeRequest {
            client_id: Some(begin.client_id),
            steamid: Some(begin.session_steamid),
            code: Some(code),
            code_type: Some(GUARD_TYPE_DEVICE_CODE),
        };
        let (er, _resp): (
            _,
            CAuthenticationUpdateAuthSessionWithSteamGuardCodeResponse,
        ) = client
            .call(
                "UpdateAuthSessionWithSteamGuardCode",
                HttpMethod::Post,
                &update_req,
            )
            .await?;
        if er != EResult::OK {
            if let Some(outcome) = map_non_ok_eresult(er, FlowKind::Password) {
                return Ok(EarlyExit::Outcome(outcome));
            }
            return Err(Error::AuthRejected(format!(
                "UpdateAuthSessionWithSteamGuardCode eresult {}",
                er.0
            )));
        }
        Ok(EarlyExit::Continue(()))
    } else if begin.guards.contains(&GUARD_TYPE_EMAIL_CODE) {
        Ok(EarlyExit::Outcome(SignInOutcome::NeedsEmailGuardCode {
            email_domain: begin.email_domain_hint.clone().unwrap_or_default(),
        }))
    } else if !begin.guards.is_empty() && !begin.guards.contains(&GUARD_TYPE_NONE) {
        // Device confirmation, machine token, etc. — not in scope.
        Err(Error::AuthRejected(format!(
            "unsupported confirmation types: {:?}",
            begin.guards
        )))
    } else {
        Ok(EarlyExit::Continue(()))
    }
}

async fn poll_for_token(client: &WebApiClient, begin: &BeginSession) -> Result<SignInOutcome> {
    let req = CAuthenticationPollAuthSessionStatusRequest {
        client_id: Some(begin.client_id),
        request_id: Some(begin.request_id.clone()),
        ..Default::default()
    };
    for attempt in 0..POLL_MAX_ATTEMPTS {
        // Poll immediately on the first try; sleep between retries only.
        // Saves a guaranteed `poll_interval` of latency on every successful
        // login when Steam already has our token ready.
        if attempt > 0 {
            sleep(begin.poll_interval).await;
        }

        let (er, resp): (_, CAuthenticationPollAuthSessionStatusResponse) = client
            .call("PollAuthSessionStatus", HttpMethod::Post, &req)
            .await?;
        if er != EResult::OK {
            if let Some(outcome) = map_non_ok_eresult(er, FlowKind::Password) {
                return Ok(outcome);
            }
            return Err(Error::AuthRejected(format!(
                "PollAuthSessionStatus eresult {}",
                er.0
            )));
        }

        if let Some(rt) = resp.refresh_token.filter(|s| !s.is_empty()) {
            let steam_id = steam_id_from_refresh_token(&rt).unwrap_or(begin.session_steamid);
            let access_token = resp.access_token.filter(|s| !s.is_empty());
            return Ok(SignInOutcome::Success {
                steam_id,
                refresh_token: RefreshToken::new(rt),
                access_token,
            });
        }
    }

    Err(Error::Timeout("auth poll exceeded 120s"))
}

/// Translate Steam's poll-interval hint (seconds, as `f32`) into a real
/// [`Duration`], clamping nonsense values.
fn compute_poll_interval(steam_interval: Option<f32>) -> Duration {
    let Some(raw) = steam_interval.filter(|i| i.is_finite() && *i > 0.0) else {
        return Duration::from_secs(POLL_DEFAULT_INTERVAL_SECS);
    };
    // Clamp to a sane upper bound — Steam shouldn't ever ask for >60s but
    // we don't want a malicious or buggy response to wedge our poll loop.
    // Lower bound of 1s defends against busy-looping if Steam sends a
    // sub-second hint.
    let clamped = raw.clamp(1.0, 60.0).round();
    // `clamp(1.0, 60.0)` keeps the value finite and in [1, 60], so the
    // cast is lossless.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let secs = clamped as u64;
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_builder_round_trips_config() {
        let s = SignIn::with_password("acc", "pw").shared_secret("base64==");
        if let Credentials::Password(p) = &s.credentials {
            assert_eq!(p.account_name, "acc");
            assert_eq!(p.password, "pw");
            assert_eq!(p.shared_secret.as_deref(), Some("base64=="));
        } else {
            panic!("expected Password credentials");
        }
    }

    #[test]
    fn shared_secret_is_ignored_for_refresh_token_flow() {
        let s = SignIn::with_refresh_token("rt").shared_secret("ignored");
        assert!(matches!(s.credentials, Credentials::RefreshToken(_)));
    }

    #[tokio::test]
    async fn refresh_token_with_malformed_jwt_errors_immediately() {
        // The refresh-token flow validates the JWT shape before doing any
        // network work, so this is safe to run with no network access.
        let err = SignIn::with_refresh_token("not.a.real.jwt")
            .execute()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn poll_interval_falls_back_to_default() {
        assert_eq!(
            compute_poll_interval(None),
            Duration::from_secs(POLL_DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            compute_poll_interval(Some(0.0)),
            Duration::from_secs(POLL_DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            compute_poll_interval(Some(f32::NAN)),
            Duration::from_secs(POLL_DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn poll_interval_passes_through_reasonable_value() {
        assert_eq!(compute_poll_interval(Some(3.0)), Duration::from_secs(3));
    }

    #[test]
    fn poll_interval_clamps_runaway_values() {
        // Defend against a buggy / hostile response trying to make us sleep
        // for hours between polls.
        assert_eq!(compute_poll_interval(Some(9000.0)), Duration::from_secs(60));
    }

    #[test]
    fn success_outcome_debug_redacts_tokens() {
        let outcome = SignInOutcome::Success {
            steam_id: 76_561_198_000_000_001,
            refresh_token: RefreshToken::new("secret-refresh-jwt"),
            access_token: Some("secret-access-jwt".into()),
        };
        let dbg = format!("{outcome:?}");
        assert!(!dbg.contains("secret-refresh-jwt"), "refresh token leaked: {dbg}");
        assert!(!dbg.contains("secret-access-jwt"), "access token leaked: {dbg}");
        // Non-secret context (the SteamID) and token presence stay visible.
        assert!(dbg.contains("76561198000000001"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn signin_builder_debug_redacts_password() {
        // SignIn derives Debug; it must not leak via its embedded credentials.
        let dbg = format!("{:?}", SignIn::with_password("bot01", "leak-me"));
        assert!(!dbg.contains("leak-me"), "password leaked through SignIn: {dbg}");
    }
}
