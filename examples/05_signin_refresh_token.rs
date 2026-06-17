//! Sign in with a previously-issued refresh token (skips 2FA).
//!
//! ```text
//! REFRESH_TOKEN=eyJhbGc...           \
//! PROXY_URL=socks5://user:pass@host:1080 \      # optional
//!     cargo run --example 05_signin_refresh_token
//! ```
//!
//! Refresh tokens come from a successful run of
//! `examples/04_signin_credentials.rs`. Persist them per-account so subsequent
//! logins are 2FA-free and (usually) password-free.
//!
//! If the token is expired, the example exits with status 1 — callers should
//! fall back to the password flow.
//!
//! Validates the token locally: the Steam ID is read from the JWT `sub` claim
//! and the `exp` claim is checked. No web access token is minted — Steam only
//! redeems these `SteamClient` tokens over an authenticated CM session, not the
//! `WebAPI`, so `access_token` is `None`. The token is meant to be handed to
//! `spawn_session` for a CM `ClientLogon` (see `examples/11_persist_login.rs`).

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

    let token = env::var("REFRESH_TOKEN").map_err(|_| "REFRESH_TOKEN env var not set")?;
    let proxy_url = env::var("PROXY_URL").ok();

    let mut signin = SignIn::with_refresh_token(token);
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
            // Steam sometimes rotates the refresh token on use; persist the
            // returned one in place of the old.
            println!("  refresh_token : {}", refresh_token.expose());
            if let Some(at) = access_token {
                println!("  access_token  : {at}");
            }
        }
        Ok(SignInOutcome::TokenRejected) => {
            eprintln!("✗ refresh token rejected — fall back to password flow (example 04)");
            std::process::exit(1);
        }
        Ok(SignInOutcome::RateLimited { retry_hint }) => {
            eprintln!("✗ rate-limited by Steam (retry after {retry_hint:?})");
            std::process::exit(3);
        }
        // The password-flow outcomes shouldn't occur for refresh-token sign-in
        // but we exhaust the enum so future variants force a re-think here.
        Ok(other) => {
            eprintln!("✗ unexpected outcome for refresh-token flow: {other:?}");
            std::process::exit(1);
        }
        Err(Error::NotImplemented(stage)) => {
            eprintln!("⚠ wired but not yet implemented: {stage}");
            std::process::exit(75); // EX_TEMPFAIL
        }
        Err(e) => {
            eprintln!("✗ transport / protocol error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
