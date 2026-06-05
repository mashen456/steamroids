# Roadmap

The shape of `steamroids` over the next versions. Targets are intentions, not
promises. While in `0.x`, **any minor version may break the API**.

## Strategy

Build the **generic Steam client first**, add the Game Coordinator (CS2) as a
consumer on top. The layering is deliberate:

```
            CS2 profile scan          ← downstream goal (consumer of the GC layer)
        ┌──────────────────────┐
        │  Game Coordinator     │      v0.3.x  — generic GC envelope + CS2 messages
        ├──────────────────────┤
        │  CM session lifecycle │      v0.2.x  — EMsg codec, ClientLogon, heartbeat
        ├──────────────────────┤
        │  WebAPI auth (tokens) │      v0.1.x  — DONE, now hardening
        ├──────────────────────┤
        │  transport + crypto   │      v0.0.x  — DONE
        └──────────────────────┘
```

Each layer is GC-agnostic and reusable. CS2 is the first GC we wire up, not the
only one the lower layers can serve.

---

## v0.0.x — Foundations ✅ done

Transport + crypto. Everything Steam-protocol-agnostic.

- [x] WebSocket + TLS transport (`transport::websocket`)
- [x] SOCKS5 + HTTP-CONNECT proxy support, with auth (`transport::proxy`)
- [x] Steam TOTP — HMAC-SHA1 + base-26 alphabet (`auth::totp`)
- [x] `Credentials`, `SessionState`, `Error` types
- [x] Vendored `.proto` files from SteamTracking + `prost-build` in `build.rs`
- [x] CI (fmt, clippy, test, doc, weekly audit)

## v0.1.0 — Cut the auth release 🎯 next, mechanical

The WebAPI auth flow already shipped in code (`auth::signin`) but the crate
metadata still claims `0.0.1` with "no login flow". Close that gap and tag a
real release. **No new features — just make the repo tell the truth.**

- [ ] Bump `Cargo.toml`, `lib.rs` (`html_root_url`), `README` status to `0.1.0`
- [ ] `CHANGELOG.md` `[0.1.0]` entry: RSA password flow, mobile 2FA, refresh-token
      flow, `SignIn` builder, WebAPI client, proto vendoring
- [ ] Update `README` "What's in" + lib.rs crate docs to reflect that auth works
- [ ] Decide on the unused `Error::NotImplemented` sentinel (keep for 0.2.x CM
      stubs, or drop now)
- [ ] Tag `v0.1.0`

**Acceptance:** `cargo build && cargo test && cargo doc` green; README, CHANGELOG,
and `Cargo.toml` version all agree; no doc claims a feature the code lacks.

## v0.1.x — Auth hardening 🎯 current focus

Make the existing WebAPI auth production-grade before building the CM layer on
top of it. This is where fleet operators actually get burned.

- [ ] **Refresh-token persistence hook** — caller-supplied trait
      (`TokenStore`: load/save by account) so `SignIn` can transparently reuse
      and refresh tokens. The roadmap's original 0.1.x promise, still open.
- [ ] **https:// proxy support** (TLS-to-proxy) — currently rejected in both
      `transport::proxy` and `auth::webapi`. Many commercial proxy providers
      only expose an `https://` frontend; blocking it blocks real fleets.
- [ ] **Real integration tests** — opt-in (env-gated) tests that hit Steam with
      a throwaway account through a proxy. Today every auth test is offline.
- [ ] **Email-Guard completion** — `NeedsEmailGuardCode` is surfaced but there's
      no `UpdateAuthSessionWithSteamGuardCode` path for an email code; add a way
      to feed the code back in (resume a pending session).
- [ ] **Poll-loop review** — `POLL_MAX_ATTEMPTS`/interval interplay; confirm the
      120s budget message matches actual worst-case timing.
- [ ] **Secret hygiene** — `Debug` redaction for `PasswordCredentials` /
      `RefreshToken` (they currently derive `Debug` and print secrets).
- [ ] **Replace hand-rolled `percent_decode`** in `proxy.rs` with a vetted path,
      or document why the minimal version is sufficient.
- [ ] `tracing` spans on every WebAPI call boundary with structured fields.

**Acceptance:** a bot logs in via password+2FA, persists its refresh token, and a
second run logs in from the stored token only — all through an `https://` proxy,
verified by an integration test.

## v0.2.x — CM session lifecycle

The first version that holds a live Steam connection. This is the **largest
remaining block** and the prerequisite for any Game Coordinator work. The old
roadmap folded this into 0.1.x; it's really its own milestone.

> **Note (from surveying [SteamHelper-rs](https://github.com/saskenuba/SteamHelper-rs)):**
> because we connect over **WSS**, TLS already encrypts the link, so we **skip**
> the classic `ChannelEncryptRequest/Response/Result` + AES session-key
> handshake and the `VT01` packet magic entirely — those only apply to the raw
> TCP CM path. Each WS binary frame is exactly one message. That removes the
> hardest crypto chunk of this milestone. We also already vendor the `EMsg`
> enum (`enums_clientserver.proto`), so no SteamLanguage codegen is needed.

- [x] **EMsg-tagged frame codec** — envelope (`EMsg` | proto-bit + `hdr_len` +
      `CMsgProtoBufHeader` + body), encode/decode, in [`crate::codec`]. This is
      the piece that wires `transport` to `proto`. ✅ landed
- [x] **CM server discovery** — `ISteamDirectory/GetCMListForConnect` over the
      shared HTTP client (proxy-aware), parsed into `CmServer`s that yield
      `wss://…/cmsocket/` URLs. In [`crate::session::discover_cm_servers`]. ✅ landed
- [ ] **`CMsgClientLogon` / `CMsgClientLogonResponse`** — log in over the CM WSS
      connection using the access token from the auth layer.
- [ ] **`CMsgClientHeartBeat` loop** — keep the session alive.
- [ ] **Multiplexed request/response** — match responses to in-flight requests
      by job ID; concurrent message handling.
- [ ] **Reconnect & backoff** — survive CM eviction / network blips.
- [ ] **Typestate `Session<Disconnected | LoggingOn | LoggedOn | LoggedOff>`** —
      compile-time prevention of "request before logged on"; the observability
      `SessionState` enum becomes its projection.
- [ ] **Clean logoff** — `CMsgClientLogOff` and graceful socket teardown.

**Acceptance:** `examples/06_cm_session.rs` logs a bot in over the CM, holds the
session open through several heartbeats, survives one forced reconnect, and logs
off cleanly.

## v0.3.x — Game Coordinator + CS2

Generic GC plumbing, then CS2 as the first consumer.

- [ ] **`CMsgClientGamesPlayed`** — launch the app (730) so the GC routes to us.
- [ ] **Generic GC envelope** — `CMsgClientFromGC` / `CMsgClientToGC` en/decode,
      app-agnostic, with GC job-ID correlation.
- [ ] **GC welcome + connection-status handling** — wait for GC readiness before
      issuing requests.
- [ ] **CS2 messages** — `CMsgGCCStrike15_v2_ClientRequestPlayersProfile` /
      `...PlayersProfile`, vendored CS2 protos added to `build.rs`.
- [ ] **`PlayerProfile` idiomatic Rust type** — no protobuf leakage at the API
      boundary.

**Acceptance:** `examples/07_scan_one_profile.rs` returns a real player's level
+ XP via the GC.

## v0.4.x — Fleet hardening

Stable enough for downstream services to depend on without weekly breakage.

- [ ] **Soak test:** 10+ bots, 1 hour, zero panics, stable memory.
- [ ] **Bad-IP / dead-proxy detection hook** — surface a proxy as unhealthy.
- [ ] **Connection-pooling primitive** (optional) for many sessions per process.
- [ ] **`tracing` spans on every wire boundary** (CM + GC, not just WebAPI).
- [ ] **Benchmark suite** (`criterion`) on the hot paths (codec en/decode).
- [ ] **Zero-allocation audit** of the framing hot path (a stated design goal).

**Acceptance:** soak test green in CI (or a documented manual run), benchmarks
tracked, no `unwrap`/`panic` reachable from bad-network input.

## v1.0.0 — API frozen

Semver guarantees kick in. Breaking changes require a major bump.

- [ ] Full rustdoc coverage (`missing_docs` already `warn`).
- [ ] Public-API surface review — no leaked `prost`/`reqwest`/`tungstenite` types.
- [ ] Migration guide from `steam-vent` / `steam-user`.
- [ ] Decision on open-sourcing.

## Beyond — not committed

- Additional GCs (Dota 2, TF2) — should be cheap once the generic GC layer exists.
- Lightweight Steam Web API client (more endpoints, no `reqwest` if feasible).
- Inventory / market operations.
- Trade-offer support.
