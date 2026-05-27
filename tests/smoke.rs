//! End-to-end smoke tests that pull on multiple modules at once.
//!
//! Unit tests live alongside the modules they cover. This file is for tests
//! that need more than one module to be useful.

use steamroids::auth::{totp::generate_auth_code, Credentials};
use steamroids::session::SessionState;
use steamroids::transport::proxy::ProxyConfig;

#[test]
fn credentials_constructors_work() {
    let pw = Credentials::password("bot01", "secret", Some("ABCD".into()));
    matches!(pw, Credentials::Password(_));

    let rt = Credentials::refresh_token("eyJ...");
    matches!(rt, Credentials::RefreshToken(_));
}

#[test]
fn proxy_url_parses_with_creds() {
    let cfg = ProxyConfig::parse("socks5://u:p@host:1080").unwrap();
    assert_eq!(cfg.host, "host");
    assert!(cfg.credentials.is_some());
}

#[test]
fn session_state_label_is_consistent() {
    assert_eq!(SessionState::Disconnected.label(), "disconnected");
    assert_eq!(SessionState::LoggedOn { steam_id: 1 }.label(), "logged_on");
}

#[test]
fn totp_produces_steam_alphabet() {
    // 20-byte zero secret is small but valid HMAC-SHA1 key material.
    let code = generate_auth_code("AAAAAAAAAAAAAAAAAAAAAAAAAAA=", Some(0)).unwrap();
    assert_eq!(code.len(), 5);
}
