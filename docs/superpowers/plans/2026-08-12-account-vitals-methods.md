# Account Vitals Methods Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Five independent library methods a caller invokes externally: Steam-synced TOTP, wallet balance, license list plus a GC-sourced CS2 Prime check, CS2 penalty state with expiry and acknowledgement, and mobile confirmations.

**Architecture:** These are **methods, not services.** No background tasks, no timers, no polling loops, no hidden state. Each is a function the caller invokes when it wants an answer. Where Steam pushes data unprompted (wallet, licenses), the driver caches the latest push and the method reads the cache, matching how `cached_snapshot` already works for the post-login friends snapshot.

**Tech Stack:** Rust 1.86, the existing `SessionHandle`, `WebSession` and `gcpd` modules.

## Global Constraints

- Rust edition 2021, rust-version 1.86.
- `#![forbid(unsafe_code)]`. clippy `all` + `pedantic` are `warn`, `missing_docs` is `warn`. CI runs clippy `-D warnings` and rustdoc `-D warnings`.
- **Add no new dependencies.**
- **`reqwest` cannot be named in `#[cfg(test)]` code or doctests.** It is fine in non-test implementation code.
- Comments CAVEMAN-MINIMAL: terse lowercase fragments, no prose, no articles. Rustdoc is the exception, proper prose.
- **No em-dashes anywhere**, including commit messages. `src/lib.rs`'s module list has pre-existing ones; new bullets use a colon and existing ones are not touched.
- **Never hardcode a protocol constant you cannot verify.** This crate shipped `TryAnotherCM` as 42 when it is 48 for exactly that reason. EMsg values come from `protos/steam/enums_clientserver.proto`. Anything not derivable from the repo must be verified against a live account or an external source, and the source cited in a comment.
- TDD: write the failing test, run it and SEE it fail, then implement.
- Run `cargo fmt` before committing.

## Verified starting facts

- `k_EMsgClientLicenseList = 780` and `k_EMsgClientWalletInfoUpdate = 5528` are both in `protos/steam/enums_clientserver.proto`. `CMsgClientLicenseList` (with its nested `License`) is at `protos/steam/steammessages_clientserver.proto:167`, `CMsgClientWalletInfoUpdate` at `:313`. Neither is handled by the crate today.
- `ITwoFactorService/QueryTime` is a plain JSON WebAPI endpoint, no protobuf involved.
- `SessionHandle::cached_snapshot(emsg)` already exists for caching a post-login push, and `POST_LOGIN_SNAPSHOT_EMSGS` in `src/session/driver.rs` is the list it caches. Tasks 2 and 3 extend that list rather than inventing a new mechanism.

---

### Task 1: Steam-synced TOTP

Steam validates a Guard code against **its** clock. Local clock drift produces a code Steam rejects, and the failure is indistinguishable from a wrong shared secret. This bit us in practice: a live run failed with `GuardCodeRejected` and the cause was ambiguous.

**Files:**
- Modify: `src/auth/totp.rs`
- Modify: `src/auth/webapi.rs` if a JSON (non-protobuf) call helper is needed there

**Interfaces:**
- Produces: `pub async fn query_server_time_offset(proxy: Option<&ProxyConfig>) -> Result<i64>` returning `server_time - local_time` in seconds, and `pub fn generate_with_offset(shared_secret: &[u8], offset_secs: i64) -> Result<String>`.

Keep the existing `generate` exactly as it is, delegating to `generate_with_offset(secret, 0)`. Callers who do not care keep working unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn offset_shifts_the_time_step() {
    // 30s step: a +30s offset must produce the next step's code
    let secret = [0u8; 20];
    let now = generate_with_offset(&secret, 0).unwrap();
    let plus_one_step = generate_with_offset(&secret, 30).unwrap();
    assert_ne!(now, plus_one_step);
}

#[test]
fn a_zero_offset_matches_the_plain_generator() {
    let secret = [0u8; 20];
    assert_eq!(
        generate_with_offset(&secret, 0).unwrap(),
        generate(&secret).unwrap()
    );
}

#[test]
fn a_negative_offset_does_not_underflow() {
    // absurd negative offset must error or clamp, never panic
    let secret = [0u8; 20];
    let _ = generate_with_offset(&secret, i64::MIN);
}
```

Adapt the exact signatures to whatever `generate` currently takes; read `src/auth/totp.rs` first.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib totp::`

- [ ] **Step 3: Implement**

`generate_with_offset` adds `offset_secs` to the current unix time before computing the step, using saturating arithmetic so an absurd offset cannot underflow.

`query_server_time_offset` does `POST https://api.steampowered.com/ITwoFactorService/QueryTime/v0001/` with an empty body, through `crate::http::client(proxy)`. The response is JSON shaped `{"response":{"server_time":"1234567890", ...}}`, and **`server_time` is a JSON string, not a number**. Parse it, subtract local unix time, return the difference. Use `serde_json`, already a dependency.

Rustdoc must say this is a network call the caller makes occasionally (the offset is stable), not something to call per code generation.

- [ ] **Step 4: Run to verify they pass, then verify live**

Run: `cargo test --lib totp::`

Then confirm the real endpoint's shape rather than trusting this plan. A throwaway `curl -s -X POST https://api.steampowered.com/ITwoFactorService/QueryTime/v0001/` is enough. **If the response shape differs from the description above, trust what you observe and say so in your report.**

- [ ] **Step 5: Verify and commit**

Full gate, then:

```bash
git commit -m "feat(auth): sync TOTP against Steam's clock

Steam validates a Guard code against its own clock, so local drift yields a
code Steam rejects, and the failure looks identical to a wrong shared
secret. A live run hit exactly that ambiguity.

query_server_time_offset returns the delta; generate_with_offset applies
it. generate is unchanged and delegates with a zero offset."
```

---

### Task 2: Wallet balance

Steam pushes `CMsgClientWalletInfoUpdate` (EMsg 5528) unprompted after logon and on balance changes. No request message exists, so the method reads the most recent push.

**Files:**
- Modify: `src/session/driver.rs` (add 5528 to the cached-snapshot emsgs)
- Create or modify: wherever the public accessor belongs, likely a new `src/wallet.rs` or an addition to an existing module. Pick one and justify it in your report.

**Interfaces:**
- Produces:
  ```rust
  pub struct Wallet {
      pub balance_cents: i64,
      pub pending_cents: i64,
      pub currency: i32,
  }
  pub fn wallet(session: &SessionHandle) -> Option<Wallet>;
  ```

`Option` because the push may not have arrived yet, or the account may have no wallet. Do NOT block waiting for it; this is a method, not a subscription.

- [ ] **Step 1: Write the failing test**

Use the `SessionHandle::for_test` seam. Feed a synthesized `CMsgClientWalletInfoUpdate` through the broadcast channel, then assert `wallet()` reports it. Read `tests/session_handlers.rs` for the established pattern.

Also test that `wallet()` returns `None` before any push.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement**

Add 5528 to `POST_LOGIN_SNAPSHOT_EMSGS`. **Read that constant's doc comment first**: it is documented as first-wins caching for one-shot post-login snapshots. Wallet updates repeatedly, so first-wins is wrong here. Either extend the cache to last-wins for this emsg, or use a separate slot, and explain your choice. Do not silently break the friends-list snapshot semantics, which depend on first-wins.

Check the real field names and numbers in `CMsgClientWalletInfoUpdate` at `protos/steam/steammessages_clientserver.proto:313` rather than assuming the struct above matches; adjust the public struct to what the proto actually carries.

- [ ] **Step 4: Run to verify it passes**

- [ ] **Step 5: Verify and commit**

---

### Task 3: License list, and Prime from the GC SO cache

**Files:**
- Modify: `src/session/driver.rs`
- Modify: the module chosen in Task 2, plus `src/cs2.rs` for the Prime helper

**Interfaces:**
- Produces:
  ```rust
  pub struct License { pub package_id: u32, pub time_created: u32, pub payment_method: i32 }
  pub fn licenses(session: &SessionHandle) -> Option<Vec<License>>;
  pub fn owns_package(session: &SessionHandle, package_id: u32) -> Option<bool>;
  ```
  and in `cs2`: `pub fn has_prime(session: &SessionHandle) -> Option<bool>;`

- [ ] **Step 1: Implement the license list first**

`CMsgClientLicenseList` (EMsg 780) is also a post-login push. Same caching approach as Task 2. Map the proto's nested `License` to the public struct, taking only fields you can justify; check the proto at `protos/steam/steammessages_clientserver.proto:167` for what is actually there.

Test through the `for_test` seam with a synthesized list.

- [ ] **Step 2: Read Prime from the GC, not from licenses**

The plan originally routed Prime through a license check. That was wrong, and the correct source was found in the vendored protos: **`elevated` is Valve's internal name for Prime.**

- `CSOEconGameAccountClient.elevated_state` (`protos/csgo/base_gcmessages.proto:106`, alongside `elevated_timestamp = 15`) is the account's own Prime flag.
- `CSOPersonaDataPublic.elevated_state` (`protos/csgo/cstrike15_gcmessages.proto:1236`) is the public flag, the one that renders the badge beside a name.

Both are SharedObjects, and the GC welcome already delivers them: `CMsgClientWelcome.outofdate_subscribed_caches` (field 3, `protos/csgo/gcsdk_gcmessages.proto`) is a `repeated CMsgSOCacheSubscribed`, each carrying `objects: repeated SubscribedType { type_id, object_data: repeated bytes }`. `CSOEconGameAccountClient` is one of those blobs.

So this becomes a GC change, not a license one. Implement `cs2::has_prime(session) -> Option<bool>` reading the cached welcome's SO cache.

**You must establish the SO `type_id` for `CSOEconGameAccountClient` empirically. Do not guess it.** Write a temporary `#[ignore]`d probe in `tests/live_auth.rs` that logs in, waits for the GC welcome, and dumps each subscribed cache's `type_id` with its blob count and sizes. Identify which type_id's blobs decode as `CSOEconGameAccountClient` with sensible field values. Pin the constant with a comment citing that it was determined live, on which date. Delete the probe before committing.

Do not rely on "try decoding every blob and take whatever parses": protobuf decoding is permissive enough that unrelated blobs can parse into the wrong message without erroring.

If the welcome's SO cache turns out not to contain it on the test account, say so and report what type_ids you did see, rather than inventing a fallback.

- [ ] **Step 3: Keep the license methods anyway**

`licenses()` and `owns_package()` are worth having independently: they answer app-ownership questions generally, and the license list is a post-login push the crate currently discards. Ship them, just do not build Prime on top of them.

- [ ] **Step 4: Verify and commit**

The commit message must record how the SO `type_id` was established, and that `elevated_state` is Prime.

---

### Task 4: CS2 penalty state, expiry and acknowledgement

The largest task. `cs2::request_player_profile` already decodes `CMsgGCCStrike15_v2_MatchmakingGC2ClientHello` but discards the penalty fields.

**Files:**
- Modify: `src/cs2.rs`, `src/gc/coordinator.rs`, `src/gcpd.rs`

**Interfaces:**
- Produces on `PlayerProfile`: `penalty_seconds`, `penalty_reason`, plus a `Penalty` type with helpers. And in `gcpd`, expose the `Acknowledged` column already present in the cooldown table.

- [ ] **Step 1: Add the penalty fields to the profile**

Fields 4 (`penalty_seconds`) and 5 (`penalty_reason`) of the hello message. Verify the numbers against `protos/csgo/cstrike15_gcmessages.proto` before use.

- [ ] **Step 2: Encode the semantics, which are the real value here**

These rules come from a working implementation and are not guessable from the protos. Each needs a unit test:

- **A penalty exists when `penalty_seconds > 0` OR `penalty_reason > 0`.** Seconds alone is wrong.
- **`penalty_seconds == 0` with a reason set means the countdown is finished but the penalty is still on the account.** Two distinct causes: a permanent or VAC-Live conviction that never had a countdown, or a **cooldown that expired and has not been acknowledged**. Steam clears it only once the client acknowledges expiry, so an unacknowledged expired cooldown persists showing no time.
- **`penalty_seconds` is ambiguous**: sometimes a remaining duration, sometimes an absolute unix expiry. Disambiguate by magnitude, treating a value above roughly ten years in seconds as an absolute timestamp.
- **Near-`u32::MAX` seconds means permanent.**
- **Permanent reason codes are 8, 10 and 14.** VAC-Live reasons are 22 and 23, and those routinely ship `penalty_seconds == 0`.

Model this as an enum rather than leaving callers to reimplement the rules, something like `None`, `Permanent { reason }`, `Active { reason, expires_at_unix }`, `ExpiredUnacknowledged { reason }`. Note you cannot distinguish the last two cases from GC data alone; see Step 4.

- [ ] **Step 3: Capture the penalty from the GC welcome, not just the profile response**

Two ordering rules, both load-bearing:

- The GC welcome (`k_EMsgGCClientWelcome`, 4004) carries an embedded serialized `MatchmakingGC2ClientHello` in its `game_data2` field, and that is **frequently the only place a cooldown is reported**: no standalone 9110 arrives. `src/gc/coordinator.rs` already handles 4004 for readiness, so decode `game_data2` there when present.
- The 9128 `PlayersProfile` response **omits the penalty**, so it must never overwrite a penalty captured from a real hello. Thread a flag distinguishing a penalty-bearing source from a profile response, and only let the former set penalty state.

Verify the `game_data2` field number against the vendored protos.

- [ ] **Step 4: Expose `Acknowledged` from GCPD**

The GCPD cooldown table already parsed in `src/gcpd.rs` has three columns: `Competitive Cooldown Expiration`, `Competitive Cooldown Level`, `Acknowledged`. Only the first two are currently surfaced. Add the third to `Cs2Cooldown` as a bool.

This matters because it is what disambiguates Step 2's last two cases: GC gives reason and seconds, GCPD gives the expiry timestamp and whether expiry was acknowledged. Together a caller can tell "expired, awaiting acknowledgement" from "flagged permanently with no countdown". Document that relationship on both types so the pairing is discoverable.

`Cs2Cooldown` is `#[non_exhaustive]`, so adding a field is not breaking.

- [ ] **Step 5: Verify and commit**

Every semantic rule in Step 2 needs a test. Commit message should record where the rules came from and that they are not derivable from the protos.

---

### Task 5: Mobile confirmations

**Files:**
- Create: `src/confirmations.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Confirmation { pub id: String, pub nonce: String, pub kind: i32, pub creator_id: String }
  pub async fn list(web: &WebSession, identity_secret: &[u8], device_id: &str) -> Result<Vec<Confirmation>>;
  pub async fn accept(web: &WebSession, identity_secret: &[u8], device_id: &str, conf: &Confirmation) -> Result<()>;
  ```

- [ ] **Step 1: Write the failing tests for the HMAC key**

The signing scheme is the SDA algorithm: `key = base64(HMAC_SHA1(identity_secret, big_endian_u64(unix_time) ‖ tag_ascii))`, where `tag` is truncated to 32 bytes. Different operations use different tags: `list` for fetching, `accept` for allowing.

Pin it with a known-answer test. **Compute the expected value independently** with a throwaway script (python `hmac`/`hashlib` is fine) rather than trusting this plan, exactly as the TOTP known-answer test was done. Test a zero identity secret at a fixed timestamp for both tags, and assert the two tags produce different keys.

- [ ] **Step 2: Run to verify it fails**

- [ ] **Step 3: Implement the key generation and the two calls**

The crate already has HMAC-SHA1 via the TOTP path; reuse it rather than adding anything.

- List: `GET https://steamcommunity.com/mobileconf/getlist?p={device_id}&a={steamid}&k={key}&t={time}&m=react&tag=list`
- Accept: `GET https://steamcommunity.com/mobileconf/ajaxop?op=allow&p={device_id}&a={steamid}&k={key}&t={time}&m=react&tag=accept&cid={conf_id}&ck={conf_nonce}`

`m=react` identifies the modern confirmation UI and is required. Both go through `WebSession`, so they inherit its cookie and its proxy: do not build a separate client.

Responses are JSON. Verify the actual shape rather than assuming; if you cannot reach a live account, model conservatively and say so.

Note `identity_secret` and `device_id` are **imported from a Steam Desktop Authenticator maFile**, not derived. The crate does not implement authenticator enrolment and this task does not add it. Say so in the rustdoc so a caller is not left hunting for a way to generate them.

- [ ] **Step 4: Verify and commit**

`accept` **mutates account state** by approving a trade or listing. Its rustdoc must say so plainly.

## Out of scope

Authenticator enrolment (`AddAuthenticator` / `FinalizeAddAuthenticator`), trade offers, community inventory, the CS2 econ SO cache, microtransactions and the store price sheet. Each is its own piece of work.
