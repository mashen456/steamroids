//! Sign in by scanning a QR code with the Steam mobile app.
//!
//! ```text
//! PROXY_URL=socks5://user:pass@host:1080 \      # optional
//! QR_INVERT=1                            \      # optional, see below
//!     cargo run --example 12_signin_qr
//! ```
//!
//! No account name, password, or shared secret needed. Two phases, since a
//! human has to scan in between: `BeginAuthSessionViaQR` fetches a challenge
//! URL, then `PollAuthSessionStatus` blocks (same poll loop the password flow
//! uses, bounded to a 120s wall-clock budget) until the phone approves it,
//! declines it, or the budget runs out.
//!
//! The challenge URL is rendered as a scannable QR code right in the
//! terminal, using Unicode half-block characters (the `qrcode` crate, a
//! dev-dependency of this example only, never a dependency of the library
//! itself). The raw URL is printed underneath as a fallback in case the
//! block-character rendering does not come through cleanly.
//!
//! Terminal QR codes need dark modules on a light background to scan
//! reliably, but this example cannot detect whether the terminal it is
//! running in has a dark or light theme, so it guesses dark-background (the
//! common case) and swaps the block/blank mapping accordingly. If the code
//! will not scan, set `QR_INVERT=1` and run again to flip it.
//!
//! The issued refresh token is `SteamClient`-platform, same as the password
//! flow, so it can be handed straight to `spawn_session` (see example 11) or
//! persisted and reused via example 05.
//!
//! `SignIn::with_qr` does not implement challenge-URL rotation; see the
//! `steamroids::auth::signin` module docs for that limitation.

use std::env;

use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;
use steamroids::auth::{SignIn, SignInOutcome};
use steamroids::transport::proxy::ProxyConfig;
use steamroids::Error;

// terminals are usually dark bg, light fg. the renderer's own default paints
// dark modules as filled blocks (fg color, i.e. light) and light modules as
// blank (bg color, i.e. dark) -- backwards. swap so dark modules blank out
// to the dark terminal bg and light modules show as fg-colored blocks.
// QR_INVERT=1 flips back to the crate's raw default, for light-bg terminals.
fn print_qr(url: &str) {
    let invert = env::var("QR_INVERT").is_ok_and(|v| v == "1");
    let (dark, light) = if invert {
        (Dense1x2::Dark, Dense1x2::Light)
    } else {
        (Dense1x2::Light, Dense1x2::Dark)
    };

    match QrCode::new(url.as_bytes()) {
        Ok(code) => {
            let rendered = code
                .render::<Dense1x2>()
                .quiet_zone(true)
                .dark_color(dark)
                .light_color(light)
                .build();
            println!("{rendered}");
            println!("If that does not scan, set QR_INVERT=1 and run again.");
        }
        Err(e) => eprintln!("could not render a qr code for the url: {e}"),
    }
    println!();
    println!("Or paste this URL into a browser on the phone:");
    println!();
    println!("  {url}");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,steamroids=debug".into()),
        )
        .init();

    let proxy_url = env::var("PROXY_URL").ok().filter(|s| !s.is_empty());

    let mut qr = SignIn::with_qr();
    if let Some(url) = proxy_url {
        let proxy = ProxyConfig::parse(&url)?;
        eprintln!("routing through proxy: {}", proxy.host);
        qr = qr.proxy(proxy);
    }

    let session = qr.begin().await?;
    println!("Open the Steam mobile app, tap the QR icon on the sign-in screen,");
    println!("and scan the code below.");
    println!();
    print_qr(session.challenge_url());
    println!();
    println!("Waiting for approval (up to 120s)...");

    match session.poll().await {
        Ok(SignInOutcome::Success {
            steam_id,
            refresh_token,
            access_token,
        }) => {
            println!("logged in");
            println!("  steam_id      : {steam_id}");
            println!("  refresh_token : {}", refresh_token.expose());
            if let Some(at) = access_token {
                println!("  access_token  : {at}");
            }
            println!();
            println!("Persist refresh_token to reuse via example 05 (no QR scan next time).");
        }
        Ok(SignInOutcome::RateLimited { retry_hint }) => {
            eprintln!("rate-limited by Steam (retry after {retry_hint:?})");
            std::process::exit(3);
        }
        // The password-flow outcomes shouldn't occur for QR sign-in, but the
        // enum is #[non_exhaustive] so exhaust it anyway.
        Ok(other) => {
            eprintln!("unexpected outcome for qr sign-in: {other:?}");
            std::process::exit(1);
        }
        Err(Error::Timeout(msg)) => {
            eprintln!("timed out waiting for the qr scan: {msg}");
            eprintln!("nobody scanned it within the poll budget, run again");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("transport / protocol error: {e}");
            std::process::exit(1);
        }
    }

    Ok(())
}
