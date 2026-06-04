# steamroids

> Steam on steroids — a pragmatic, performance-focused Rust client for the Steam Connection Manager and Game Coordinator protocols.

[![CI](https://github.com/mashen456/steamroids/actions/workflows/ci.yml/badge.svg)](https://github.com/mashen456/steamroids/actions/workflows/ci.yml)
[![Audit](https://github.com/mashen456/steamroids/actions/workflows/audit.yml/badge.svg)](https://github.com/mashen456/steamroids/actions/workflows/audit.yml)

**Status:** `0.1.0` — pre-alpha, API will change weekly. Do not depend on this for production yet.

## What this is

A Rust library for talking to Steam from automation tooling. Built for use cases where you need to operate **hundreds to thousands of Steam sessions** simultaneously, reliably, behind rotating proxies. Initial focus is the **CS2 (Counter-Strike 2)** Game Coordinator surface for profile/XP scanning, but the lower layers (transport, auth, session) are GC-agnostic and reusable for any Steam-app workload.

## Why it exists

The Rust Steam ecosystem today is `steam-vent` (capable but pre-1.0, no native proxy support, archived upstream repo) — and not much else. The Node ecosystem (`steam-user`, `globaloffensive`) is mature but Node's single-threaded event loop hits a wall at a few hundred concurrent bot sessions. This library is the Rust-native answer when you've outgrown both.

Design priorities, in order:

1. **Stability under fleet load** — zero-allocation hot paths, deterministic state machines, no panics on bad-network inputs.
2. **Proxy support is first-class** — SOCKS5 and HTTP-CONNECT both, with auth, baked into the transport layer.
3. **Small dependency surface** — no Steam-specific dependencies, only foundational crates (tokio, rustls, prost).
4. **Embedded-friendly API** — clean re-exports, no leaked protobuf types, idiomatic Rust at the boundary.

## What's in 0.1.0

- ✅ **WebAPI sign-in** — password + mobile 2FA (TOTP) or stored refresh token,
  via the `SignIn` builder; returns access + refresh tokens
- ✅ RSA password encryption, Steam-Guard handling, `EResult` → outcome mapping
- ✅ Vendored Steam protobufs + `prost-build` codegen (`crate::proto`)
- ✅ Steam **TOTP** code generation (Steam's HMAC-SHA1 / base-26 variant)
- ✅ Proxy connection layer — SOCKS5, HTTP-CONNECT, and `https://` (TLS-to-proxy),
  all with auth
- ✅ WebSocket+TLS transport (works through proxies)
- ✅ Credential and session-state data types
- ✅ CI: rustfmt, clippy, test, doc, audit
- ❌ **Email Steam Guard** — not supported. Accounts that prompt for an emailed
  code return `SignInOutcome::NeedsEmailGuardCode` and cannot complete sign-in;
  use the mobile authenticator (TOTP shared secret) or an account with Steam
  Guard disabled.
- ❌ Live CM session — `ClientLogon` over WSS + heartbeat (coming in 0.2.0)
- ❌ Game Coordinator / CS2 layer (coming in 0.3.0+)

See [ROADMAP.md](./ROADMAP.md) for the full plan.

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
```

## Using as a dependency

While this is in pre-alpha, pin to a specific commit:

```toml
[dependencies]
steamroids = { git = "ssh://git@github.com/mashen456/steamroids.git", tag = "v0.1.0" }
```

## Layout

```
src/
├── lib.rs               — crate root, re-exports
├── error.rs             — Error enum
├── proto.rs             — generated protobuf types (built from protos/)
├── transport/           — WebSocket + TLS + Proxy
├── auth/                — Credentials, TOTP, RSA, JWT, WebAPI sign-in flow
└── session/             — session state (typestate FSM in 0.2.x)
```

## License

MIT — see [LICENSE](./LICENSE).