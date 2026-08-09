//! Sign in with username + password (and optional 2FA / proxy).
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                  # optional — mobile 2FA
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//!     cargo run --example 04_signin_credentials
//! ```
//!
//! On success, prints the Steam ID and the issued refresh token. Persist the
//! refresh token and reuse it via `examples/05_signin_refresh_token.rs` to
//! skip 2FA on later logins.
//!
//! Drives the real `IAuthenticationService` flow against `api.steampowered.com`
//! — `GetPasswordRSAPublicKey` → encrypt → `BeginAuthSessionViaCredentials`
//! → `UpdateAuthSessionWithSteamGuardCode` (if mobile 2FA) →
//! `PollAuthSessionStatus`. Optional `PROXY_URL` is routed through reqwest.

use std::env;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,steamroids=debug".into()),
        )
        .init();

    let account = env::var("STEAM_ACCOUNT").map_err(|_| "STEAM_ACCOUNT env var not set")?;
    let password = env::var("STEAM_PASSWORD").map_err(|_| "STEAM_PASSWORD env var not set")?;
    // Empty counts as unset: CI passes an unconfigured secret through as an
    // empty string, and an empty PROXY_URL would otherwise reach the parser as
    // a URL and fail with `RelativeUrlWithoutBase`.
    let shared_secret = env::var("SHARED_SECRET").ok().filter(|s| !s.is_empty());
    let proxy_url = env::var("PROXY_URL").ok().filter(|s| !s.is_empty());

    // Builder: required → optional. Chaining only the fields that are set
    // keeps the call site readable when the optional inputs vary.
    let mut signin = SignIn::with_password(account, password);
    if let Some(secret) = shared_secret {
        signin = signin.shared_secret(secret);
    }
    if let Some(url) = proxy_url {
        let proxy = ProxyConfig::parse(&url)?;
        eprintln!("→ routing through proxy: {}", proxy.host);
        signin = signin.proxy(proxy);
    }

    match signin.execute().await {
        Ok(SignInOutcome::Success {
            steam_id,
            refresh_token,
            access_token,
        }) => {
            println!("✓ logged in");
            println!("  steam_id      : {steam_id}");
            println!("  refresh_token : {}", refresh_token.expose());
            if let Some(at) = access_token {
                println!("  access_token  : {at}");
            }
            println!();
            println!("Persist refresh_token to reuse via example 05 (no 2FA next time).");
        }
        Ok(SignInOutcome::NeedsMobileGuardCode) => {
            eprintln!("✗ 2FA required but no SHARED_SECRET provided");
            std::process::exit(2);
        }
        Ok(SignInOutcome::NeedsEmailGuardCode { email_domain }) => {
            eprintln!("✗ email Steam Guard required (sent to …@{email_domain})");
            eprintln!("  email-Guard handling is not in scope for this example");
            std::process::exit(2);
        }
        Ok(SignInOutcome::InvalidCredentials) => {
            eprintln!("✗ bad username or password");
            std::process::exit(1);
        }
        Ok(SignInOutcome::TokenRejected) => {
            // Unreachable in the password flow but exhaustive matching keeps
            // us honest if SignInOutcome grows.
            eprintln!("✗ token rejected (unexpected for password flow)");
            std::process::exit(1);
        }
        Ok(SignInOutcome::RateLimited { retry_hint }) => {
            eprintln!("✗ rate-limited by Steam (retry after {retry_hint:?})");
            std::process::exit(3);
        }
        // SignInOutcome is #[non_exhaustive] — future variants land here.
        Ok(other) => {
            eprintln!("✗ unhandled sign-in outcome: {other:?}");
            std::process::exit(1);
        }
        Err(Error::NotImplemented(stage)) => {
            eprintln!("⚠ wired but not yet implemented: {stage}");
            eprintln!("  (proxy validation + TOTP derivation already ran successfully)");
            std::process::exit(75); // EX_TEMPFAIL
        }
        Err(e) => {
            eprintln!("✗ transport / protocol error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
