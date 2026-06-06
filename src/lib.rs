//! # steamroids
//!
//! Rust client for the Steam Connection Manager and (future) CS2 Game Coordinator,
//! built for high-concurrency automation workloads.
//!
//! ## Scope of this version
//!
//! The full client stack is now in-tree:
//!
//! - **Auth** — log an account in (password + mobile 2FA, or a stored refresh
//!   token) and obtain access / refresh tokens via [`auth::SignIn`].
//! - **CM session** — [`session::spawn_session`] holds a live, self-healing
//!   Connection Manager session over WSS (`ClientLogon`, heartbeat, reconnect,
//!   job-id-multiplexed `request` / `notify` / `subscribe`).
//! - **Game Coordinator** — [`gc::GameCoordinator`] talks to an app's GC on top
//!   of a session; [`cs2`] is the first consumer (CS2 player profiles).
//!
//! See [ROADMAP](https://github.com/mashen456/steamroids/blob/main/ROADMAP.md)
//! for what's still ahead (fleet hardening, more GCs).
//!
//! ## Quick tour
//!
//! ```no_run
//! use steamroids::auth::totp::generate_auth_code;
//!
//! // Generate the Steam 2FA code that the mobile authenticator would show.
//! let code = generate_auth_code("YourBase64SharedSecret==", None).unwrap();
//! println!("{code}");
//! ```
//!
//! ## Module layout
//!
//! - [`transport`] — WebSocket + TLS + proxy plumbing
//! - [`auth`] — credentials, TOTP, and the `WebAPI` sign-in flow ([`auth::SignIn`])
//! - [`codec`] — Steam `EMsg` + protobuf message framing for the CM transport
//! - [`session`] — live CM session lifecycle ([`session::spawn_session`])
//! - [`gc`] — generic Game Coordinator envelope + client ([`gc::GameCoordinator`])
//! - [`cs2`] — CS2 (app 730) helpers built on the GC layer ([`cs2::PlayerProfile`])

#![doc(html_root_url = "https://docs.rs/steamroids/0.1.0")]

pub mod auth;
pub mod codec;
pub mod cs2;
pub mod error;
pub mod gc;
pub mod proto;
pub mod session;
pub mod transport;

// Crate-private: shared reqwest client setup for the WebAPI auth flow and CM
// server discovery.
mod http;

pub use error::Error;

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
