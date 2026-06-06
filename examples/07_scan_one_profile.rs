//! Scan one CS2 player's public profile (level + XP) through the Game Coordinator.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # optional — mobile 2FA
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! TARGET_STEAMID=76561198000000000 \                   # whose profile to scan
//!     cargo run --example 07_scan_one_profile
//! ```
//!
//! If `TARGET_STEAMID` is unset, scans the logged-in account's own profile.
//!
//! End to end: `WebAPI` sign-in -> CM session (`spawn_session`) -> "play" CS2 so
//! the GC routes to us -> `ClientHello`/`ClientWelcome` handshake -> request the
//! player's profile -> print the level, XP, and competitive rank. This is the
//! 0.3.x milestone: a real value pulled from the CS2 Game Coordinator.

use std::env;
use std::time::Duration;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::{cs2, gc::GameCoordinator};

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
    let shared_secret = env::var("SHARED_SECRET").ok();
    let proxy = env::var("PROXY_URL")
        .ok()
        .map(|u| ProxyConfig::parse(&u))
        .transpose()?;

    // 1. Sign in for a refresh token.
    let mut signin = SignIn::with_password(account.clone(), password);
    if let Some(secret) = shared_secret {
        signin = signin.shared_secret(secret);
    }
    if let Some(p) = &proxy {
        eprintln!("→ routing through proxy: {}", p.display());
        signin = signin.proxy(p.clone());
    }
    let (steam_id, refresh_token) = match signin.execute().await? {
        SignInOutcome::Success {
            steam_id,
            refresh_token,
            ..
        } => (steam_id, refresh_token),
        other => {
            eprintln!("✗ sign-in did not succeed: {other:?}");
            std::process::exit(1);
        }
    };
    println!("✓ signed in as {steam_id}");

    // Who to scan: an explicit target, or our own account.
    let target_id = match env::var("TARGET_STEAMID").ok() {
        Some(s) => cs2::account_id_from_steam_id(s.parse()?),
        None => cs2::account_id_from_steam_id(steam_id),
    };

    // 2. Bring up a live CM session.
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account,
        refresh_token,
        proxy,
    })
    .await?;
    println!("✓ CM session up (steam_id {})", handle.steam_id());

    // 3. Attach to the CS2 Game Coordinator and wait for its welcome.
    let gc = GameCoordinator::attach(handle.clone(), cs2::APP_ID).await?;
    gc.wait_ready(Duration::from_secs(20)).await?;
    println!("✓ CS2 Game Coordinator ready");

    // 4. Ask for the profile and print what came back.
    let profile = cs2::request_player_profile(&gc, target_id).await?;
    println!();
    println!("CS2 profile for account {}:", profile.account_id);
    println!("  level       : {}", profile.level);
    println!("  current XP  : {}", profile.current_xp);
    match (profile.competitive_rank, profile.competitive_wins) {
        (Some(rank), wins) => println!("  competitive : rank {rank} ({} wins)", wins.unwrap_or(0)),
        (None, _) => println!("  competitive : unranked / not played"),
    }

    // 5. Clean shutdown.
    handle.logoff().await?;
    let _ = join.await;
    Ok(())
}
