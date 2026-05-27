# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While in `0.x.y`, **any minor version may break the API**.

## [Unreleased]

## [0.0.1] - 2026-05-27

### Added

- Initial crate scaffolding with strict lints (`unsafe_code = forbid`).
- `auth::totp::generate_auth_code` — Steam-flavored TOTP (HMAC-SHA1, 26-char alphabet, 5-character code).
- `auth::credentials::Credentials` — password / refresh-token variants.
- `transport::proxy::ProxyConfig` — SOCKS5 and HTTP-CONNECT, with URL parsing.
- `transport::websocket::connect_ws` — establishes a WSS connection optionally through a proxy.
- `session::state::SessionState` — placeholder state-machine type (full typestate FSM in 0.0.2).
- `Error` enum covering transport, auth, proxy, and session failure modes.
- GitHub Actions: fmt, clippy, test, doc, weekly audit.
- Examples: TOTP generation, proxy connectivity test, WebSocket echo through proxy.

[Unreleased]: https://github.com/mashen456/steamroids/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/mashen456/steamroids/releases/tag/v0.0.1
