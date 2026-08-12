//! A shareable request pacer.
//!
//! Steam rate-limits by exit IP, not by account. A fleet running many
//! accounts through per-account proxies has many exits, so a single
//! process-wide limiter would throttle every account down to the
//! throughput of one shared exit. [`RateLimiter`] does not decide this for
//! you: the caller owns the limiter and picks the sharing granularity. The
//! usual arrangement for a fleet is one limiter per proxy exit, shared by
//! every session that leaves through that exit.
//!
//! ```no_run
//! # async fn demo() {
//! use std::sync::Arc;
//!
//! use steamroids::ratelimit::RateLimiter;
//!
//! // one limiter per proxy exit, shared by every sign-in through it
//! let limiter = Arc::new(RateLimiter::per_minute(12));
//!
//! // tokio::spawn, not tokio::join!: spawn is the pattern the compiler can
//! // actually enforce !Send-across-await on. see acquire()'s docs.
//! let a = Arc::clone(&limiter);
//! let sign_in_a = tokio::spawn(async move {
//!     a.acquire().await;
//!     // ... sign in account A through this proxy ...
//! });
//!
//! let b = Arc::clone(&limiter);
//! let sign_in_b = tokio::spawn(async move {
//!     b.acquire().await;
//!     // ... sign in account B through the same proxy ...
//! });
//!
//! let _ = tokio::join!(sign_in_a, sign_in_b);
//! # }
//! ```

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

/// Paces calls to at most one per `interval`, shared across callers.
///
/// See the [module docs](self) for the sharing model. There is no `Clone`
/// on purpose: wrap in `Arc` and share the `Arc`, so every caller draws
/// from the same slot sequence.
pub struct RateLimiter {
    interval: Duration,
    // next free slot, none until the first acquire
    // must stay std::sync::Mutex, not tokio's: !send guard is what stops the
    // guard being held across the await in acquire(). no runtime test can
    // catch a regression here, see acquire() below.
    next: Mutex<Option<Instant>>,
}

impl RateLimiter {
    /// Builds a limiter that spaces `acquire` calls by exactly `interval`.
    ///
    /// A zero interval disables pacing: `acquire` then returns immediately
    /// every time.
    #[must_use]
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            next: Mutex::new(None),
        }
    }

    /// Builds a limiter allowing `requests` calls per 60-second window,
    /// spaced evenly.
    ///
    /// `requests == 0` yields a zero interval (no pacing) rather than
    /// dividing by zero.
    #[must_use]
    pub fn per_minute(requests: u32) -> Self {
        let interval = if requests == 0 {
            Duration::ZERO
        } else {
            Duration::from_secs(60) / requests
        };
        Self::with_interval(interval)
    }

    /// Waits until the next free slot, then takes it.
    ///
    /// The first call on a fresh limiter returns immediately. Every call
    /// after that waits until `interval` has passed since the last slot
    /// taken. An idle gap does not bank credit: after a long pause the
    /// next call is immediate, not owed extra slots.
    ///
    /// Internally, the `std::sync::Mutex` guard that computes the next slot
    /// is dropped before the `await` that actually sleeps. That ordering is
    /// load-bearing: a `std::sync::MutexGuard` is `!Send`, so holding one
    /// across an `.await` would make this future `!Send`, and a `!Send`
    /// future fails to compile only where something actually requires
    /// `Send` (`tokio::spawn`, not a bare `.await`), which is why the
    /// [module example](self) spawns each `acquire` call instead of joining
    /// them inline.
    pub async fn acquire(&self) {
        if self.interval.is_zero() {
            return;
        }
        let at = {
            let mut next = self.next.lock().expect("rate limiter mutex poisoned");
            let now = Instant::now();
            // idle gap does not bank credit: never schedule in the past
            let at = next.map_or(now, |n| if n > now { n } else { now });
            *next = Some(at + self.interval);
            at
            // guard drops here, before the await below -- must stay that way
        };
        tokio::time::sleep_until(at).await;
    }

    // test-only: exposes the computed interval without exposing storage shape
    #[cfg(test)]
    fn interval(&self) -> Duration {
        self.interval
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::time::Instant;

    use super::*;

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
}
