//! List friends (with names), resolve a vanity URL, and optionally add/remove.
//!
//! ```text
//! STEAM_ACCOUNT=bot01 STEAM_PASSWORD=hunter2 \
//! SHARED_SECRET=base64SharedSecret== \                 # optional — mobile 2FA
//! PROXY_URL=socks5://user:pass@host:1080 \              # optional
//! RESOLVE_VANITY=gabelogannewell \                     # optional — vanity → SteamID
//! ADD_FRIEND=76561198000000000 \                       # optional — send a request
//! REMOVE_FRIEND=76561198000000000 \                    # optional — remove
//!     cargo run --example 09_friends
//! ```
//!
//! Keyless, all over the CM session: the friend list is captured from the
//! `CMsgClientFriendsList` Steam pushes right after login, each friend's name
//! comes from [`persona`], and the vanity lookup hits the community XML view.

use std::env;

use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::session::{spawn_session, SessionConfig};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::{friends, persona};

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

    // Vanity → SteamID is keyless and needs no session.
    if let Ok(vanity) = env::var("RESOLVE_VANITY") {
        match persona::resolve_vanity_url(&vanity, proxy.as_ref()).await? {
            Some(id) => println!(
                "vanity '{vanity}' → SteamID {id} ({})",
                persona::profile_url(id)
            ),
            None => println!("vanity '{vanity}' → no such custom URL"),
        }
    }

    // Sign in and bring up a session.
    let mut signin = SignIn::with_password(account.clone(), password);
    if let Some(secret) = shared_secret {
        signin = signin.shared_secret(secret);
    }
    if let Some(p) = &proxy {
        signin = signin.proxy(p.clone());
    }
    let refresh_token = match signin.execute().await? {
        SignInOutcome::Success { refresh_token, .. } => refresh_token,
        other => {
            eprintln!("✗ sign-in did not succeed: {other:?}");
            std::process::exit(1);
        }
    };
    let (handle, join) = spawn_session(SessionConfig {
        account_name: account,
        refresh_token,
        proxy,
    })
    .await?;

    // Capture the friends list FIRST — Steam pushes it once right after login.
    let friends = friends::request_friends_list(&handle).await?;
    println!("\n{} friends:", friends.len());
    for friend in friends.iter().take(25) {
        // Resolve each friend's display name (best-effort).
        let name = match persona::request_player_summary(&handle, friend.steam_id).await {
            Ok(s) => s.persona_name,
            Err(_) => "<name unavailable>".to_string(),
        };
        println!("  {} {:?} — {name}", friend.steam_id, friend.relationship);
    }
    if friends.len() > 25 {
        println!("  … and {} more", friends.len() - 25);
    }

    // Optional mutations (off by default to avoid accidental friend spam).
    if let Ok(id) = env::var("ADD_FRIEND") {
        let added = friends::add_friend(&handle, id.parse()?).await?;
        println!(
            "\n+ friend request sent to {} ({})",
            added.steam_id, added.persona_name
        );
    }
    if let Ok(id) = env::var("REMOVE_FRIEND") {
        friends::remove_friend(&handle, id.parse()?).await?;
        println!("- removed friend {id}");
    }

    handle.logoff().await?;
    let _ = join.await;
    Ok(())
}
