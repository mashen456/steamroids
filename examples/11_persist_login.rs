//! Persist the login and reuse it — with **your** storage.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # first run only
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! TOKEN_FILE=refresh_token.txt \                       # optional — your store
//!     cargo run --example 11_persist_login
//! ```
//!
//! The "session data" you save is the **refresh token**. The library is
//! storage-agnostic: it hands you the token string ([`RefreshToken::expose`])
//! and takes one back ([`SignIn::with_refresh_token`] /
//! [`SessionConfig::refresh_token`]). *You* persist it wherever fits — a DB row,
//! a Redis key, a file. (For automatic load/try/save, implement the
//! [`TokenStore`](steamroids::auth::TokenStore) trait over your store and use
//! [`execute_with_store`](steamroids::auth::SignIn::execute_with_store).)
//!
//! - **First run** — no saved token, so do the password (+2FA) login once and
//!   persist the issued refresh token.
//! - **Later runs** — load the token and hand it to
//!   [`spawn_session`](steamroids::session::spawn_session): no password, no 2FA.
//!
//! Below, the "store" is a plain file written by *this example* (caller code) to
//! stand in for your database — the library never touches it.

use std::{env, fs};

use steamroids::auth::{RefreshToken, SignIn, SignInOutcome};
use steamroids::persona;
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;

// ---- Stand-in for YOUR storage. Swap these two for your DB / Redis / etc. ----
fn load_token(store: &str) -> Option<String> {
    fs::read_to_string(store)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn save_token(store: &str, token: &str) -> std::io::Result<()> {
    fs::write(store, token)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,steamroids=info".into()),
        )
        .init();

    let account = env::var("STEAM_ACCOUNT").map_err(|_| "STEAM_ACCOUNT env var not set")?;
    let store = env::var("TOKEN_FILE").unwrap_or_else(|_| "refresh_token.txt".into());
    let proxy = env::var("PROXY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|u| ProxyConfig::parse(&u))
        .transpose()?;

    // ---- Get a refresh token: reuse the one in your store, or log in once. ----
    let refresh: RefreshToken = if let Some(token) = load_token(&store) {
        println!("→ found a saved token in '{store}'; logging in without password / 2FA");
        RefreshToken::new(token)
    } else {
        println!("→ no saved token; doing the one-time password + 2FA login");
        let password =
            env::var("STEAM_PASSWORD").map_err(|_| "STEAM_PASSWORD needed for the first login")?;
        let mut signin = SignIn::with_password(account.clone(), password);
        if let Some(secret) = env::var("SHARED_SECRET").ok().filter(|s| !s.is_empty()) {
            signin = signin.shared_secret(secret);
        }
        if let Some(p) = &proxy {
            signin = signin.proxy(p.clone());
        }
        match signin.execute().await? {
            SignInOutcome::Success { refresh_token, .. } => {
                // The library gives you the raw string — persist it in YOUR store.
                save_token(&store, refresh_token.expose())?;
                println!("✓ logged in; persisted the refresh token to your store ('{store}')");
                refresh_token
            }
            other => {
                eprintln!("✗ sign-in did not succeed: {other:?}");
                std::process::exit(1);
            }
        }
    };

    // ---- Hand the token back to the library to bring up a session. -----------
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account,
        refresh_token: refresh,
        proxy,
    })
    .await?;
    println!("✓ session up for steam_id {}", handle.steam_id());

    // Best-effort proof (a slow proxy exit can make one request time out).
    match persona::request_player_summary(&handle, handle.steam_id()).await {
        Ok(me) => println!("  persona: {} — {}", me.persona_name, me.profile_url),
        Err(e) => println!("  (persona fetch skipped: {e})"),
    }

    handle.logoff().await?;
    let _ = join.await;
    println!("\nRun again — it now logs in straight from the saved token.");
    Ok(())
}
