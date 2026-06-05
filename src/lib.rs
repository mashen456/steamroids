//! # steamroids
//!
//! Rust client for the Steam Connection Manager and (future) CS2 Game Coordinator,
//! built for high-concurrency automation workloads.
//!
//! ## Scope of this version
//!
//! `0.1.x` ships the **transport, crypto, and `WebAPI` authentication** layers:
//! you can log an account in (password + mobile 2FA, or a stored refresh token)
//! and obtain access / refresh tokens — see [`auth::SignIn`]. The live Steam
//! Connection Manager session (`ClientLogon` over WSS, heartbeat) lands in
//! `0.2.x`, and the CS2 Game Coordinator in `0.3.x`. See
//! [ROADMAP](https://github.com/mashen456/steamroids/blob/main/ROADMAP.md).
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
//! - [`session`] — session state types and (later) the typestate FSM

#![doc(html_root_url = "https://docs.rs/steamroids/0.1.0")]

pub mod auth;
pub mod codec;
pub mod error;
pub mod proto;
pub mod session;
pub mod transport;

pub use error::Error;

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
