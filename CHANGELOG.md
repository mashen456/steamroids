# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While in `0.x.y`, **any minor version may break the API**.

## [Unreleased]

> **Public API breaks in this cycle**, all detailed below: `add_friend_by_name`
> is gone (*Removed*); `SignIn::with_refresh_token` makes no network call and
> never yields an `access_token` (*Changed*); `SessionState::Disconnected` /
> `Authenticating` are gone and `SessionState` is not `#[non_exhaustive]`, so an
> exhaustive downstream `match` breaks (*Removed*); `SessionHandle::request` now
> times out and turns a non-OK reply `EResult` into `Error::Remote` where it
> used to hand back an empty response; `SessionHandle::notify` can now return
> `Err`; `CmConnection::run` / `send_heartbeat` are gone (*Removed*);
> `persona::resolve_vanity_url` / `persona::fetch_avatar` each gained a
> `rate_limiter` parameter (*Changed*).

### Added

- **Friends expansion (`friends`)**: the module grew from "list / add / remove"
  to 16 public functions, all keyless over the CM session:
  - *Nicknames*: `set_nickname` / `clear_nickname` (job-correlated
    `CMsgClientSetPlayerNickname` to the AM) and `request_nicknames`, plus the
    `Nickname` type. Local to the account: only you see them.
  - *Visibility*: `hide_friend` / `unhide_friend` (`CMsgClientHideFriend`).
    Fire-and-forget; the effect shows up in a later friends list.
  - *Chat*: `send_message` and `send_typing` (`CMsgClientFriendMsg`), plus
    `ChatEntryType`. Also fire-and-forget: `Ok` means the frame was written, not
    that Steam accepted or delivered it.
  - *Friend groups* (the custom tags in the friends list):
    `create_friends_group` / `delete_friends_group` / `rename_friends_group`,
    `add_friends_to_group` / `remove_friends_from_group`, and
    `request_friends_groups_list`, plus the `FriendsGroup` / `NewFriendsGroup`
    types. The add/remove wire messages carry a single `SteamID`, so those two
    loop one job-correlated request per id and short-circuit on the first
    rejection.
  - Covered by the opt-in live test `tests/live_friends.rs`.
- **Post-login snapshot cache (`session`)**: Steam pushes the friends list
  (`767`), friends-groups list (`5553`) and nickname list (`5587`) exactly once
  per logon. The driver now caches the first body it sees per emsg (a later
  incremental delta never overwrites it) and clears the cache on every relogon,
  since Steam re-pushes. Read it with `SessionHandle::cached_snapshot(emsg)`.
  `friends::request_friends_list` / `request_nicknames` /
  `request_friends_groups_list` subscribe first and then read the cache, so they
  no longer have to be called the instant `spawn_session` returns to catch the
  push.
- **Caller-shared rate limiting (`ratelimit`)**: `ratelimit::RateLimiter` paces
  calls to at most one `acquire()` per interval (`with_interval`, or
  `per_minute` for a requests-per-60s shorthand), shared across callers via
  `Arc`. Steam rate-limits by exit IP, not by account, so a fleet running one
  proxy per account needs one limiter per proxy exit rather than a single
  process-wide one; the limiter itself takes no view on sharing granularity,
  the caller does. A zero interval disables pacing outright. Wired in, as
  optional, on every outbound HTTP path that takes a proxy: the
  `SignIn::rate_limiter` builder (paces all four `WebAPI` steps of a password
  sign-in, `BeginAuthSessionViaCredentials` through `PollAuthSessionStatus`),
  the `WebSession::with_rate_limiter` builder (paces `WebSession::get`), and a
  new `rate_limiter` parameter on `persona::resolve_vanity_url` /
  `persona::fetch_avatar`. With no limiter attached, `acquire` is never
  called: no sleep, no lock traffic, unchanged from before this landed. CM
  server discovery (`session::discover_cm_servers`) is deliberately not
  paced: the session layer already backs off on transient logon failures
  there.
- **CS2 medals**: `cs2::PlayerProfile::medals` (displayed medal/coin item
  definition indexes, in display order) and `cs2::PlayerProfile::featured_medal`
  (the showcased one). Resolve either through the econ items manifest.
- **Avatar sizes**: `persona::AvatarSize` (32 / 64 / 184 px) and
  `persona::avatar_url_sized`; `persona::avatar_url` is now the `Full` case of
  it.
- **Token persistence example** — `11_persist_login` shows the bring-your-own-
  storage pattern: get the refresh token (`RefreshToken::expose`), persist it in
  your own store (DB / Redis / file), and reuse it by handing it to
  `spawn_session` — no password / 2FA / `WebAPI` round-trip. The library stays
  storage-agnostic; implement the `TokenStore` trait over your store for
  automatic load / try / save via `SignIn::execute_with_store`.
- **Avatar download** — `persona::fetch_avatar(url, proxy)` fetches the raw JPEG
  bytes of an avatar from its CDN URL (proxy-aware, keyless). Example
  `10_account_dump` saves it to a file.
- **Proxy pool / dead-proxy detection (`pool`)** — `pool::ProxyPool` holds a set
  of proxies, tracks per-proxy consecutive connect failures, hands out healthy
  ones round-robin (`acquire`), and surfaces dead exits (`healthy`, `statuses`)
  so a fleet can rotate them out. Passive: feed it outcomes via `report_success`
  / `report_failure`. Configurable `HealthPolicy`.
- **tracing observability** — structured `tracing` spans + events across the
  session and GC wire boundaries: a per-session span (`account`, `steam_id`) with
  events for CM discovery, connect attempts, logon, reconnect/backoff, server-side
  logoffs (with the `EResult`), and heartbeats; plus a per-GC span (`appid`) with
  attach / welcome / hello-retry / reconnect-re-announce events. Filter via
  `RUST_LOG` (e.g. `steamroids=debug`).
- **Benchmarks** — a `criterion` suite (`benches/codec.rs`) over the framing hot
  path: `codec` encode / encode_raw / decode / try_decode and the GC envelope
  wrap / unwrap. Run with `cargo bench --bench codec`.
- `Error::LogonRetryable(String)` for a *transient* CM logon rejection
  (`NoConnection`, `Busy`, `Timeout`, `ServiceUnavailable`, `TryAnotherCM`,
  `RateLimitExceeded`). `CmConnection::logon` / `establish` and `spawn_session`'s
  initial connect used to report these as `Error::AuthRejected`, which made a
  busy CM indistinguishable from a dead account.
- `SessionHandle::request_with_timeout(emsg, req, timeout)` for a request budget
  other than the default.
- `GameCoordinator::request_matching(.., accept)`: a `request` variant that keeps
  waiting until a predicate accepts the decoded reply, so an empty or foreign GC
  reply cannot satisfy the wrong request.
- `friends::AddedFriend::eresult`: `1` (`OK`) for a newly sent or accepted
  request, `29` (`DuplicateRequest`) when the relationship already existed.
- `friends::ChatEntryType::raw` / `from_raw`.
- `SignInOutcome::GuardCodeRejected` for `EResult` 88 (`TwoFactorCodeMismatch`):
  Steam saw a code and refused it, which re-prompting cannot fix.
- `test-seam` cargo feature, exposing `SessionHandle::for_test` /
  `GameCoordinator::for_test` (and `session::driver::Command`) to out-of-crate
  tests. Off by default, so the seam is not compiled into a normal build.

### Changed

- **The refresh-token sign-in flow no longer touches the `WebAPI`.** This crate
  issues `SteamClient`-platform tokens and Steam won't redeem those over the
  plain `WebAPI`: `GenerateAccessTokenForApp` answers `AccessDenied`, and that
  exchange has to ride an authenticated CM session. `SignIn::with_refresh_token`
  therefore validates the token locally (JWT shape, `SteamID`, `exp`) and hands
  it back for `spawn_session` to present in `CMsgClientLogon`. Consequences for
  callers: the flow makes **no** network call, so `.proxy(..)` on it is a no-op;
  `SignInOutcome::Success::access_token` is always `None`; and
  `SignInOutcome::TokenRejected` now means "expired", never "revoked". A revoked
  token looks valid here and is only rejected later, by the CM, as
  `Error::AuthRejected` out of `spawn_session`, which is the signal a fleet must
  treat as "discard the stored token and re-run the password flow".
- **Single-allocation framing**: `codec::encode` and `encode_raw` now encode the
  protobuf header (and body) straight into one pre-sized buffer instead of via
  intermediate `Vec`s: `encode` drops from 3 allocations to 1, `encode_raw` and
  the GC envelope from 2 to 1. `encode_raw` is now generic over the header type,
  so the CM codec and the GC envelope share the one allocation-free path.
- `SessionHandle::request` now fails with `Error::Timeout` when Steam does not
  answer inside a default budget, and with `Error::Remote` when the reply
  *header* carries a non-OK `EResult`. A Steam-side failure previously decoded
  as a default/empty response and read as success.
- `SessionHandle::notify` awaits the actual socket write instead of returning as
  soon as the command is queued, so it can now return `Err`.
- **Read-idle watchdog**: the driver reconnects when no inbound frame of any
  kind, control or application, has arrived for several heartbeat intervals
  (with a floor, in case Steam hands out a very short interval). Steam never
  acks the CM-level heartbeat, so a dead-but-open exit (the classic
  rotating-proxy failure) has no other symptom.
- `GameCoordinator::request` awaits the GC welcome inside its own deadline, and
  decodes the reply inside the deadline window. A request against a GC that never
  welcomes us now fails with `Error::Timeout("GC welcome")` and writes nothing.
- Dropping the last `GameCoordinator` clone now stops its pump.
- `cs2::request_player_profile` discards an empty or foreign `PlayersProfile`
  instead of returning it, so `PlayerProfile::account_id` is always the requested
  account. An unknown account now fails with `Error::Timeout` after 15s.
- `persona::request_profile_info` returns `Error::Remote` on a non-OK `EResult`
  (including an absent one, whose proto2 default is `2` = `Fail`) instead of an
  all-`None` `ProfileInfo`.
- `persona::request_player_summary` no longer builds a summary out of an
  unrelated unsolicited push that happens to mention the same `SteamID`; a
  partial answer now runs into the existing 10s `Error::Timeout`.
- `persona::resolve_vanity_url` returns `Error::Network` for a non-success HTTP
  status (429, 5xx, …) instead of `Ok(None)`. `Ok(None)` now means only "no such
  vanity URL".
- `persona::resolve_vanity_url` / `persona::fetch_avatar` each take a new
  `rate_limiter: Option<&ratelimit::RateLimiter>` parameter, so a caller pacing
  `WebSession::get` through a limiter can pace these `steamcommunity.com`
  requests through the same one, since they leave via the same proxy exit and
  count against the same Steam-side limit. `None` behaves exactly as before.
- `friends::add_friend` treats only `EResult` `1` and `29` as success; every
  other result is `Error::Remote`, even when Steam still identified the target.
- `friends::create_friends_group` errors when an OK response omits `groupid`
  instead of reporting group id `0`.
- `friends::request_nicknames` / `request_friends_groups_list` skip a push
  carrying `removal` / `bremoval`, which is an incremental delta rather than the
  post-login snapshot.
- `ProxyConfig::host` stores an IPv6 literal without the URL's square brackets
  (`socks5://[::1]:1080` yields `::1`). `ProxyConfig::parse` now always honours a
  port written in the URL (`http://host:80` is 80, not 8080), keeps
  username-only and password-only userinfo as credentials, and percent-decodes
  userinfo as UTF-8 rather than Latin-1.
- `transport::connect_ws` bounds the proxy branch at 45s
  (`Error::Timeout("proxy connect")`); the HTTP-CONNECT path gives up after 64
  response header lines instead of reading unbounded.
- Auth: `EResult` 87 (`AccountLoginDeniedThrottle`) maps to
  `SignInOutcome::RateLimited` instead of falling through to
  `Error::AuthRejected`; `SignIn::execute_with_store` falls through to the
  password flow when a stored token fails to decode; `poll_for_token` uses a
  wall-clock 120s budget rather than 24 attempts, so a large Steam-supplied poll
  interval no longer stretches the flow to ~23 minutes.

### Fixed

- **Session teardown frees its callers.** Every exit from the driver's connected
  loop (a transport drop, a server-side logoff, a `logoff()` / dropped-handles
  shutdown, or a fatal reconnect) now fails the in-flight requests instead of
  holding them until the driver task itself drops. The goodbye `Close` write is
  bounded too: a half-open proxy exit could leave the flush pending forever,
  stranding the `JoinHandle` and every `SessionState` watcher behind it.
- **Coalesced `Multi` batches are bounded**: the driver refuses a batch that
  declares or inflates past 8 MiB, or that nests more than 4 deep. A crafted or
  corrupt `Multi` could previously drive unbounded allocation and recursion.
- **JWT parsing** rejects anything that is not exactly 3 non-empty dot-separated
  segments, instead of parsing on a best-effort basis. Reachable through
  `SignIn::with_refresh_token`.
- `SignInOutcome::RateLimited::retry_hint` is documented as this crate's own
  fixed 60s default, not a value Steam supplied. The value is unchanged.
- **Doc drift in `README`, `examples/`, `protos/` and `ROADMAP`.** The `README`
  no longer advertises `friends::add_friend_by_name` (removed API; this was the
  one doc bug that broke user code on paste), no longer claims "zero-allocation
  hot paths"
  (the framing floor is one allocation), "deterministic state machines"
  (`SessionState` is an observability projection, not a state machine), or "no
  leaked protobuf types" (`prost::Message` is a public bound on `codec`,
  `SessionHandle::request` / `notify`, and `GameCoordinator::send`; it is the
  *feature* modules that stay protobuf-free). The `README` layout tree now lists
  `pool.rs` and `http.rs` and describes `friends` as what it is. `examples/`
  documents `07` to `11` instead of promising an `06`, and no longer claims the
  refresh-token example calls `GenerateAccessTokenForApp`. `protos/` lists all
  18 vendored files and stops calling the CS2 set future work. `ROADMAP` marks
  `v0.1.0` shipped, ticks the `v0.4.x` items that landed (benchmarks,
  dead-proxy detection, wire-boundary tracing spans, the allocation audit), and
  drops the reference to a `codec::frame` that never existed. The GC relay is
  named correctly throughout: the message is `CMsgGCClient`, carried as the
  `k_EMsgClientToGC` / `k_EMsgClientFromGC` `EMsg`s.

### Removed

- **`friends::add_friend_by_name`** (shipped in `0.3.0`). Steam account names
  are not unique, and `CMsgClientAddFriend` with `accountname_or_email_to_add`
  resolves an ambiguous match, so the wrong account could be befriended. A
  `SteamID64` is now the only supported target for `friends::add_friend`;
  resolve a vanity URL with `persona::resolve_vanity_url` first if that's all
  you have.
- `SessionState::Disconnected` and `SessionState::Authenticating`. Neither was
  ever emitted by any code path. `SessionState` is not `#[non_exhaustive]`, so a
  downstream exhaustive `match` (and serde payloads naming either variant)
  breaks.
- `CmConnection::run` and `CmConnection::send_heartbeat`, both callerless. The
  driver's own loop is the supported path.

## [0.3.0] - 2026-06-07

The Game Coordinator milestone: a generic GC layer with CS2 as the first
consumer, plus session-level profile/friends features and rotating-proxy
resilience.

### Added

- **Game Coordinator layer (`gc`)** — app-agnostic GC plumbing: `gc::wrap` /
  `gc::unwrap` frame messages into a `CMsgGCClient` relay envelope (using the
  GC's own `CMsgProtoBufHeader`), sent as `k_EMsgClientToGC` and received as
  `k_EMsgClientFromGC`, and `gc::GameCoordinator` rides on a
  `SessionHandle` to announce the app (`CMsgClientGamesPlayed`), do the
  `ClientHello` → `ClientWelcome` handshake, and correlate replies. Re-announces
  the app automatically when the session reconnects.
- **CS2 helpers (`cs2`)** — `cs2::attach` + `cs2::request_player_profile` return
  an idiomatic `cs2::PlayerProfile` (account id, level, current XP, competitive
  rank/wins) with no protobuf types at the boundary, plus `cs2::APP_ID` and
  `cs2::account_id_from_steam_id`. Example `07_scan_one_profile` and the live
  `cs2_profile_scan` test pull a real level + XP through the GC.
- **Profile details (`persona`)** — `persona::request_player_summary` pulls a
  player's persona name, avatar URL, online status, and current game over the CM
  session (no `WebAPI` key) via `CMsgClientRequestFriendData` →
  `CMsgClientPersonaState`; `persona::request_profile_info` adds the public
  fields (real name, location, summary, account age). `persona::profile_url` /
  `avatar_url` build the community URLs. Example `08_profile_details`.
- **Vanity URL resolution** — `persona::resolve_vanity_url` maps a custom URL
  name back to a `SteamID` keyless via the community XML view. `persona::profile_url`
  is documented as the always-valid canonical form (Steam redirects it to the
  vanity); the reverse pretty form needs the `WebAPI` `GetPlayerSummaries`.
- **Friends (`friends`)** — `friends::request_friends_list` captures the
  post-login `CMsgClientFriendsList` (with `FriendRelationship`),
  `friends::add_friend` sends requests (job-correlated `CMsgClientAddFriend`),
  and `friends::remove_friend` removes / declines. Example `09_friends`.
  (`0.3.0` also shipped an `add_friend_by_name`; it has since been removed, see
  `[Unreleased]`.)
- `Error::Remote` for "Steam processed the request but returned a non-OK
  `EResult`" (e.g. a rejected `AddFriend`).
- Vendored CS2 Game Coordinator protos (`protos/csgo/`) compiled into a separate
  `proto::gc` module to avoid the package-less name clashes with the Steam set
  (`CMsgProtoBufHeader`, `CMsgClientHello`).

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

### Fixed

- **CS2 GC logon** — the GC `ClientHello` now carries the app's protocol version.
  CS2's Game Coordinator rejected a version-less hello with a fatal logon error
  (`ClientLogonFatalError`), so the welcome never arrived; `cs2::attach` supplies
  `cs2::GC_HELLO_VERSION`. The pump also re-sends the hello until welcomed (the
  GC ignores the first one right after launch).

## [0.2.0] - 2026-06-06

The live-session milestone: hold a real, self-healing CM connection, plus auth
hardening for fleet use.

### Added

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

[Unreleased]: https://github.com/mashen456/steamroids/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/mashen456/steamroids/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/mashen456/steamroids/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mashen456/steamroids/compare/v0.0.1...v0.1.0
[0.0.1]: https://github.com/mashen456/steamroids/releases/tag/v0.0.1
