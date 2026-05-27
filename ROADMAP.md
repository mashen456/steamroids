# Roadmap

The shape of `steamroids` over the next versions. Targets are intentions, not promises.

## v0.0.x — Foundations (current)

Transport, crypto, scaffolding. No actual Steam protocol exchange yet. Goal: every example runs, every test passes, every CI check is green.

- [x] WebSocket + TLS transport
- [x] SOCKS5 + HTTP-CONNECT proxy support, with auth
- [x] Steam TOTP (HMAC-SHA1 + base-26 alphabet)
- [x] `Credentials`, `SessionState`, `Error` types
- [x] CI (fmt, clippy, test, doc, audit)
- [ ] Vendored `.proto` files from SteamKit2
- [ ] `prost-build` integration in `build.rs`

## v0.1.x — Login + Session

The first version that talks to Steam. After this milestone, the lib can log a bot in, hold a session open, and log it out cleanly.

- [ ] EMsg-tagged frame codec (envelope + protobuf payload)
- [ ] `CAuthentication_BeginAuthSessionViaCredentials` flow
- [ ] `CAuthentication_PollAuthSessionStatus` flow
- [ ] `CAuthentication_AccessToken_GenerateForApp` (refresh token issuance)
- [ ] `CMsgClientLogon` / `CMsgClientLogonResponse`
- [ ] `CMsgClientHeartBeat` loop
- [ ] Reconnect & backoff
- [ ] Typestate `Session<Disconnected | LoggingOn | LoggedOn | LoggedOff>`
- [ ] Refresh-token persistence hook (caller-supplied trait)

**Acceptance:** `examples/04_login_password.rs` and `examples/05_login_refresh.rs` log a real Steam bot in and back out cleanly.

## v0.2.x — Game Coordinator

Adds CS2 GC capability. With this version, downstream code can ask the GC for player profiles.

- [ ] `CMsgClientGamesPlayed` (launch CS2 GC client)
- [ ] GC envelope (`CMsgClientFromGC` / `CMsgClientToGC`) en/de
- [ ] GC welcome + ranking sync
- [ ] `CMsgGCCStrike15_v2_ClientRequestPlayersProfile`
- [ ] `CMsgGCCStrike15_v2_PlayersProfile` parsing
- [ ] `PlayerProfile` idiomatic Rust type, no protobuf leakage

**Acceptance:** `examples/06_scan_one_profile.rs` returns a real player's level + XP.

## v0.3.x — Hardening

Stable enough that downstream services (`tracker/`) can build on it without breaking weekly.

- [ ] Soak test: 10 bots, 1 hour stable
- [ ] Bad-IP detection hook
- [ ] Connection pooling primitive (optional)
- [ ] `tracing` spans on every wire boundary
- [ ] Benchmark suite (`criterion`)
- [ ] API documentation freeze proposal

## v1.0.0 — API Frozen

Semver guarantees kick in. Breaking changes require a major bump.

- [ ] Full rustdoc coverage
- [ ] Migration guide from `steam-vent` / `steam-user`
- [ ] Public-API surface review
- [ ] Decision on open-sourcing

## Beyond

Possible future scopes — not committed:

- Additional Steam apps (Dota 2 GC, TF2 GC)
- Steam Web API client (lightweight, no `reqwest` dependency)
- Inventory + market operations
- Trade offer support
