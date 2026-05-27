//! # steamroids
//!
//! Rust client for the Steam Connection Manager and (future) CS2 Game Coordinator,
//! built for high-concurrency automation workloads.
//!
//! ## Scope of this version
//!
//! `0.0.x` ships the **transport and crypto foundation**. The actual Steam
//! protocol exchange (login, app-launch, GC) lands in `0.1.x` once the protobuf
//! vendoring is in place. See [ROADMAP](https://github.com/mashen456/steamroids/blob/main/ROADMAP.md).
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
//! - [`auth`] — credentials, TOTP, (later) the Steam auth flow
//! - [`session`] — session state types and (later) the typestate FSM

#![doc(html_root_url = "https://docs.rs/steamroids/0.0.1")]

pub mod auth;
pub mod error;
pub mod session;
pub mod transport;

pub use error::Error;

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
