//! Dump everything we can currently fetch about one account, after login.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # optional — mobile 2FA
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! TARGET_STEAMID=76561198000000000 \                   # optional — default: self
//!     cargo run --example 10_account_dump
//! ```
//!
//! Pulls, keyless over one CM session: the persona summary (name, avatar, status,
//! current game), the public profile fields (real name, location, summary, age),
//! the friends list (with a sample of resolved names), and — if the account owns
//! CS2 — its Game Coordinator profile (level + XP). Each section is best-effort,
//! so one empty/unavailable bit doesn't abort the rest.

use std::env;
use std::time::Duration;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::{cs2, friends, persona};

#[tokio::main]
#[allow(clippy::too_many_lines)] // a linear, top-to-bottom dump reads best in one fn
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,steamroids=info".into()),
        )
        .init();

    let account = env::var("STEAM_ACCOUNT").map_err(|_| "STEAM_ACCOUNT env var not set")?;
    let password = env::var("STEAM_PASSWORD").map_err(|_| "STEAM_PASSWORD env var not set")?;
    let shared_secret = env::var("SHARED_SECRET").ok();
    let proxy = env::var("PROXY_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|u| ProxyConfig::parse(&u))
        .transpose()?;

    // ---- 1. Sign in ----------------------------------------------------------
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

    // ---- 2. Live CM session --------------------------------------------------
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account.clone(),
        refresh_token,
        proxy: proxy.clone(),
    })
    .await?;

    // Capture the friends list FIRST: Steam pushes it once, right after login, so
    // grab it before any other await (persona/profile/GC) consumes that window.
    let friends_list = friends::request_friends_list(&handle).await;

    let target: u64 = match env::var("TARGET_STEAMID").ok() {
        Some(s) => s.parse()?,
        None => steam_id,
    };
    let account_id = cs2::account_id_from_steam_id(target);

    println!("\n============================================================");
    println!(" ACCOUNT DUMP");
    println!("============================================================");
    println!("login account : {account}");
    println!("SteamID64     : {target}");
    println!("account id     : {account_id}");
    println!("profile URL    : {}", persona::profile_url(target));

    // ---- 3. Persona summary --------------------------------------------------
    println!("\n--- persona summary ---");
    match persona::request_player_summary(&handle, target).await {
        Ok(s) => {
            println!("name          : {}", s.persona_name);
            println!("status        : {:?}", s.state);
            println!("avatar (full) : {}", s.avatar_url);
            println!(
                "avatar (med.) : {}",
                s.avatar_url.replace("_full.jpg", "_medium.jpg")
            );
            println!("avatar hash   : {}", s.avatar_hash);
            match &s.current_game {
                Some(g) => println!("in game       : {} (app {})", g.name, g.app_id),
                None => println!("in game       : —"),
            }
            // Download the actual image and save it next to the binary's CWD.
            match persona::fetch_avatar(&s.avatar_url, proxy.as_ref(), None).await {
                Ok(bytes) => {
                    let path = format!("avatar_{account_id}.jpg");
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        println!(
                            "avatar image  : ({} bytes, but save failed: {e})",
                            bytes.len()
                        );
                    } else {
                        println!("avatar image  : saved {} bytes -> {path}", bytes.len());
                    }
                }
                Err(e) => println!("avatar image  : (download failed: {e})"),
            }
        }
        Err(e) => println!("(unavailable: {e})"),
    }

    // ---- 4. Public profile fields -------------------------------------------
    println!("\n--- profile info ---");
    match persona::request_profile_info(&handle, target).await {
        Ok(p) => {
            print_opt("real name", p.real_name.as_deref());
            let location: Vec<String> =
                [p.city, p.state, p.country].into_iter().flatten().collect();
            println!(
                "location      : {}",
                if location.is_empty() {
                    "—".into()
                } else {
                    location.join(", ")
                }
            );
            print_opt("summary", p.summary.as_deref());
            match p.created_at {
                Some(t) => println!("created (unix) : {t}"),
                None => println!("created (unix) : —"),
            }
        }
        Err(e) => println!("(unavailable: {e})"),
    }

    // ---- 5. Friends (captured right after login, above) ----------------------
    println!("\n--- friends ---");
    match friends_list {
        Ok(list) => {
            println!("count         : {}", list.len());
            // Resolve a sample of names (one persona request each).
            for f in list.iter().take(15) {
                let name = match persona::request_player_summary(&handle, f.steam_id).await {
                    Ok(s) => s.persona_name,
                    Err(_) => "<name unavailable>".into(),
                };
                println!("  {} {:?} — {name}", f.steam_id, f.relationship);
            }
            if list.len() > 15 {
                println!("  … and {} more", list.len() - 15);
            }
        }
        Err(e) => println!("(unavailable: {e} — call right after login to catch the pushed list)"),
    }

    // ---- 6. CS2 Game Coordinator profile ------------------------------------
    println!("\n--- CS2 profile ---");
    match cs2::attach(handle.clone()).await {
        Ok(gc) => match gc.wait_ready(Duration::from_secs(20)).await {
            Ok(()) => match cs2::request_player_profile(&gc, account_id).await {
                Ok(p) => {
                    println!("level         : {}", p.level);
                    println!("current XP    : {}", p.current_xp);
                    match (p.competitive_rank, p.competitive_wins) {
                        (Some(r), w) => {
                            println!("competitive   : rank {r} ({} wins)", w.unwrap_or(0));
                        }
                        (None, _) => println!("competitive   : unranked / not played"),
                    }
                }
                Err(e) => println!("(no profile data: {e})"),
            },
            Err(e) => println!("(GC not ready: {e} — account may lack a CS2 license)"),
        },
        Err(e) => println!("(GC attach failed: {e})"),
    }

    println!("\n============================================================");

    // ---- 7. Clean shutdown ---------------------------------------------------
    handle.logoff().await?;
    let _ = join.await;
    Ok(())
}

fn print_opt(label: &str, value: Option<&str>) {
    println!("{label:<13} : {}", value.unwrap_or("—"));
}
