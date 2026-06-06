# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While in `0.x.y`, **any minor version may break the API**.

## [Unreleased]

### Fixed

- **CS2 GC logon** — the GC `ClientHello` now carries the app's protocol version.
  CS2's Game Coordinator rejected a version-less hello with a fatal logon error
  (`ClientLogonFatalError`), so the welcome never arrived; `cs2::attach` supplies
  `cs2::GC_HELLO_VERSION`. The pump also re-sends the hello until welcomed (the
  GC ignores the first one right after launch).

### Changed

- **Proxy resilience / self-heal** — built for flaky *rotating* proxies where
  exits vary in performance:
  - `CmConnection::establish` now bounds each server attempt
    (`CONNECT_ATTEMPT_TIMEOUT`), so a slow/silent exit is abandoned and the next
    attempt routes through a fresh exit instead of stalling.
  - `spawn_session` retries the initial connect with backoff (several fresh
    exits) before giving up.
  - Transient server-side logoffs (`TryAnotherCM`, `ServiceUnavailable`,
    `NoConnection`) now trigger a reconnect instead of ending the session; the
    logoff `EResult` is surfaced in `SessionState::LoggedOff`.
- `GameCoordinator::attach` takes a `hello_version` argument; CS2 callers should
  use `cs2::attach`.

### Added

- **Friends (`friends`)** — `friends::request_friends_list` captures the
  post-login `CMsgClientFriendsList` (with `FriendRelationship`),
  `friends::add_friend` / `add_friend_by_name` send requests (job-correlated
  `CMsgClientAddFriend`), and `friends::remove_friend` removes / declines.
  Example `09_friends`.
- **Vanity URL resolution** — `persona::resolve_vanity_url` maps a custom URL
  name back to a `SteamID` keyless via the community XML view. `persona::profile_url`
  is documented as the always-valid canonical form (Steam redirects it to the
  vanity); the reverse pretty form needs the `WebAPI` `GetPlayerSummaries`.
- **Profile details (`persona`)** — `persona::request_player_summary` pulls a
  player's persona name, avatar URL, online status, and current game over the CM
  session (no `WebAPI` key) via `CMsgClientRequestFriendData` →
  `CMsgClientPersonaState`; `persona::request_profile_info` adds the public
  fields (real name, location, summary, account age). `persona::profile_url` /
  `avatar_url` build the community URLs. Example `08_profile_details`.
- **Game Coordinator layer (`gc`)** — app-agnostic GC plumbing: `gc::wrap` /
  `gc::unwrap` frame messages into the `CMsgClientToGC` / `…FromGC` relay (using
  the GC's own `CMsgProtoBufHeader`), and `gc::GameCoordinator` rides on a
  `SessionHandle` to announce the app (`CMsgClientGamesPlayed`), do the
  `ClientHello` → `ClientWelcome` handshake, and correlate replies. Re-announces
  the app automatically when the session reconnects.
- **CS2 helpers (`cs2`)** — `cs2::request_player_profile` returns an idiomatic
  `cs2::PlayerProfile` (account id, level, current XP, competitive rank/wins)
  with no protobuf types at the boundary, plus `cs2::APP_ID` and
  `cs2::account_id_from_steam_id`. Example `07_scan_one_profile` and the live
  `cs2_profile_scan` test pull a real level + XP through the GC.
- **CM session lifecycle (`session`)** — `session::spawn_session` establishes a
  logged-on CM connection over WSS (`CMsgClientLogon` + refresh token), then runs
  a background driver that heartbeats, multiplexes `request` / `notify` /
  `subscribe` by job id, reconnects with exponential backoff on transport drops,
  and logs off cleanly. State is observable via `SessionHandle::state` /
  `watch_state`. Includes the `codec` frame en/decoder and
  `session::discover_cm_servers`.
- `auth::TokenStore` trait + `SignIn::execute_with_store` for transparent
  refresh-token reuse and persistence (try a stored token, fall back to the
  password flow, persist the issued token).
- `https://` proxy support (TLS-to-proxy) for both the WebAPI auth flow and the
  WebSocket transport, in addition to SOCKS5 and plain HTTP-CONNECT.
- Opt-in live integration tests (`tests/live_auth.rs`) that authenticate against
  real Steam, gated by environment variables / CI secrets.
- Vendored CS2 Game Coordinator protos (`protos/csgo/`) compiled into a separate
  `proto::gc` module to avoid the package-less name clashes with the Steam set
  (`CMsgProtoBufHeader`, `CMsgClientHello`).

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
