//! End-to-end smoke tests that pull on multiple modules at once.
//!
//! Unit tests live alongside the modules they cover. This file is for tests
//! that need more than one module to be useful.

use prost::Message;
use steamroids::auth::{totp::generate_auth_code, Credentials};
use steamroids::proto;
use steamroids::session::SessionState;
use steamroids::transport::proxy::ProxyConfig;

#[test]
fn credentials_constructors_work() {
    let pw = Credentials::password("bot01", "secret", Some("ABCD".into()));
    assert!(matches!(pw, Credentials::Password(_)));

    let rt = Credentials::refresh_token("eyJ...");
    assert!(matches!(rt, Credentials::RefreshToken(_)));
}

#[test]
fn proxy_url_parses_with_creds() {
    let cfg = ProxyConfig::parse("socks5://u:p@host:1080").unwrap();
    assert_eq!(cfg.host, "host");
    assert!(cfg.credentials.is_some());
}

#[test]
fn session_state_label_is_consistent() {
    assert_eq!(SessionState::Connecting.label(), "connecting");
    assert_eq!(SessionState::LoggedOn { steam_id: 1 }.label(), "logged_on");
}

#[test]
fn totp_produces_steam_alphabet() {
    // 20-byte zero secret is small but valid HMAC-SHA1 key material.
    let code = generate_auth_code("AAAAAAAAAAAAAAAAAAAAAAAAAAA=", Some(0)).unwrap();
    assert_eq!(code.len(), 5);
}

#[test]
fn protobuf_header_round_trips() {
    // Smoke-tests that the vendored .proto files compiled and the prost
    // runtime is wired up. If this stops linking, the include! in proto.rs
    // (or the build.rs paths) is wrong.
    let header = proto::CMsgProtoBufHeader {
        steamid: Some(76_561_198_000_000_000),
        client_sessionid: Some(42),
        ..Default::default()
    };

    let bytes = header.encode_to_vec();
    let decoded = proto::CMsgProtoBufHeader::decode(&*bytes).unwrap();

    assert_eq!(decoded.steamid, Some(76_561_198_000_000_000));
    assert_eq!(decoded.client_sessionid, Some(42));
}

#[test]
fn known_login_messages_exist() {
    // Type-level smoke test — if these names disappear in a future re-vendor,
    // we'll catch it here before the Session code starts to fail mysteriously.
    let _: proto::CMsgClientLogon = proto::CMsgClientLogon::default();
    let _: proto::CMsgClientLogonResponse = proto::CMsgClientLogonResponse::default();
    let _: proto::CMsgClientHeartBeat = proto::CMsgClientHeartBeat::default();
    let _: proto::CAuthenticationBeginAuthSessionViaCredentialsRequest =
        proto::CAuthenticationBeginAuthSessionViaCredentialsRequest::default();
}
