//! Save the login and reuse it — bring up a session with no password / 2FA.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # first run only
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! TOKEN_FILE=steam_tokens.json \                       # optional — where to store
//!     cargo run --example 11_persist_login
//! ```
//!
//! The "session data" you persist is the **refresh token**. The flow:
//!
//! - **First run** — no saved token, so do the password (+2FA) login once and
//!   save the issued refresh token to `TOKEN_FILE`.
//! - **Later runs** — load the token and hand it straight to
//!   [`spawn_session`](steamroids::session::spawn_session); the CM logon uses it,
//!   so **no password, no 2FA, and no extra `WebAPI` round-trip** are needed.
//!
//! For a `WebAPI`-managed alternative (validate / rotate the token on each run)
//! see [`SignIn::execute_with_store`](steamroids::auth::SignIn::execute_with_store)
//! — but note that a per-call `WebAPI` re-auth through a *rotating* proxy can trip
//! Steam's anti-fraud (a new exit IP looks like a new location). Handing the saved
//! token to `spawn_session` avoids that.

use std::env;

use steamroids::auth::{FileTokenStore, RefreshToken, SignIn, SignInOutcome, TokenStore};
use steamroids::persona;
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,steamroids=info".into()),
        )
        .init();

    let account = env::var("STEAM_ACCOUNT").map_err(|_| "STEAM_ACCOUNT env var not set")?;
    let token_file = env::var("TOKEN_FILE").unwrap_or_else(|_| "steam_tokens.json".into());
    let proxy = env::var("PROXY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|u| ProxyConfig::parse(&u))
        .transpose()?;

    let store = FileTokenStore::new(&token_file);

    // ---- Get a refresh token: reuse the saved one, or do a one-time login. ----
    let refresh: RefreshToken = if let Some(token) = store.load(&account).await? {
        println!("→ reusing saved token for '{account}' (no password / 2FA)");
        RefreshToken::new(token)
    } else {
        println!("→ no saved token for '{account}'; doing the one-time password + 2FA login");
        let password =
            env::var("STEAM_PASSWORD").map_err(|_| "STEAM_PASSWORD needed for first login")?;
        let mut signin = SignIn::with_password(account.clone(), password);
        if let Some(secret) = env::var("SHARED_SECRET").ok().filter(|s| !s.is_empty()) {
            signin = signin.shared_secret(secret);
        }
        if let Some(p) = &proxy {
            signin = signin.proxy(p.clone());
        }
        match signin.execute().await? {
            SignInOutcome::Success { refresh_token, .. } => {
                store.save(&account, refresh_token.expose()).await?;
                println!("✓ logged in; saved refresh token to '{token_file}'");
                refresh_token
            }
            other => {
                eprintln!("✗ sign-in did not succeed: {other:?}");
                std::process::exit(1);
            }
        }
    };

    // ---- Log in with the token: bring up a live CM session. ------------------
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account.clone(),
        refresh_token: refresh,
        proxy,
    })
    .await?;
    println!("✓ session up for steam_id {}", handle.steam_id());

    // Prove it's a real, working session (best-effort — a slow proxy exit can
    // make this one request time out without meaning the login failed).
    match persona::request_player_summary(&handle, handle.steam_id()).await {
        Ok(me) => {
            println!("  persona name : {}", me.persona_name);
            println!("  profile URL  : {}", me.profile_url);
        }
        Err(e) => println!("  (persona fetch skipped: {e})"),
    }

    handle.logoff().await?;
    let _ = join.await;
    println!("\nRun this again — it now logs in straight from the saved token.");
    Ok(())
}
