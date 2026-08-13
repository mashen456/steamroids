# Client-Side Rate Limiting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a caller pace outbound Steam requests so a fleet stops tripping Steam's rate limiter, instead of only reacting after it has already been throttled.

**Architecture:** A single `RateLimiter` type the caller constructs and shares behind an `Arc`. The library never decides sharing granularity, because Steam limits by **exit IP** and a per-account proxy deployment has many exits: a process-wide limiter would throttle nine accounts to the throughput of one. Attaching a limiter is opt-in; omitting it preserves today's behaviour exactly.

**Tech Stack:** Rust 1.86, tokio (already a dependency, with `test-util` in dev-dependencies for time control).

## Why this is needed, measured not assumed

Running the live suite tripped Steam's limiter after **three logins of one account in about 35 seconds**, and the tests soft-skipped. That is the reactive path. Nothing in the crate currently paces requests to avoid reaching that point.

Note also that `SignInOutcome::RateLimited`'s `retry_hint` is a **hardcoded 60 seconds** (`src/auth/webapi.rs`), not a value Steam sent. Reactive backoff is therefore guessing, which is a further argument for pacing proactively.

## Scope

**In:** the WebAPI sign-in flow, and `WebSession` fetches.

**Out:** CM connections and discovery. The session layer already backs off on transient logon failures, and CM logons are not the observed pinch point. Do not add a limiter there.

## Global Constraints

- Rust edition 2021, rust-version 1.86.
- `#![forbid(unsafe_code)]`. `clippy::all` and `clippy::pedantic` are `warn`, `missing_docs` is `warn`. CI runs clippy `-D warnings` and rustdoc `-D warnings`.
- **Add no new dependencies.** tokio is already present.
- **`reqwest` cannot be named in `#[cfg(test)]` code or doctests.**
- Comments CAVEMAN-MINIMAL: terse lowercase fragments, no prose, no articles. Rustdoc is the exception, proper prose matching the crate's voice.
- **No em-dashes anywhere**, including commit messages. `src/lib.rs`'s module list has pre-existing ones; a new bullet uses a colon and the existing ones are not touched.
- **Backwards compatible.** A caller that attaches no limiter must see byte-identical behaviour, with no added latency and no sleeps.
- TDD: write the failing test, run it and SEE it fail, then implement.
- Run `cargo fmt` before committing.

---

### Task 1: The `RateLimiter` type

Pure and self-contained. All the logic and all the tests live here.

**Files:**
- Create: `src/ratelimit.rs`
- Modify: `src/lib.rs` (add `pub mod ratelimit;` plus a module-list bullet using a colon)

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub struct RateLimiter { /* private */ }
  impl RateLimiter {
      pub fn with_interval(interval: Duration) -> Self;
      pub fn per_minute(requests: u32) -> Self;
      pub async fn acquire(&self);
  }
  ```

- [ ] **Step 1: Write the failing tests**

These use `tokio`'s paused clock, so they are deterministic and instant. Put them in `src/ratelimit.rs`'s `#[cfg(test)] mod tests`.

```rust
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn first_acquire_is_immediate() {
    let limiter = RateLimiter::with_interval(Duration::from_secs(5));
    let start = Instant::now();
    limiter.acquire().await;
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn sequential_acquires_are_spaced_by_the_interval() {
    let limiter = RateLimiter::with_interval(Duration::from_secs(5));
    let start = Instant::now();
    limiter.acquire().await;
    limiter.acquire().await;
    limiter.acquire().await;
    // first is free, the next two each wait one interval
    assert_eq!(start.elapsed(), Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn a_shared_limiter_paces_concurrent_callers() {
    let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(2)));
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let l = Arc::clone(&limiter);
        tasks.push(tokio::spawn(async move { l.acquire().await }));
    }
    for t in tasks {
        t.await.expect("task");
    }
    // 4 slots at 2s: 0, 2, 4, 6
    assert_eq!(start.elapsed(), Duration::from_secs(6));
}

#[tokio::test(start_paused = true)]
async fn an_idle_limiter_does_not_bank_credit() {
    let limiter = RateLimiter::with_interval(Duration::from_secs(5));
    limiter.acquire().await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    // long gap: the next call is immediate, not owed extra slots
    let start = Instant::now();
    limiter.acquire().await;
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[test]
fn per_minute_divides_the_minute() {
    assert_eq!(
        RateLimiter::per_minute(12).interval(),
        Duration::from_secs(5)
    );
}

#[test]
fn per_minute_zero_does_not_divide_by_zero() {
    // degenerate input must not panic; treat as "no pacing"
    assert_eq!(RateLimiter::per_minute(0).interval(), Duration::ZERO);
}
```

`interval()` is a `#[cfg(test)]`-only or private accessor if you prefer; do not add it to the public API unless you think it earns its place, and say which you chose.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib ratelimit::`

Expected: FAIL, module does not exist.

- [ ] **Step 3: Implement**

Required behaviour:

- Hold the next free slot as a `std::sync::Mutex<Option<tokio::time::Instant>>`. Use `std::sync::Mutex`, not tokio's: the lock must NOT be held across the await.
- `acquire()` computes its slot under the lock, advances the stored slot, releases the lock, and only then sleeps:

```rust
pub async fn acquire(&self) {
    if self.interval.is_zero() {
        return;
    }
    let at = {
        let mut next = self.next.lock().expect("rate limiter mutex poisoned");
        let now = Instant::now();
        // an idle gap does not bank credit: never schedule in the past
        let at = next.map_or(now, |n| if n > now { n } else { now });
        *next = Some(at + self.interval);
        at
    };
    tokio::time::sleep_until(at).await;
}
```

That ordering is what makes concurrent callers queue correctly: each takes a distinct slot immediately, then waits for it. Holding the lock across the sleep would serialize the waits and give the wrong spacing.

- `with_interval` stores the interval as given. `per_minute(n)` computes `60s / n`, and **must not divide by zero**: `per_minute(0)` yields a zero interval, which `acquire` treats as no pacing.
- `tokio::time::Instant`, not `std::time::Instant`, so the paused-clock tests work.

Rustdoc on the type must explain the sharing model explicitly: Steam limits by exit IP, so the caller decides granularity by choosing which limiter goes where, and one limiter per proxy exit is the usual arrangement for a fleet. Include a short `no_run` example showing an `Arc` shared between two sign-ins on the same proxy.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib ratelimit::`

Expected: all PASS.

- [ ] **Step 5: Verify and commit**

Run: `cargo test --all-features`, `cargo clippy --all-targets --all-features`, `cargo test --doc`, `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features`.

```bash
cargo fmt
git add src/ratelimit.rs src/lib.rs
git commit -m "feat(ratelimit): add a shareable request pacer

Steam limits by exit IP, and a per-account proxy deployment has many
exits, so a process-wide limiter would throttle a fleet to the throughput
of one account. The caller therefore owns the limiter and decides sharing
granularity, typically one per proxy exit.

acquire takes its slot under the lock and sleeps outside it, so concurrent
callers queue rather than serializing their waits. An idle gap does not
bank credit."
```

---

### Task 2: Attach it to sign-in and web fetches

**Files:**
- Modify: `src/auth/signin.rs`, `src/auth/webapi.rs`, `src/web.rs`

**Interfaces:**
- Consumes: `RateLimiter` from Task 1.
- Produces: `SignIn::rate_limiter(Arc<RateLimiter>)` and `WebSession::with_rate_limiter(Arc<RateLimiter>)`, both builder-style.

- [ ] **Step 1: Write the failing tests**

The pacing itself is already proven in Task 1, so these test the **wiring**: that an attached limiter is actually consulted, and that omitting one changes nothing.

For `WebSession`, the existing local-listener pattern in `src/web.rs` (see `get_surfaces_a_non_success_status`) gives a real request to pace:

```rust
#[tokio::test(start_paused = true)]
async fn get_waits_on_an_attached_rate_limiter() {
    // burn the first slot so the next acquire must wait
    let limiter = Arc::new(RateLimiter::with_interval(Duration::from_secs(30)));
    limiter.acquire().await;

    let web = WebSession {
        steam_id: 1,
        access_token: "tok".to_string(),
        session_id: None,
        proxy: None,
        rate_limiter: Some(Arc::clone(&limiter)),
    };

    let start = tokio::time::Instant::now();
    // connect refused, but the limiter must be consulted BEFORE the request
    let _ = web.get("http://127.0.0.1:1/").await;
    assert!(start.elapsed() >= Duration::from_secs(30));
}

#[tokio::test(start_paused = true)]
async fn get_without_a_limiter_does_not_wait() {
    let web = WebSession {
        steam_id: 1,
        access_token: "tok".to_string(),
        session_id: None,
        proxy: None,
        rate_limiter: None,
    };
    let start = tokio::time::Instant::now();
    let _ = web.get("http://127.0.0.1:1/").await;
    assert!(start.elapsed() < Duration::from_secs(1));
}
```

Add an equivalent pair for `SignIn` if you can reach its call path from a test without credentials. If you cannot, say so plainly in your report rather than writing a test that does not exercise the wiring; the `WebSession` pair plus Task 1's tests are then the coverage.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::`

Expected: FAIL, no `rate_limiter` field.

- [ ] **Step 3: Wire `WebSession`**

Add `rate_limiter: Option<Arc<RateLimiter>>` to the struct, defaulted to `None` in `request_web_token`. Add:

```rust
/// Pace requests from this session through a shared limiter.
///
/// Steam rate-limits by exit IP, so share one limiter across every session
/// that leaves through the same proxy.
#[must_use]
pub fn with_rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self { ... }
```

In `get`, `acquire()` before issuing the request. Include the field in the redacting `Debug` impl as a bool or presence marker rather than trying to format the limiter.

- [ ] **Step 4: Wire the sign-in flow**

`src/auth/webapi.rs:109`'s `call` helper is the single choke point: all four WebAPI requests go through it (`GetPasswordRSAPublicKey`, `BeginAuthSessionViaCredentials`, `UpdateAuthSessionWithSteamGuardCode`, `PollAuthSessionStatus`, at `src/auth/signin.rs:494`, `:546`, `:617`, `:670`). Acquire once at the top of `call`, so pacing covers every request rather than only the first of a multi-step login.

Thread an `Option<Arc<RateLimiter>>` from `SignIn` into whatever type owns `call`, and add the builder method on `SignIn`:

```rust
/// Pace this sign-in's `WebAPI` requests through a shared limiter.
#[must_use]
pub fn rate_limiter(mut self, limiter: Arc<RateLimiter>) -> Self { ... }
```

Note `SignIn::execute` consumes the builder, and the live tests rebuild it per attempt via `execute_with_retry`. An `Arc` clone per attempt is correct and intended: the shared budget must survive rebuilding.

- [ ] **Step 5: Verify the no-limiter path is unchanged**

Confirm by reading and by the `get_without_a_limiter_does_not_wait` test that a `None` limiter adds no sleep and no lock traffic. This is the backwards-compatibility requirement; call it out explicitly in your report.

- [ ] **Step 6: Verify and commit**

Run the full gate: `cargo test --all-features`, `cargo clippy --all-targets --all-features`, `cargo test --doc`, `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features`, `cargo test --test live_auth --no-run`.

```bash
cargo fmt
git add src/auth/signin.rs src/auth/webapi.rs src/web.rs
git commit -m "feat(auth,web): pace sign-in and web fetches through a limiter

Opt-in: without a limiter nothing sleeps and behaviour is unchanged.

Sign-in acquires inside webapi's call helper rather than once per execute,
so all four requests of a multi-step login are paced, not just the first.
SignIn::execute consumes the builder and callers rebuild it per retry, so
the Arc is cloned per attempt and the budget survives.

Live measurement that motivated this: three logins of one account inside
about 35 seconds tripped Steam's limiter."
```

## Out of scope

Automatic retry on `RateLimited` inside the library. The crate surfaces the outcome and the caller decides, which stays true here. Worth revisiting only once `retry_hint` carries something Steam actually sent rather than the current hardcoded 60 seconds.
