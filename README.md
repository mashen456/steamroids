# steamroids

> Steam on steroids — a pragmatic, performance-focused Rust client for the Steam Connection Manager and Game Coordinator protocols.

[![CI](https://github.com/mashen456/steamroids/actions/workflows/ci.yml/badge.svg)](https://github.com/mashen456/steamroids/actions/workflows/ci.yml)
[![Audit](https://github.com/mashen456/steamroids/actions/workflows/audit.yml/badge.svg)](https://github.com/mashen456/steamroids/actions/workflows/audit.yml)

**Status:** `0.3.0` — pre-alpha, with the full **auth → live CM session → Game
Coordinator → CS2** stack. API changes weekly; don't depend on this for
production yet.

## What this is

A Rust library for talking to Steam from automation tooling. Built for use cases where you need to operate **hundreds to thousands of Steam sessions** simultaneously, reliably, behind rotating proxies. Initial focus is the **CS2 (Counter-Strike 2)** Game Coordinator surface for profile/XP scanning, but the lower layers (transport, auth, session) are GC-agnostic and reusable for any Steam-app workload.

## Why it exists

The Rust Steam ecosystem today is `steam-vent` (capable but pre-1.0, no native proxy support, archived upstream repo) — and not much else. The Node ecosystem (`steam-user`, `globaloffensive`) is mature but Node's single-threaded event loop hits a wall at a few hundred concurrent bot sessions. This library is the Rust-native answer when you've outgrown both.

Design priorities, in order:

1. **Stability under fleet load** — zero-allocation hot paths, deterministic state machines, no panics on bad-network inputs.
2. **Proxy support is first-class** — SOCKS5, HTTP-CONNECT, and `https://`, with auth, baked into the transport layer; the session self-heals across flaky **rotating** proxies (skip a bad exit, reconnect onto a fresh one).
3. **Small dependency surface** — no Steam-specific dependencies, only foundational crates (tokio, rustls, prost).
4. **Embedded-friendly API** — clean re-exports, no leaked protobuf types, idiomatic Rust at the boundary.

## What's in (current `main`)

The full client stack works end to end — **auth → live CM session → Game
Coordinator → CS2** — keyless (no Steam Web API key) and proxy-aware throughout.

**Auth & transport**

- ✅ **WebAPI sign-in** — password + mobile 2FA (TOTP) or stored refresh token
  via the `SignIn` builder; RSA password encryption, `EResult` → outcome mapping
- ✅ **Refresh-token persistence** hook (`auth::TokenStore`) — reuse a stored
  token, fall back to the password flow, persist the issued token
- ✅ Steam **TOTP** code generation (HMAC-SHA1 / base-26 variant)
- ✅ **Proxy layer** — SOCKS5, HTTP-CONNECT, and `https://` (TLS-to-proxy), all
  with auth; resilient to flaky **rotating** proxies (per-attempt connect
  timeouts, retry onto fresh exits)
- ✅ WebSocket + TLS transport; secret-redacting `Debug` on credentials/tokens
- ✅ Vendored Steam protobufs + `prost-build` codegen (`crate::proto`)

**Live CM session** (`session`)

- ✅ `spawn_session` — logged-on CM session over WSS (`ClientLogon` + refresh
  token) on a background driver with heartbeat
- ✅ Job-id-multiplexed `request` / `notify` / `subscribe`
- ✅ **Self-healing**: reconnect with backoff on drops, transient server-side
  logoffs (`TryAnotherCM` / `ServiceUnavailable`) reconnect, observable
  `SessionState`, clean logoff
- ✅ `EMsg` frame codec (`codec`)

**Game Coordinator** (`gc`, `cs2`)

- ✅ Generic, app-agnostic GC plumbing (`gc::GameCoordinator`) — launch the app,
  `ClientHello` → `ClientWelcome` handshake, reply correlation, re-announce on
  reconnect
- ✅ **CS2 player-profile scan** → idiomatic `cs2::PlayerProfile` (level, XP,
  competitive rank) via `cs2::attach` + `cs2::request_player_profile`

**Persona / profile / friends** (`persona`, `friends`)

- ✅ `persona::request_player_summary` — name, avatar, online status, current
  game; `request_profile_info` — real name, location, summary, account age
- ✅ `persona::resolve_vanity_url` — custom URL → `SteamID` (keyless); `profile_url`
  / `avatar_url` helpers
- ✅ `friends::request_friends_list`, `add_friend` / `add_friend_by_name`,
  `remove_friend`

**Not supported**

- ❌ **Email Steam Guard** — accounts that prompt for an emailed code return
  `SignInOutcome::NeedsEmailGuardCode` and can't complete sign-in; use the mobile
  authenticator (TOTP) or a Guard-disabled account.
- ❌ **`SteamID` → display vanity URL** — needs the Web API `GetPlayerSummaries`
  (a key); `persona::profile_url`'s `/profiles/{id}` form always works and
  redirects to the vanity. The reverse direction is supported (see above).

See [ROADMAP.md](./ROADMAP.md) for the full plan and [CHANGELOG.md](./CHANGELOG.md)
for what's landed since `0.1.0`.

## Quickstart

```bash
# Run the TOTP generator
SHARED_SECRET="<base64>" cargo run --example 01_totp

# Test a proxy by fetching httpbin through it
PROXY_URL="socks5://user:pass@host:1080" cargo run --example 02_proxy_test

# Connect a WebSocket through a proxy to a public echo server
PROXY_URL="socks5://user:pass@host:1080" cargo run --example 03_ws_echo

# Sign in with password (+ optional 2FA) and print the refresh token
STEAM_ACCOUNT="bot01" STEAM_PASSWORD="hunter2" SHARED_SECRET="<base64>" \
  cargo run --example 04_signin_credentials

# Sign in again from a stored refresh token (skips 2FA)
REFRESH_TOKEN="eyJ..." cargo run --example 05_signin_refresh_token

# Scan a CS2 player's level + XP through the Game Coordinator
STEAM_ACCOUNT="bot01" STEAM_PASSWORD="hunter2" SHARED_SECRET="<base64>" \
  TARGET_STEAMID="76561198000000000" cargo run --example 07_scan_one_profile

# Print profile details (name, avatar, profile URL, status) after login
STEAM_ACCOUNT="bot01" STEAM_PASSWORD="hunter2" cargo run --example 08_profile_details

# List friends (with names), resolve a vanity URL, optionally add/remove
STEAM_ACCOUNT="bot01" STEAM_PASSWORD="hunter2" \
  RESOLVE_VANITY="gabelogannewell" cargo run --example 09_friends
```

## Quick tour (library)

After signing in, bring up a live session and use it — all over one connection,
no Web API key:

```rust
use std::time::Duration;
use steamroids::{cs2, friends, persona};
use steamroids::session::{spawn_session, SessionConfig};

// `refresh_token` comes from `auth::SignIn` (password + 2FA, or a stored token).
let (handle, _join) = spawn_session(SessionConfig {
    account_name: "bot01".into(),
    refresh_token,
    proxy, // Option<ProxyConfig> — SOCKS5 / HTTP / https, rotating-proxy friendly
})
.await?;

// Profile details for any account.
let me = persona::request_player_summary(&handle, handle.steam_id()).await?;
println!("{} — {} — {}", me.persona_name, me.profile_url, me.avatar_url);

// Friends.
for f in friends::request_friends_list(&handle).await? {
    println!("{} {:?}", f.steam_id, f.relationship);
}

// CS2 Game Coordinator: attach, wait for the welcome, scan a profile.
let gc = cs2::attach(handle.clone()).await?;
gc.wait_ready(Duration::from_secs(20)).await?;
let profile = cs2::request_player_profile(&gc, cs2::account_id_from_steam_id(handle.steam_id())).await?;
println!("CS2 level {} ({} XP)", profile.level, profile.current_xp);

handle.logoff().await?;
```

## Using as a dependency

While this is in pre-alpha, pin to a specific commit:

```toml
[dependencies]
steamroids = { git = "ssh://git@github.com/mashen456/steamroids.git", tag = "v0.3.0" }
```

## Layout

```
src/
├── lib.rs               — crate root, re-exports
├── error.rs             — Error enum
├── proto.rs             — generated protobuf types (Steam + GC, built from protos/)
├── codec.rs             — EMsg frame en/decoder (shared by CM + GC)
├── transport/           — WebSocket + TLS + Proxy (SOCKS5 / HTTP / https)
├── auth/                — Credentials, TOTP, RSA, JWT, WebAPI sign-in, TokenStore
├── session/             — live CM session: discovery, connection, driver, state
├── gc/                  — generic Game Coordinator envelope + GameCoordinator
├── cs2.rs               — CS2 consumer of the GC layer (PlayerProfile)
├── persona.rs           — player summary / profile info / vanity URL resolution
└── friends.rs           — friends list, add / remove
```

## License

MIT — see [LICENSE](./LICENSE).