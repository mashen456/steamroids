//! Generate a Steam 2FA code from a shared secret.
//!
//! ```bash
//! SHARED_SECRET="YourBase64Secret==" cargo run --example 01_totp
//! ```

use steamroids::auth::totp::generate_auth_code;

fn main() {
    let secret = std::env::var("SHARED_SECRET")
        .expect("set SHARED_SECRET to the base64-encoded TOTP secret");

    match generate_auth_code(&secret, None) {
        Ok(code) => println!("{code}"),
        Err(e) => {
            eprintln!("failed to generate code: {e}");
            std::process::exit(1);
        }
    }
}
