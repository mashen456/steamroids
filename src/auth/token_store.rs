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

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;

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

/// A [`TokenStore`] backed by a single JSON file — the batteries-included store
/// for "save the login, reuse it next time."
///
/// The file is a flat `{ "account": "refresh_token", … }` map, so one file can
/// hold a whole fleet's tokens. Reads/writes are serialized within the process,
/// and a save is read-modify-write so concurrent accounts don't clobber each
/// other. A missing file simply reads as empty (the first login creates it).
///
/// **The file contains credentials** — refresh tokens are bearer secrets. Store
/// it somewhere private and treat it like a password file.
///
/// ```no_run
/// # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use steamroids::auth::{FileTokenStore, SignIn};
///
/// let store = FileTokenStore::new("steam_tokens.json");
/// // First run does password (+2FA) and saves the token; later runs reuse it.
/// let outcome = SignIn::with_password("bot01", "hunter2")
///     .shared_secret("base64SharedSecret==")
///     .execute_with_store(&store)
///     .await?;
/// # let _ = outcome;
/// # Ok(())
/// # }
/// ```
pub struct FileTokenStore {
    path: PathBuf,
    /// Serializes read-modify-write within the process so concurrent saves to the
    /// same file don't lose updates.
    lock: Mutex<()>,
}

impl FileTokenStore {
    /// Use the JSON file at `path` (created on first save).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// Read the account→token map, treating a missing/empty file as empty.
    fn read_map(&self) -> Result<HashMap<String, String>, TokenStoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if s.trim().is_empty() => Ok(HashMap::new()),
            Ok(s) => Ok(serde_json::from_str(&s)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(Box::new(e)),
        }
    }

    /// Write the map back as pretty JSON, creating parent directories as needed.
    fn write_map(&self, map: &HashMap<String, String>) -> Result<(), TokenStoreError> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(map)?)?;
        Ok(())
    }
}

impl TokenStore for FileTokenStore {
    async fn load(&self, account: &str) -> Result<Option<String>, TokenStoreError> {
        // Recover from a poisoned lock — the guarded data (a file path) is fine.
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(self.read_map()?.get(account).cloned())
    }

    async fn save(&self, account: &str, refresh_token: &str) -> Result<(), TokenStoreError> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut map = self.read_map()?;
        map.insert(account.to_owned(), refresh_token.to_owned());
        self.write_map(&map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn file_store_persists_and_keeps_other_accounts() {
        let path = std::env::temp_dir().join(format!("steamroids_fts_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = FileTokenStore::new(&path);

        // Missing file reads as empty.
        assert!(store.load("bot01").await.unwrap().is_none());

        store.save("bot01", "tok-1").await.unwrap();
        store.save("bot02", "tok-2").await.unwrap();
        // A second store over the same file sees the persisted tokens.
        let reopened = FileTokenStore::new(&path);
        assert_eq!(
            reopened.load("bot01").await.unwrap().as_deref(),
            Some("tok-1")
        );
        assert_eq!(
            reopened.load("bot02").await.unwrap().as_deref(),
            Some("tok-2")
        );

        // Saving one account doesn't clobber the other (read-modify-write).
        store.save("bot01", "tok-1-rotated").await.unwrap();
        assert_eq!(
            reopened.load("bot01").await.unwrap().as_deref(),
            Some("tok-1-rotated")
        );
        assert_eq!(
            reopened.load("bot02").await.unwrap().as_deref(),
            Some("tok-2")
        );

        let _ = std::fs::remove_file(&path);
    }
}
