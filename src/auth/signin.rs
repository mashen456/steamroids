//! High-level sign-in entry point.
//!
//! This is the public face of the auth flow — most consumers should not need
//! to touch `transport::*` or `proto::*` directly. Three modes:
//!
//! 1. **Password (+ optional Steam mobile 2FA shared secret).** First-time
//!    login or whenever the refresh token is missing / expired.
//! 2. **Refresh token.** Reuses a token previously returned by mode 1, skips
//!    the 2FA round-trip.
//! 3. **QR code.** No password needed at all: a human scans a URL with the
//!    Steam mobile app. Two-phase, since there's a human in the loop; see
//!    [`SignIn::with_qr`].
//!
//! All three modes accept an optional [`ProxyConfig`].
//!
//! # How it works
//!
//! Internally we drive Steam's `IAuthenticationService` `WebAPI`:
//!
//! - **Password flow:** `GetPasswordRSAPublicKey` → RSA-encrypt the password →
//!   `BeginAuthSessionViaCredentials` → (optional) `UpdateAuthSessionWithSteamGuardCode`
//!   with a TOTP code derived from the shared secret → poll
//!   `PollAuthSessionStatus` until we get a refresh token.
//! - **Refresh-token flow:** decode and validate the token's JWT (`SteamID`,
//!   expiry) locally and hand it back for a CM `ClientLogon`. `SteamClient`
//!   tokens can't be redeemed over the `WebAPI`, so no access token is minted.
//! - **QR flow:** `BeginAuthSessionViaQR` returns a `challenge_url` to display
//!   → poll `PollAuthSessionStatus` (same poll loop as the password flow)
//!   until a human approves it in the app.
//!
//! # Limitation: `QrSession::challenge_url` does not rotate
//!
//! Steam's QR response carries a `version` field and `PollAuthSessionStatus`
//! can hand back a `new_challenge_url` mid-session: the URL a caller
//! displayed can go stale before it's scanned. This version does not track
//! either: [`QrSession::challenge_url`] returns the one URL from the initial
//! `BeginAuthSessionViaQR` response for the whole session. If Steam rotates
//! it before a human scans, the poll just runs out its budget and returns
//! [`Error::Timeout`] instead of surfacing the new URL. Not implemented here;
//! a future version may plumb it through.
//!
//! # Limitation: the refresh-token flow is offline-only
//!
//! Nothing in this module opens a Connection Manager socket, so **no sign-in
//! here proves a refresh token is still live**. The refresh flow checks the
//! JWT's shape and its `exp` claim and nothing else. A token that Steam has
//! *revoked* (password change, "deauthorise all devices", a session the user
//! killed from the mobile app) is indistinguishable from a good one until it is
//! actually presented to a CM.
//!
//! In practice that means:
//!
//! - [`SignInOutcome::TokenRejected`] is only ever returned for a token this
//!   crate could reject locally, i.e. an expired one.
//! - A revoked-but-unexpired token yields [`SignInOutcome::Success`], and the
//!   rejection surfaces later from
//!   [`spawn_session`](crate::session::spawn_session) as
//!   [`Error::AuthRejected`] (as opposed to [`Error::LogonRetryable`], which
//!   means the CM was merely busy and the token is fine).
//! - So a fleet that persists tokens must treat an `AuthRejected` out of
//!   `spawn_session` as "discard the stored token and re-run the password
//!   flow". [`SignIn::execute_with_store`] cannot do that for you: by the time
//!   the CM says no, `execute_with_store` has already returned.
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
//!
//! QR sign-in, two phases:
//!
//! ```no_run
//! use steamroids::auth::{SignIn, SignInOutcome};
//!
//! # async fn run() -> steamroids::Result<()> {
//! let qr = SignIn::with_qr().begin().await?;
//! println!("scan with the Steam mobile app: {}", qr.challenge_url());
//!
//! if let SignInOutcome::Success { steam_id, .. } = qr.poll().await? {
//!     println!("logged in as {steam_id}");
//! }
//! # Ok(()) }
//! ```

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::debug;

use crate::auth::jwt::{refresh_token_claims, steam_id_from_refresh_token};
use crate::auth::rsa_pw::encrypt_password;
use crate::auth::token_store::TokenStore;
use crate::auth::webapi::{map_non_ok_eresult, EResult, HttpMethod, WebApiClient};
use crate::auth::{Credentials, RefreshToken};
use crate::proto::{
    CAuthenticationBeginAuthSessionViaCredentialsRequest,
    CAuthenticationBeginAuthSessionViaCredentialsResponse,
    CAuthenticationBeginAuthSessionViaQrRequest, CAuthenticationBeginAuthSessionViaQrResponse,
    CAuthenticationDeviceDetails, CAuthenticationGetPasswordRsaPublicKeyRequest,
    CAuthenticationGetPasswordRsaPublicKeyResponse, CAuthenticationPollAuthSessionStatusRequest,
    CAuthenticationPollAuthSessionStatusResponse,
    CAuthenticationUpdateAuthSessionWithSteamGuardCodeRequest,
    CAuthenticationUpdateAuthSessionWithSteamGuardCodeResponse,
};
use crate::ratelimit::RateLimiter;
use crate::transport::proxy::ProxyConfig;
use crate::{Error, Result};

// EAuthSessionGuardType — values we care about on the response side.
const GUARD_TYPE_NONE: i32 = 1;
const GUARD_TYPE_EMAIL_CODE: i32 = 2;
const GUARD_TYPE_DEVICE_CODE: i32 = 3;

// EAuthTokenPlatformType_SteamClient. The refresh token must be issued for the
// Steam *client* platform, otherwise the CM `ClientLogon` rejects it with
// eresult 5 (a WebBrowser token only works for website / WebAPI use).
const PLATFORM_STEAM_CLIENT: i32 = 1;
// EOSType for Windows 10 — sent in the device details for the client session.
const OS_TYPE_WINDOWS_10: i32 = 16;
// ESessionPersistence_Persistent
const SESSION_PERSISTENT: i32 = 1;
// Seconds of slack so a token about to expire isn't treated as still valid.
const TOKEN_EXPIRY_SLACK_SECS: u64 = 60;

// Polling defaults — Steam returns a per-session `interval` (seconds) but
// some responses leave it zero. Anchor on the bounds steam-user uses.
const POLL_DEFAULT_INTERVAL_SECS: u64 = 5;
// Wall-clock budget for the whole poll phase. Bounding attempts instead would
// scale with whatever interval Steam hands out, so the timeout message could
// name a duration that never elapsed.
const POLL_BUDGET: Duration = Duration::from_secs(120);

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
    /// Steam saw the Steam Guard code we submitted and refused it
    /// (`EResult::TwoFactorCodeMismatch`). The usual causes are a wrong
    /// `shared_secret` or a host clock more than a time-step out of sync,
    /// not a missing secret, so re-running with the same inputs won't help.
    GuardCodeRejected,
    /// Email-based Steam Guard is required. The two-letter domain is what
    /// Steam reveals about the recipient address.
    NeedsEmailGuardCode {
        /// Hint Steam returns about which mailbox to check (often `"…@gm…"`).
        email_domain: String,
    },
    /// Username/password rejected. Permanent — no point retrying with the
    /// same input.
    InvalidCredentials,
    /// The refresh token is unusable and the password flow should be re-run.
    ///
    /// Only reachable from checks this crate can make without a CM round-trip:
    /// the token is expired, or it doesn't decode as a JWT at all. Steam-side
    /// revocation is **not** detected here; see the [module docs](self#limitation-the-refresh-token-flow-is-offline-only).
    TokenRejected,
    /// Steam threw a transient rate limit or login throttle. Caller should
    /// back off.
    RateLimited {
        /// Suggested backoff before retrying. Steam's auth responses carry no
        /// retry-after field, so this is this crate's own fixed default rather
        /// than anything Steam told us. `None` means "back off, we have no
        /// suggestion".
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
            Self::GuardCodeRejected => f.write_str("GuardCodeRejected"),
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
///
/// `Debug` is implemented by hand: `credentials` already redacts secrets, and
/// `rate_limiter` is shown only as a presence marker rather than trying to
/// format the limiter itself.
#[derive(Clone)]
#[must_use = "SignIn does nothing until .execute() is awaited"]
pub struct SignIn {
    credentials: Credentials,
    proxy: Option<ProxyConfig>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl fmt::Debug for SignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignIn")
            .field("credentials", &self.credentials)
            .field("proxy", &self.proxy)
            .field("rate_limiter", &self.rate_limiter.is_some())
            .finish()
    }
}

impl SignIn {
    /// Start a sign-in with username + password. Add a 2FA shared secret with
    /// [`Self::shared_secret`] if the account has the Steam mobile
    /// authenticator enabled.
    pub fn with_password(account: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::password(account, password, None),
            proxy: None,
            rate_limiter: None,
        }
    }

    /// Start a sign-in with a previously-obtained refresh token. Skips 2FA.
    ///
    /// `execute` validates the token **locally only** (decodes the JWT, checks
    /// it isn't expired) and returns it for use with
    /// [`spawn_session`](crate::session::spawn_session), which logs on over the
    /// CM. `Success` here therefore means "this token is well-formed and not
    /// yet expired", not "Steam still honours it"; see the
    /// [module docs](self#limitation-the-refresh-token-flow-is-offline-only).
    ///
    /// It does **not** mint a web access token: Steam only redeems these
    /// `SteamClient` tokens over an authenticated CM session, not the `WebAPI`,
    /// so [`SignInOutcome::Success::access_token`] is `None` for this flow.
    pub fn with_refresh_token(token: impl Into<String>) -> Self {
        Self {
            credentials: Credentials::refresh_token(token),
            proxy: None,
            rate_limiter: None,
        }
    }

    /// Start a QR-code sign-in. No password, no shared secret; a human
    /// scans a URL with the Steam mobile app and approves the login there.
    ///
    /// Returns a separate builder, [`QrSignIn`], rather than `Self`: unlike
    /// [`Self::with_password`] / [`Self::with_refresh_token`], this flow can't
    /// be driven to completion in one `execute()` call. There's a human step
    /// in between opening the session and knowing the outcome, so it's two
    /// calls: [`QrSignIn::begin`] to fetch the URL to display, then
    /// [`QrSession::poll`] to await the human. See the [module docs](self)
    /// for a full example.
    pub fn with_qr() -> QrSignIn {
        QrSignIn {
            proxy: None,
            rate_limiter: None,
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
    ///
    /// No-op for [`Self::with_refresh_token`] flows: that flow validates the
    /// token offline and opens no socket, so there is nothing to route. As with
    /// [`Self::shared_secret`] the configuration is accepted rather than
    /// rejected, since [`Self::execute_with_store`] still needs it for the
    /// password-flow fallback.
    pub fn proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Pace this sign-in's `WebAPI` requests through a shared limiter.
    ///
    /// Every poll of `PollAuthSessionStatus` acquires a slot too, and the
    /// whole poll phase is itself bounded by a 120s wall-clock budget. A
    /// limiter interval that is coarse relative to that budget (tens of
    /// seconds, shared across a fleet behind one exit) can eat enough of it
    /// that a healthy login times out instead of completing, so pick an
    /// interval with that budget in mind, not just the steady-state request
    /// rate you want.
    pub fn rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
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
            Credentials::RefreshToken(_) => self.execute_refresh_token_flow(),
        }
    }

    /// Execute while transparently reusing and persisting a refresh token via
    /// `store`.
    ///
    /// This is the ergonomic entry point for long-running fleets: it avoids a
    /// full password+2FA login (which Steam rate-limits) whenever a still-valid
    /// refresh token is on hand. The flow for a password builder:
    ///
    /// 1. [`TokenStore::load`] the account's saved token. If present, try the
    ///    refresh flow with it.
    /// 2. On success, [`TokenStore::save`] whatever token came back (Steam may
    ///    rotate it) and return.
    /// 3. If no token existed, or the stored one was unusable (expired, or not
    ///    a decodable JWT, from a truncated or corrupted store entry), run the
    ///    full password flow, then save the freshly issued token. A bad store
    ///    entry is never fatal: it is treated as "no token".
    ///
    /// A [`SignIn::with_refresh_token`] builder has no account key to persist
    /// under, so it bypasses the store and behaves like [`Self::execute`].
    ///
    /// Note the fallback can only fire on failures this crate detects offline.
    /// A stored token Steam has *revoked* still looks valid here and is
    /// returned as `Success`; see the
    /// [module docs](self#limitation-the-refresh-token-flow-is-offline-only)
    /// for what the caller has to do about that.
    ///
    /// # Errors
    ///
    /// Same as [`Self::execute`], plus [`Error::TokenStore`] if the store's
    /// `load`/`save` fails.
    pub async fn execute_with_store(
        self,
        store: &(impl TokenStore + Sync),
    ) -> Result<SignInOutcome> {
        // Only a password builder has an account name to key the store on.
        let account = match &self.credentials {
            Credentials::Password(c) => c.account_name.clone(),
            Credentials::RefreshToken(_) => return self.execute().await,
        };
        let proxy = self.proxy.clone();

        // 1. Reuse a stored token if there is one.
        if let Some(token) = store
            .load(&account)
            .await
            .map_err(|e| Error::TokenStore(e.to_string()))?
        {
            let mut attempt = SignIn::with_refresh_token(token);
            if let Some(p) = proxy {
                attempt = attempt.proxy(p);
            }
            // A stored token that won't even decode is a bad store entry, not
            // a caller input error: treat it as stale and re-auth.
            let outcome = match attempt.execute().await {
                Ok(o) => o,
                Err(Error::AuthRejected(reason)) => {
                    debug!(
                        target: "steamroids::auth::signin",
                        %reason,
                        "stored refresh token undecodable, falling back to password flow"
                    );
                    SignInOutcome::TokenRejected
                }
                Err(e) => return Err(e),
            };
            match outcome {
                SignInOutcome::Success {
                    steam_id,
                    refresh_token,
                    access_token,
                } => {
                    store
                        .save(&account, refresh_token.expose())
                        .await
                        .map_err(|e| Error::TokenStore(e.to_string()))?;
                    return Ok(SignInOutcome::Success {
                        steam_id,
                        refresh_token,
                        access_token,
                    });
                }
                // Stored token is no good — fall through to the password flow.
                SignInOutcome::TokenRejected => {}
                // Anything transient (rate limit, …) is the caller's to handle.
                other => return Ok(other),
            }
        }

        // 2. Full password flow; persist the new token on success.
        let outcome = self.execute().await?;
        if let SignInOutcome::Success { refresh_token, .. } = &outcome {
            store
                .save(&account, refresh_token.expose())
                .await
                .map_err(|e| Error::TokenStore(e.to_string()))?;
        }
        Ok(outcome)
    }

    async fn execute_password_flow(self) -> Result<SignInOutcome> {
        let Credentials::Password(creds) = &self.credentials else {
            unreachable!("dispatched by execute()");
        };

        let client = WebApiClient::new(self.proxy.as_ref(), self.rate_limiter.clone())?;

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

    /// Validate a refresh token locally and hand it back for a CM logon.
    ///
    /// This library issues **`SteamClient`-platform** refresh tokens (see the
    /// `with_password` flow). Steam will **not** redeem those over the plain
    /// `WebAPI` — `GenerateAccessTokenForApp` answers `AccessDenied`; that
    /// exchange has to ride an *authenticated* CM session. The token is instead
    /// meant to be used directly in `CMsgClientLogon`, which
    /// [`spawn_session`](crate::session::spawn_session) does. So here we only
    /// decode and sanity-check the JWT (Steam ID present, not expired) and
    /// return the token unchanged. No web access token is minted — `Success`
    /// carries `access_token: None`.
    ///
    /// The corollary is that revocation is invisible to this function: only
    /// the CM knows whether Steam still honours the token. `TokenRejected`
    /// here means "expired", never "revoked".
    fn execute_refresh_token_flow(self) -> Result<SignInOutcome> {
        let Credentials::RefreshToken(token) = &self.credentials else {
            unreachable!("dispatched by execute()");
        };

        let claims = refresh_token_claims(token.expose())?;
        if let Some(exp) = claims.exp {
            if exp.saturating_sub(TOKEN_EXPIRY_SLACK_SECS) <= now_unix() {
                return Ok(SignInOutcome::TokenRejected);
            }
        }

        Ok(SignInOutcome::Success {
            steam_id: claims.steam_id,
            refresh_token: token.clone(),
            access_token: None,
        })
    }
}

/// Builder for a QR-code sign-in.
///
/// Construct via [`SignIn::with_qr`], add optional config, then call
/// [`Self::begin`] to open the session and get the URL to display.
///
/// `Debug` is implemented by hand to match [`SignIn`]'s style: `rate_limiter`
/// is shown only as a presence marker, since [`RateLimiter`] itself has no
/// `Debug` impl.
#[derive(Clone)]
#[must_use = "QrSignIn does nothing until .begin() is awaited"]
pub struct QrSignIn {
    proxy: Option<ProxyConfig>,
    rate_limiter: Option<Arc<RateLimiter>>,
}

impl fmt::Debug for QrSignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrSignIn")
            .field("proxy", &self.proxy)
            .field("rate_limiter", &self.rate_limiter.is_some())
            .finish()
    }
}

impl QrSignIn {
    /// Route `BeginAuthSessionViaQR` and the subsequent poll through a SOCKS5
    /// or HTTP-CONNECT proxy. See [`SignIn::proxy`].
    pub fn proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = Some(proxy);
        self
    }

    /// Pace this sign-in's `WebAPI` requests through a shared limiter. See
    /// [`SignIn::rate_limiter`]; the same 120s poll budget applies here.
    pub fn rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// Open the QR auth session: calls `BeginAuthSessionViaQR` and returns
    /// the [`QrSession`] holding the challenge URL to display. Call
    /// [`QrSession::poll`] afterwards to wait for the human.
    ///
    /// # Errors
    ///
    /// - Transport / proxy errors propagate as-is.
    /// - A rate-limited or throttled response comes back as
    ///   [`Error::AuthRateLimited`].
    /// - Any other non-OK `EResult`, or a response missing a field this flow
    ///   needs, comes back as [`Error::AuthRejected`].
    pub async fn begin(self) -> Result<QrSession> {
        let client = WebApiClient::new(self.proxy.as_ref(), self.rate_limiter.clone())?;

        let req = CAuthenticationBeginAuthSessionViaQrRequest {
            device_friendly_name: Some("steamroids".into()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            device_details: Some(CAuthenticationDeviceDetails {
                device_friendly_name: Some("steamroids".into()),
                platform_type: Some(PLATFORM_STEAM_CLIENT),
                os_type: Some(OS_TYPE_WINDOWS_10),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (er, resp): (_, CAuthenticationBeginAuthSessionViaQrResponse) = client
            .call("BeginAuthSessionViaQR", HttpMethod::Post, &req)
            .await?;
        if er != EResult::OK {
            // qr begin has no credentials to reject, so throttle is the only
            // outcome worth naming; anything else is just an error.
            if er == EResult::RATE_LIMIT_EXCEEDED || er == EResult::ACCOUNT_LOGIN_DENIED_THROTTLE {
                return Err(Error::AuthRateLimited(format!(
                    "BeginAuthSessionViaQR eresult {}",
                    er.0
                )));
            }
            return Err(Error::AuthRejected(format!(
                "BeginAuthSessionViaQR eresult {}",
                er.0
            )));
        }

        let client_id = resp.client_id.ok_or_else(|| {
            Error::AuthRejected("BeginAuthSessionViaQR response: missing client_id".into())
        })?;
        let challenge_url = resp.challenge_url.ok_or_else(|| {
            Error::AuthRejected("BeginAuthSessionViaQR response: missing challenge_url".into())
        })?;
        let request_id = resp.request_id.ok_or_else(|| {
            Error::AuthRejected("BeginAuthSessionViaQR response: missing request_id".into())
        })?;
        let poll_interval = compute_poll_interval(resp.interval);

        debug!(
            target: "steamroids::auth::signin",
            client_id,
            "BeginAuthSessionViaQR accepted"
        );

        Ok(QrSession {
            client,
            challenge_url,
            begin: BeginSession {
                client_id,
                request_id,
                // qr response carries no steamid; poll_for_token only falls
                // back to this if the refresh-token JWT itself won't decode.
                session_steamid: 0,
                poll_interval,
                guards: Vec::new(),
                email_domain_hint: None,
            },
        })
    }
}

/// An opened QR sign-in session: the URL to display and the pending poll for
/// a human to approve it in the Steam mobile app.
///
/// Obtained from [`QrSignIn::begin`]. Display [`Self::challenge_url`] first,
/// then call [`Self::poll`]; the URL does not rotate for the lifetime of
/// this session, see the [module docs](self#limitation-qrsessionchallenge_url-does-not-rotate).
pub struct QrSession {
    client: WebApiClient,
    begin: BeginSession,
    challenge_url: String,
}

impl fmt::Debug for QrSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrSession")
            .field("challenge_url", &self.challenge_url)
            .field("client_id", &self.begin.client_id)
            .finish_non_exhaustive()
    }
}

impl QrSession {
    /// The URL to render as a QR code (or hand to the user directly, the
    /// Steam mobile app also accepts it pasted into a browser).
    ///
    /// This crate does not render QR codes itself: encode/display the URL
    /// however fits the caller (a terminal QR renderer, a web page, …).
    pub fn challenge_url(&self) -> &str {
        &self.challenge_url
    }

    /// Block until the session is approved, declined, or times out.
    ///
    /// Polls `PollAuthSessionStatus` at Steam's suggested interval (falling
    /// back sensibly if it's absent or zero; see [`SignIn::rate_limiter`]'s
    /// docs on the shared 120s wall-clock budget), the same loop the password
    /// flow uses. Display [`Self::challenge_url`] *before* calling this: a
    /// human has to scan it while this call is blocked waiting.
    ///
    /// # Errors
    ///
    /// - An unexpected non-OK `EResult` mid-poll comes back as
    ///   [`Error::AuthRejected`].
    /// - Nobody scanning (or the session otherwise never resolving) within
    ///   the poll budget comes back as [`Error::Timeout`].
    pub async fn poll(self) -> Result<SignInOutcome> {
        poll_for_token(&self.client, &self.begin).await
    }
}

/// Current Unix time in seconds (0 if the clock is before the epoch).
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        if let Some(outcome) = map_non_ok_eresult(er) {
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
        platform_type: Some(PLATFORM_STEAM_CLIENT),
        persistence: Some(SESSION_PERSISTENT),
        // No `website_id` — that's a WebBrowser-platform concept.
        device_details: Some(CAuthenticationDeviceDetails {
            device_friendly_name: Some("steamroids".into()),
            platform_type: Some(PLATFORM_STEAM_CLIENT),
            os_type: Some(OS_TYPE_WINDOWS_10),
            ..Default::default()
        }),
        ..Default::default()
    };
    let (er, resp): (_, CAuthenticationBeginAuthSessionViaCredentialsResponse) = client
        .call("BeginAuthSessionViaCredentials", HttpMethod::Post, &req)
        .await?;
    if er != EResult::OK {
        if let Some(outcome) = map_non_ok_eresult(er) {
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
            if let Some(outcome) = map_non_ok_eresult(er) {
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
    let mut req = CAuthenticationPollAuthSessionStatusRequest {
        client_id: Some(begin.client_id),
        request_id: Some(begin.request_id.clone()),
        ..Default::default()
    };
    let deadline = Instant::now() + POLL_BUDGET;
    let mut first = true;
    loop {
        // Poll immediately on the first try; sleep between retries only.
        // Saves a guaranteed `poll_interval` of latency on every successful
        // login when Steam already has our token ready.
        if first {
            first = false;
        } else {
            sleep(begin.poll_interval).await;
            if Instant::now() >= deadline {
                break;
            }
        }

        let (er, resp): (_, CAuthenticationPollAuthSessionStatusResponse) = client
            .call("PollAuthSessionStatus", HttpMethod::Post, &req)
            .await?;
        if er != EResult::OK {
            if let Some(outcome) = map_non_ok_eresult(er) {
                return Ok(outcome);
            }
            return Err(Error::AuthRejected(format!(
                "PollAuthSessionStatus eresult {}",
                er.0
            )));
        }

        // Steam may rotate the auth session mid-poll; keep polling the new one.
        req.client_id = Some(rotate_client_id(
            req.client_id.unwrap_or(begin.client_id),
            resp.new_client_id,
        ));

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

/// Apply Steam's `new_client_id` rotation hint, keeping `current` when the
/// field is absent or zero.
fn rotate_client_id(current: u64, hint: Option<u64>) -> u64 {
    hint.filter(|id| *id != 0).unwrap_or(current)
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

    #[test]
    fn qr_builder_round_trips_config() {
        let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(1)));
        let qr = SignIn::with_qr()
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .rate_limiter(Arc::clone(&limiter));
        assert!(qr.proxy.is_some());
        assert!(qr.rate_limiter.is_some());
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

    /// Build a JWT whose payload is `payload_json`. Header / signature are
    /// placeholders — the flow only decodes the middle segment.
    fn build_jwt(payload_json: &str) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
        use base64::Engine;
        let header = B64URL.encode(br#"{"alg":"none"}"#);
        let payload = B64URL.encode(payload_json.as_bytes());
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn refresh_token_valid_jwt_passes_through_without_webapi() {
        // exp far in the future (year ~2100); no network is touched.
        let jwt = build_jwt(r#"{"sub":"76561198000000001","exp":4102444800}"#);
        let outcome = SignIn::with_refresh_token(&jwt).execute().await.unwrap();
        match outcome {
            SignInOutcome::Success {
                steam_id,
                refresh_token,
                access_token,
            } => {
                assert_eq!(steam_id, 76_561_198_000_000_001);
                assert_eq!(refresh_token.expose(), jwt, "token handed back unchanged");
                assert!(access_token.is_none(), "no web access token is minted");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_token_expired_jwt_is_rejected() {
        let jwt = build_jwt(r#"{"sub":"76561198000000001","exp":1}"#);
        let outcome = SignIn::with_refresh_token(&jwt).execute().await.unwrap();
        assert!(matches!(outcome, SignInOutcome::TokenRejected));
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
        assert!(
            !dbg.contains("secret-refresh-jwt"),
            "refresh token leaked: {dbg}"
        );
        assert!(
            !dbg.contains("secret-access-jwt"),
            "access token leaked: {dbg}"
        );
        // Non-secret context (the SteamID) and token presence stay visible.
        assert!(dbg.contains("76561198000000001"));
        assert!(dbg.contains("redacted"));
    }

    #[test]
    fn signin_builder_debug_redacts_password() {
        // SignIn's Debug must not leak via its embedded credentials.
        let dbg = format!("{:?}", SignIn::with_password("bot01", "leak-me"));
        assert!(
            !dbg.contains("leak-me"),
            "password leaked through SignIn: {dbg}"
        );
    }

    /// A store whose methods must never run — used to prove the refresh-token
    /// builder bypasses the store entirely.
    struct PanicStore;

    impl TokenStore for PanicStore {
        async fn load(
            &self,
            _account: &str,
        ) -> std::result::Result<Option<String>, crate::auth::TokenStoreError> {
            panic!("store must not be loaded for a refresh-token builder");
        }
        async fn save(
            &self,
            _account: &str,
            _token: &str,
        ) -> std::result::Result<(), crate::auth::TokenStoreError> {
            panic!("store must not be saved for a refresh-token builder");
        }
    }

    /// A store holding one corrupted entry, recording whether `save` ran.
    struct CorruptStore {
        saved: std::sync::Mutex<Option<String>>,
    }

    impl TokenStore for CorruptStore {
        async fn load(
            &self,
            _account: &str,
        ) -> std::result::Result<Option<String>, crate::auth::TokenStoreError> {
            Ok(Some("this-is-not-a-jwt".into()))
        }
        async fn save(
            &self,
            _account: &str,
            token: &str,
        ) -> std::result::Result<(), crate::auth::TokenStoreError> {
            *self.saved.lock().unwrap() = Some(token.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn execute_with_store_falls_back_when_stored_token_is_corrupt() {
        // A garbage store entry must not propagate as Err; it has to fall
        // through to the password flow. Route through a dead local proxy so
        // the fallback fails at the first HTTP hop instead of reaching Steam.
        let store = CorruptStore {
            saved: std::sync::Mutex::new(None),
        };
        let err = SignIn::with_password("bot01", "pw")
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .execute_with_store(&store)
            .await
            .unwrap_err();
        // Network, not AuthRejected: we got past the token stage.
        assert!(
            matches!(err, Error::Network(_)),
            "expected the password flow to run, got {err:?}"
        );
        assert!(
            store.saved.lock().unwrap().is_none(),
            "a corrupt token must never be written back"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn execute_waits_on_an_attached_rate_limiter() {
        // burn the first slot so the next acquire must wait
        // 45s: clearly above both http.rs's 30s TOTAL_TIMEOUT and its 10s
        // connect_timeout, so this asserts the limiter waited, not a reqwest
        // timeout that happened to fire first.
        let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(45)));
        limiter.acquire().await;

        let start = Instant::now();
        // dead proxy: fails at connect, but the limiter must be consulted first
        let _ = SignIn::with_password("bot01", "pw")
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .rate_limiter(Arc::clone(&limiter))
            .execute()
            .await;
        assert!(start.elapsed() >= Duration::from_secs(45));
    }

    #[tokio::test]
    async fn execute_without_a_limiter_does_not_wait() {
        // real time, no start_paused: connect-refused timing is os-dependent,
        // fine for the paired test's lower bound but not this upper bound.
        // 20s: above http.rs's 10s connect timeout, below the 45s floor a
        // limiter would force.
        let start = std::time::Instant::now();
        let _ = SignIn::with_password("bot01", "pw")
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .execute()
            .await;
        assert!(start.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn qr_begin_fails_over_a_dead_proxy() {
        // proves begin() actually dispatches BeginAuthSessionViaQR over the
        // configured transport, same shape as the password flow's proxy test.
        let err = SignIn::with_qr()
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .begin()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Network(_)), "got {err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn qr_begin_waits_on_an_attached_rate_limiter() {
        // same reasoning as execute_waits_on_an_attached_rate_limiter, for
        // the qr begin() path.
        let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(45)));
        limiter.acquire().await;

        let start = Instant::now();
        let _ = SignIn::with_qr()
            .proxy(ProxyConfig::parse("socks5://127.0.0.1:1").unwrap())
            .rate_limiter(Arc::clone(&limiter))
            .begin()
            .await;
        assert!(start.elapsed() >= Duration::from_secs(45));
    }

    #[test]
    fn rotate_client_id_keeps_current_without_a_hint() {
        assert_eq!(rotate_client_id(7, None), 7);
        assert_eq!(rotate_client_id(7, Some(0)), 7);
    }

    #[test]
    fn rotate_client_id_follows_steams_rotation() {
        assert_eq!(rotate_client_id(7, Some(9)), 9);
    }

    #[tokio::test]
    async fn execute_with_store_bypasses_store_for_refresh_builder() {
        // The malformed JWT fails offline before any network work, and the
        // store must not be touched — so PanicStore's methods never fire.
        let err = SignIn::with_refresh_token("not.a.real.jwt")
            .execute_with_store(&PanicStore)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }
}
