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

## v0.1.0 — Cut the auth release ✅ done

Closing the gap between the shipped WebAPI auth flow (`auth::signin`) and the
crate metadata, then tagging a real release. **No new features — just making the
repo tell the truth.**

- [x] Bump `Cargo.toml`, `lib.rs` (`html_root_url`), `README` status to `0.1.0`
- [x] `CHANGELOG.md` `[0.1.0]` entry: RSA password flow, mobile 2FA, refresh-token
      flow, `SignIn` builder, WebAPI client, proto vendoring
- [x] Update `README` "What's in" + lib.rs crate docs to reflect that auth works
- [x] Decide on the unused `Error::NotImplemented` sentinel: **kept**, still the
      "wired but not implemented" signal (examples exit `EX_TEMPFAIL` on it)
- [x] Tag `v0.1.0`

**Acceptance:** `cargo build && cargo test && cargo doc` green; README, CHANGELOG,
and `Cargo.toml` version all agree; no doc claims a feature the code lacks.

## v0.1.x — Auth hardening

Make the existing WebAPI auth production-grade before building the CM layer on
top of it. This is where fleet operators actually get burned.

- [x] **Refresh-token persistence hook** — `auth::TokenStore` +
      `SignIn::execute_with_store`. ✅
- [x] **https:// proxy support** (TLS-to-proxy) — both the WebAPI flow (reqwest)
      and the WebSocket transport. ✅
- [x] **Real integration tests** — opt-in, env-gated `tests/live_auth.rs`
      against real Steam through a proxy; runs in CI's `live` job. ✅
- [x] **Secret hygiene** — `Debug` redaction for credentials, tokens, and proxy
      password. ✅
- [~] **Email-Guard** — intentionally **unsupported**: `NeedsEmailGuardCode` is
      surfaced but can't complete; use mobile 2FA or a Guard-disabled account.
      Documented in the README.
- [x] **Poll-loop review** — the attempt counter is gone; `poll_for_token` runs
      against a wall-clock `POLL_BUDGET` of 120s, which is exactly what the
      timeout message says. ✅
- [x] **Hand-rolled `percent_decode`** in `proxy.rs`: kept, and justified. It
      decodes to bytes then UTF-8 (not Latin-1), is covered by round-trip tests
      over non-ASCII input, and saves a dependency for one small need. ✅
- [x] **Refresh-token flow is offline**: `SteamClient` tokens can't be redeemed
      over the `WebAPI` (`GenerateAccessTokenForApp` answers `AccessDenied`), so
      `with_refresh_token` validates the JWT locally and the CM logon is the only
      thing that can prove a token is live. Documented on `auth::signin`. ✅
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
- [x] **`CMsgClientLogon` / `CMsgClientLogonResponse`** — `CmConnection` in
      [`crate::session::connection`] connects over WSS, sends `ClientLogon` with
      the refresh token, handles `Multi` (incl. gzip) + legacy non-proto
      messages, and returns `LoggedOn` (SteamID, session, heartbeat). Required
      switching the auth flow to `SteamClient` platform tokens. Verified live. ✅
- [x] **`CMsgClientHeartBeat` loop** — `CmConnection::send_heartbeat` +
      `run(interval, on_message)`, which heartbeats on a deadline and dispatches
      incoming messages (cancel-safe recv). Verified live (session alive across
      several intervals). ✅
- [x] **Multiplexed request/response** — background session driver
      ([`crate::session::spawn_session`]) owns the socket on its own task:
      heartbeats, routes replies to in-flight requests by job id, broadcasts the
      rest. Cloneable `SessionHandle` with `request`/`notify`/`subscribe`.
      Verified live (events, heartbeat survival, clean shutdown). ✅
- [x] **Reconnect & backoff** — the driver re-establishes (discover → connect →
      logon) with exponential backoff on transport drops, failing in-flight
      requests so callers retry; gives up only if the token is rejected. Session
      now created from a `SessionConfig` (account + refresh token + proxy) via
      `CmConnection::establish`. ✅
- [x] **Session state** — the handle architecture already makes "request before
      logged on" unrepresentable (no handle exists until logon). Instead of a
      redundant typestate, `SessionHandle` exposes live state via a `watch`
      channel projected onto [`SessionState`]: `state()` (snapshot) and
      `watch_state()` (await transitions: `LoggedOn` → `Connecting` while
      reconnecting → `LoggedOff` / `Failed`). Practical for fleet monitoring. ✅
- [x] **Clean logoff** — `SessionHandle::logoff()` sends `CMsgClientLogOff` and
      the driver closes the socket (WebSocket Close frame), ending in
      `LoggedOff`. ✅

**Acceptance:** met by the live test `cm_logon_over_wss` — signs in, runs the
session through several heartbeats, dispatches events, and logs off cleanly
(state `LoggedOn` → `LoggedOff`), all against real Steam in CI. (A forced-reconnect
live test is still worth adding; reconnect is currently structural.)

## v0.3.x — Game Coordinator + CS2

Generic GC plumbing, then CS2 as the first consumer.

> **Note:** the CS2 protos (`protos/csgo/`) are package-less like the Steam set
> and several names collide (`CMsgProtoBufHeader`, `CMsgClientHello`,
> `ECommunityItemClass`) while being *different* messages — the GC
> `CMsgProtoBufHeader` has its own field layout. They therefore compile into a
> separate [`proto::gc`](crate::proto::gc) module (own `OUT_DIR/gc` output) so
> the flat namespaces can't clash. The GC payload reuses the CM frame layout, so
> [`codec::encode_raw`] (generic over the header type) and the crate-private
> frame splitter are shared, just with the GC header.

- [x] **`CMsgClientGamesPlayed`** — launch the app (730) so the GC routes to us.
      Done in [`gc::GameCoordinator::attach`]. ✅
- [x] **Generic GC envelope** — `CMsgGCClient` en/decode in [`gc::wrap`] /
      [`gc::unwrap`], sent as `k_EMsgClientToGC` and received as
      `k_EMsgClientFromGC`; app-agnostic, decoding the GC routing header.
      `GcMessage::jobid_target` exposes GC job ids (CS2 doesn't populate them, so
      the client correlates by response type instead). ✅
- [x] **GC welcome handling** — `attach` sends a `ClientHello` and the pump
      flags readiness on `ClientWelcome`; [`gc::GameCoordinator::wait_ready`]
      awaits it before requests. The pump re-announces the app on reconnect.
      (`GC_CLIENT_CONNECTION_STATUS` is surfaced as a constant; acting on it is
      future work.) ✅
- [x] **CS2 messages** — `CMsgGCCStrike15_v2_ClientRequestPlayersProfile` /
      `...PlayersProfile` wired through [`cs2::request_player_profile`]; CS2
      protos vendored and compiled via `build.rs`. ✅
- [x] **`PlayerProfile` idiomatic Rust type** — [`cs2::PlayerProfile`] (level,
      XP, competitive rank/wins, displayed + featured medal defindexes) with no
      protobuf leakage at the boundary. ✅

**Acceptance:** met by `examples/07_scan_one_profile.rs` and the live
`cs2_profile_scan` test — sign in, bring up a CM session, attach the CS2 GC,
await its welcome, and request a profile, returning a real level + XP. (Whether a
given CI account has a CS2 license is provisioning, not code, so the live test
soft-skips if the GC never welcomes.)

## v0.4.x — Fleet hardening

Stable enough for downstream services to depend on without weekly breakage.

- [ ] **Soak test:** 10+ bots, 1 hour, zero panics, stable memory.
- [x] **Bad-IP / dead-proxy detection hook** — [`pool::ProxyPool`] counts
      consecutive per-proxy connect failures, hands out healthy exits
      round-robin, and reports dead ones (`healthy`, `statuses`). ✅
- [ ] **Connection-pooling primitive** (optional) for many sessions per process.
      (`pool` pools *proxies*, not sessions.)
- [x] **`tracing` spans on every wire boundary**: a per-session span
      (`account`, `steam_id`) and a per-GC span (`appid`), with events for
      discovery, connect, logon, reconnect/backoff, logoffs, heartbeats, and the
      GC attach / welcome / re-announce. WebAPI boundaries are still bare. ✅
- [x] **Benchmark suite** (`criterion`) on the hot paths: `benches/codec.rs`
      covers `encode` / `encode_raw` / `decode` / `try_decode` and the GC
      envelope wrap / unwrap. `cargo bench --bench codec`. ✅
- [x] **Allocation audit** of the framing hot path: the floor is **one**
      allocation, not zero: a frame is built into a single pre-sized `Vec`
      (`encode` 3 → 1, `encode_raw` and the GC envelope 2 → 1). Returning owned
      bytes means that last allocation stays. ✅

**Acceptance:** soak test green in CI (or a documented manual run), benchmarks
tracked, no `unwrap`/`panic` reachable from bad-network input.

## v1.0.0 — API frozen

Semver guarantees kick in. Breaking changes require a major bump.

- [ ] Full rustdoc coverage (`missing_docs` already `warn`).
- [ ] Public-API surface review — keep `reqwest` / `tungstenite` internal.
      `prost::Message` is deliberately public (the `codec`, `SessionHandle`, and
      `GameCoordinator` generics); the feature modules stay protobuf-free.
- [ ] Migration guide from `steam-vent` / `steam-user`.
- [ ] Decision on open-sourcing.

## Beyond — not committed

- Additional GCs (Dota 2, TF2) — should be cheap once the generic GC layer exists.
- Lightweight Steam Web API client (more endpoints, no `reqwest` if feasible).
- Inventory / market operations.
- Trade-offer support.
