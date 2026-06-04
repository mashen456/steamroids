//! Caller-supplied persistence for Steam refresh tokens.
//!
//! Logging in with a password (and 2FA) is expensive and rate-limited; a
//! refresh token lets later logins skip all of that. To make reuse transparent,
//! the caller provides a [`TokenStore`] — a small load/save hook keyed by
//! account name — and drives sign-in through
//! [`SignIn::execute_with_store`](crate::auth::SignIn::execute_with_store).
//!
//! Implement it over whatever backing store fits the deployment: a JSON file
//! for a handful of bots, Redis or Postgres for a fleet. The futures are
//! required to be `Send` so sessions can be driven across worker threads.
//!
//! # Example
//!
//! ```
//! use std::collections::HashMap;
//! use std::sync::Mutex;
//! use steamroids::auth::{TokenStore, TokenStoreError};
//!
//! /// A trivial in-memory store (process-lifetime only).
//! #[derive(Default)]
//! struct MemoryStore(Mutex<HashMap<String, String>>);
//!
//! impl TokenStore for MemoryStore {
//!     async fn load(&self, account: &str) -> Result<Option<String>, TokenStoreError> {
//!         Ok(self.0.lock().unwrap().get(account).cloned())
//!     }
//!     async fn save(&self, account: &str, refresh_token: &str) -> Result<(), TokenStoreError> {
//!         self.0.lock().unwrap().insert(account.to_owned(), refresh_token.to_owned());
//!         Ok(())
//!     }
//! }
//! ```

use std::future::Future;

/// Error type a [`TokenStore`] implementation may return. Boxed so callers can
/// surface any backing-store error (I/O, database, serialization) without this
/// crate having to know about it.
pub type TokenStoreError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Persistence hook for Steam refresh tokens, keyed by account name.
///
/// See the [module docs](self) for the rationale and an example
/// implementation.
pub trait TokenStore {
    /// Load the saved refresh token for `account`, or `None` if there isn't
    /// one yet.
    fn load(
        &self,
        account: &str,
    ) -> impl Future<Output = Result<Option<String>, TokenStoreError>> + Send;

    /// Persist `refresh_token` for `account`, replacing any previous value.
    fn save(
        &self,
        account: &str,
        refresh_token: &str,
    ) -> impl Future<Output = Result<(), TokenStoreError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryStore(Mutex<HashMap<String, String>>);

    impl TokenStore for MemoryStore {
        async fn load(&self, account: &str) -> Result<Option<String>, TokenStoreError> {
            Ok(self.0.lock().unwrap().get(account).cloned())
        }
        async fn save(&self, account: &str, refresh_token: &str) -> Result<(), TokenStoreError> {
            self.0
                .lock()
                .unwrap()
                .insert(account.to_owned(), refresh_token.to_owned());
            Ok(())
        }
    }

    #[tokio::test]
    async fn store_roundtrips() {
        let store = MemoryStore::default();
        assert!(store.load("bot01").await.unwrap().is_none());
        store.save("bot01", "refresh-tok").await.unwrap();
        assert_eq!(
            store.load("bot01").await.unwrap().as_deref(),
            Some("refresh-tok")
        );
    }
}
