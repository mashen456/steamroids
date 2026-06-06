//! Fetch profile details (persona name, avatar, profile URL, …) after login.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # optional — mobile 2FA
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! TARGET_STEAMID=76561198000000000 \                   # whose profile (default: self)
//!     cargo run --example 08_profile_details
//! ```
//!
//! Pure CM-session path — no `WebAPI` key. After login it asks the Connection
//! Manager for the player's persona (`CMsgClientRequestFriendData` →
//! `CMsgClientPersonaState`) and public profile fields
//! (`CMsgClientFriendProfileInfo`), then prints the `SteamID`, name, profile
//! URL, avatar URL, online status, current game, and any shared real
//! name / location / summary.

use std::env;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::persona;
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;

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
    let (own_steam_id, refresh_token) = match signin.execute().await? {
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

    // 2. Bring up a live CM session.
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account,
        refresh_token,
        proxy,
    })
    .await?;

    let target: u64 = match env::var("TARGET_STEAMID").ok() {
        Some(s) => s.parse()?,
        None => own_steam_id,
    };

    // 3. Persona summary (name, avatar, status, current game).
    let summary = persona::request_player_summary(&handle, target).await?;
    println!();
    println!("SteamID     : {}", summary.steam_id);
    println!("Name        : {}", summary.persona_name);
    println!("Profile URL : {}", summary.profile_url);
    println!("Avatar      : {}", summary.avatar_url);
    println!("Status      : {:?}", summary.state);
    if let Some(game) = &summary.current_game {
        println!("In game     : {} (app {})", game.name, game.app_id);
    }

    // 4. Public profile fields (best-effort — many are private/empty).
    match persona::request_profile_info(&handle, target).await {
        Ok(info) => {
            if let Some(name) = info.real_name {
                println!("Real name   : {name}");
            }
            let location: Vec<String> = [info.city, info.state, info.country]
                .into_iter()
                .flatten()
                .collect();
            if !location.is_empty() {
                println!("Location    : {}", location.join(", "));
            }
            if let Some(summary) = info.summary {
                println!("Summary     : {summary}");
            }
        }
        Err(e) => eprintln!("(profile info unavailable: {e})"),
    }

    // 5. Clean shutdown.
    handle.logoff().await?;
    let _ = join.await;
    Ok(())
}
