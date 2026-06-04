# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While in `0.x.y`, **any minor version may break the API**.

## [Unreleased]

### Added

- `auth::TokenStore` trait + `SignIn::execute_with_store` for transparent
  refresh-token reuse and persistence (try a stored token, fall back to the
  password flow, persist the issued token).
- `https://` proxy support (TLS-to-proxy) for both the WebAPI auth flow and the
  WebSocket transport, in addition to SOCKS5 and plain HTTP-CONNECT.
- Opt-in live integration tests (`tests/live_auth.rs`) that authenticate against
  real Steam, gated by environment variables / CI secrets.

### Security

- `Debug` for `PasswordCredentials`, `RefreshToken`, `ProxyCredentials`, and
  `SignInOutcome` now redacts secrets (password, shared secret, refresh/access
  tokens, proxy password) so they no longer leak into logs and traces.

### Notes

- Email Steam Guard is explicitly unsupported (`NeedsEmailGuardCode` cannot
  complete sign-in); use mobile 2FA or a Steam-Guard-disabled account.

## [0.1.0] - 2026-06-04

### Added

- `auth::SignIn` — high-level sign-in builder, the public entry point for
  authenticating an account. Two modes: password (+ optional mobile 2FA shared
  secret) and refresh token.
- `auth::SignInOutcome` — `#[non_exhaustive]` result enum distinguishing
  Steam-side rejections (`InvalidCredentials`, `TokenRejected`,
  `NeedsMobileGuardCode`, `NeedsEmailGuardCode`, `RateLimited`) from success.
- WebAPI auth flow against `IAuthenticationService`: `GetPasswordRSAPublicKey`
  → RSA (PKCS#1 v1.5) password encryption → `BeginAuthSessionViaCredentials` →
  `UpdateAuthSessionWithSteamGuardCode` (mobile TOTP) → `PollAuthSessionStatus`,
  plus the refresh-token flow via `GenerateAccessTokenForApp`.
- Vendored Steam `.proto` files (SteamTracking pin in `protos/COMMIT.txt`) and
  `prost-build` integration in `build.rs`; generated types re-exported from
  `crate::proto`.
- `Error::Network` / `Error::AuthRejected` / `Error::AuthRateLimited` to
  separate "never reached Steam" from "Steam said no".

### Changed

- Crate metadata, docs, and README now reflect that authentication works.

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

[Unreleased]: https://github.com/mashen456/steamroids/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mashen456/steamroids/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/mashen456/steamroids/releases/tag/v0.0.1
