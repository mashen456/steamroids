//! Proxy pooling with dead-proxy detection — a fleet primitive.
//!
//! Running many sessions behind **rotating proxies** means some exits are slow,
//! dead, or banned. [`ProxyPool`] holds a set of proxies, tracks each one's
//! recent connect outcomes, hands out healthy ones (round-robin), and surfaces
//! the dead ones so you can alert or swap them out.
//!
//! It's a passive primitive: it doesn't connect anything itself. You take a
//! [`Lease`], use its [`proxy`](Lease::proxy) for a session, then feed the result
//! back with [`report_success`](ProxyPool::report_success) /
//! [`report_failure`](ProxyPool::report_failure).
//!
//! ```no_run
//! # async fn demo(
//! #     proxies: Vec<steamroids::transport::proxy::ProxyConfig>,
//! #     refresh_token: steamroids::auth::RefreshToken,
//! # ) -> steamroids::Result<()> {
//! use steamroids::pool::ProxyPool;
//! use steamroids::session::{spawn_session, SessionConfig};
//!
//! let mut pool = ProxyPool::new(proxies);
//! // Try proxies until one connects; the pool skips dead exits.
//! while let Some(lease) = pool.acquire() {
//!     match spawn_session(SessionConfig {
//!         account_name: "bot01".into(),
//!         refresh_token: refresh_token.clone(),
//!         proxy: Some(lease.proxy().clone()),
//!     })
//!     .await
//!     {
//!         Ok((handle, _join)) => {
//!             pool.report_success(&lease);
//!             // ... use `handle` ...
//!             break;
//!         }
//!         Err(_) => pool.report_failure(&lease), // exit looked bad — try the next
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use tracing::{info, warn};

use crate::transport::proxy::ProxyConfig;

/// When a proxy is considered dead.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct HealthPolicy {
    /// Consecutive connect failures before a proxy is marked unhealthy. A single
    /// success resets the count.
    pub dead_after: u32,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self { dead_after: 3 }
    }
}

/// Health of a pooled proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyStatus {
    /// Connecting normally (fewer than `dead_after` consecutive failures).
    Healthy,
    /// Marked dead after this many consecutive failures.
    Unhealthy {
        /// Consecutive failures recorded so far.
        consecutive_failures: u32,
    },
}

impl ProxyStatus {
    /// `true` while the proxy is still considered usable.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// A proxy taken from the pool. Clone [`proxy`](Self::proxy) into a
/// `SessionConfig`, then report the outcome to the pool with the same lease.
#[derive(Debug, Clone)]
pub struct Lease {
    index: usize,
    proxy: ProxyConfig,
}

impl Lease {
    /// The leased proxy config (clone it into a `SessionConfig`).
    #[must_use]
    pub fn proxy(&self) -> &ProxyConfig {
        &self.proxy
    }
}

struct Entry {
    proxy: ProxyConfig,
    consecutive_failures: u32,
}

/// A round-robin pool of proxies with per-proxy health tracking.
///
/// `acquire` returns healthy proxies in rotation. When none are healthy it still
/// returns the least-failed one (so dead exits get a chance to recover and the
/// caller keeps probing) — check [`healthy`](Self::healthy) to alert when the
/// pool is exhausted.
pub struct ProxyPool {
    entries: Vec<Entry>,
    policy: HealthPolicy,
    cursor: usize,
}

impl ProxyPool {
    /// Build a pool from `proxies` with the [default policy](HealthPolicy::default).
    #[must_use]
    pub fn new(proxies: impl IntoIterator<Item = ProxyConfig>) -> Self {
        Self::with_policy(proxies, HealthPolicy::default())
    }

    /// Build a pool with an explicit [`HealthPolicy`].
    #[must_use]
    pub fn with_policy(
        proxies: impl IntoIterator<Item = ProxyConfig>,
        policy: HealthPolicy,
    ) -> Self {
        let entries = proxies
            .into_iter()
            .map(|proxy| Entry {
                proxy,
                consecutive_failures: 0,
            })
            .collect();
        Self {
            entries,
            policy,
            cursor: 0,
        }
    }

    /// Total number of proxies in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if the pool holds no proxies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many proxies are currently healthy.
    #[must_use]
    pub fn healthy(&self) -> usize {
        (0..self.entries.len())
            .filter(|&i| self.is_healthy(i))
            .count()
    }

    fn is_healthy(&self, index: usize) -> bool {
        self.entries[index].consecutive_failures < self.policy.dead_after
    }

    /// Take a proxy to try. Prefers healthy proxies in round-robin order; if none
    /// are healthy, returns the least-failed one so the caller keeps probing.
    /// Returns `None` only when the pool is empty.
    pub fn acquire(&mut self) -> Option<Lease> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        // First choice: the next healthy proxy, round-robin from the cursor.
        for step in 0..n {
            let index = (self.cursor + step) % n;
            if self.is_healthy(index) {
                self.cursor = (index + 1) % n;
                return Some(self.lease(index));
            }
        }
        // All dead: hand out the least-failed one so it can recover.
        let index = (0..n)
            .min_by_key(|&i| self.entries[i].consecutive_failures)
            .expect("pool is non-empty");
        self.cursor = (index + 1) % n;
        Some(self.lease(index))
    }

    fn lease(&self, index: usize) -> Lease {
        Lease {
            index,
            proxy: self.entries[index].proxy.clone(),
        }
    }

    /// Record a successful connect for a leased proxy, clearing its failure count.
    pub fn report_success(&mut self, lease: &Lease) {
        let Some(entry) = self.entries.get_mut(lease.index) else {
            return;
        };
        let was_unhealthy = entry.consecutive_failures >= self.policy.dead_after;
        entry.consecutive_failures = 0;
        if was_unhealthy {
            info!(proxy = %entry.proxy.display(), "proxy recovered");
        }
    }

    /// Record a failed connect for a leased proxy. After `dead_after` consecutive
    /// failures the proxy is marked unhealthy and skipped by [`acquire`](Self::acquire).
    pub fn report_failure(&mut self, lease: &Lease) {
        let dead_after = self.policy.dead_after;
        let Some(entry) = self.entries.get_mut(lease.index) else {
            return;
        };
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        // Log exactly on the transition into "dead".
        if entry.consecutive_failures == dead_after {
            warn!(
                proxy = %entry.proxy.display(),
                consecutive_failures = entry.consecutive_failures,
                "proxy marked unhealthy"
            );
        }
    }

    /// The current [`ProxyStatus`] for a leased proxy.
    #[must_use]
    pub fn status(&self, lease: &Lease) -> ProxyStatus {
        self.entries
            .get(lease.index)
            .map_or(ProxyStatus::Healthy, |e| self.status_of(e))
    }

    /// A snapshot of every proxy's status (by display string) for monitoring.
    #[must_use]
    pub fn statuses(&self) -> Vec<(String, ProxyStatus)> {
        self.entries
            .iter()
            .map(|e| (e.proxy.display(), self.status_of(e)))
            .collect()
    }

    fn status_of(&self, entry: &Entry) -> ProxyStatus {
        if entry.consecutive_failures < self.policy.dead_after {
            ProxyStatus::Healthy
        } else {
            ProxyStatus::Unhealthy {
                consecutive_failures: entry.consecutive_failures,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::proxy::ProxyConfig;

    fn pool(n: usize) -> ProxyPool {
        let proxies = (0..n).map(|i| ProxyConfig::parse(&format!("socks5://p{i}:1080")).unwrap());
        ProxyPool::with_policy(proxies, HealthPolicy { dead_after: 2 })
    }

    #[test]
    fn acquire_round_robins_healthy() {
        let mut p = pool(3);
        let a = p.acquire().unwrap();
        let b = p.acquire().unwrap();
        let c = p.acquire().unwrap();
        assert_eq!((a.index, b.index, c.index), (0, 1, 2));
        assert_eq!(p.acquire().unwrap().index, 0, "wraps around");
    }

    #[test]
    fn failures_mark_unhealthy_and_acquire_skips_it() {
        let mut p = pool(2); // dead_after = 2
                             // Kill proxy 0.
        let l0 = Lease {
            index: 0,
            proxy: p.entries[0].proxy.clone(),
        };
        p.report_failure(&l0);
        assert!(p.status(&l0).is_healthy(), "one failure is still healthy");
        p.report_failure(&l0);
        assert_eq!(
            p.status(&l0),
            ProxyStatus::Unhealthy {
                consecutive_failures: 2
            }
        );
        assert_eq!(p.healthy(), 1);

        // Now every acquire should return the healthy proxy 1.
        for _ in 0..4 {
            assert_eq!(p.acquire().unwrap().index, 1);
        }
    }

    #[test]
    fn success_resets_and_recovers() {
        let mut p = pool(1);
        let l = Lease {
            index: 0,
            proxy: p.entries[0].proxy.clone(),
        };
        p.report_failure(&l);
        p.report_failure(&l);
        assert_eq!(p.healthy(), 0);
        p.report_success(&l);
        assert_eq!(p.healthy(), 1);
        assert!(p.status(&l).is_healthy());
    }

    #[test]
    fn all_dead_still_hands_out_least_failed() {
        let mut p = pool(2);
        let l0 = Lease {
            index: 0,
            proxy: p.entries[0].proxy.clone(),
        };
        let l1 = Lease {
            index: 1,
            proxy: p.entries[1].proxy.clone(),
        };
        // Both dead, but proxy 1 has fewer failures.
        for _ in 0..3 {
            p.report_failure(&l0);
        }
        for _ in 0..2 {
            p.report_failure(&l1);
        }
        assert_eq!(p.healthy(), 0);
        // Probing should prefer the least-failed (index 1).
        assert_eq!(p.acquire().unwrap().index, 1);
    }

    #[test]
    fn empty_pool_acquires_nothing() {
        let mut p = ProxyPool::new(std::iter::empty());
        assert!(p.is_empty());
        assert!(p.acquire().is_none());
    }
}
