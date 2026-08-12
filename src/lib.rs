//! # steamroids
//!
//! Rust client for the Steam Connection Manager and the CS2 Game Coordinator,
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
//! - **Persona / profile** — [`persona::request_player_summary`] pulls a
//!   player's name, avatar, status, and current game over the session (no
//!   `WebAPI` key); [`persona::request_profile_info`] adds the public fields,
//!   and [`persona::resolve_vanity_url`] maps a custom URL back to a `SteamID`.
//! - **Friends**: [`friends`] manages the account's social graph over the
//!   session: the friends list ([`friends::request_friends_list`]), add /
//!   remove, per-friend nicknames, friends-list visibility, chat messages, and
//!   friends groups.
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
//! - [`auth`]: credentials, TOTP, the `WebAPI` sign-in flow ([`auth::SignIn`]),
//!   and refresh-token persistence ([`auth::TokenStore`])
//! - [`codec`] — Steam `EMsg` + protobuf message framing for the CM transport
//! - [`session`] — live CM session lifecycle ([`session::spawn_session`])
//! - [`persona`] — player summary, profile info, and vanity-URL resolution
//! - [`friends`]: friends list, add / remove, nicknames, visibility, chat,
//!   groups
//! - [`gc`] — generic Game Coordinator envelope + client ([`gc::GameCoordinator`])
//! - [`gcpd`]: fetch and parse an account's CS2 competitive cooldown from its
//!   GCPD page ([`gcpd::request_cs2_cooldown`])
//! - [`cs2`] — CS2 (app 730) helpers built on the GC layer ([`cs2::PlayerProfile`])
//! - [`pool`] — proxy pooling + dead-proxy detection for fleets ([`pool::ProxyPool`])
//! - [`web`]: mint a `steamcommunity.com` web access token over the session
//!   ([`web::request_web_token`])

#![doc(html_root_url = "https://docs.rs/steamroids/0.3.0")]

pub mod auth;
pub mod codec;
pub mod cs2;
pub mod error;
pub mod friends;
pub mod gc;
pub mod gcpd;
pub mod persona;
pub mod pool;
pub mod proto;
pub mod session;
pub mod transport;
pub mod web;

// Crate-private: shared reqwest client setup for the WebAPI auth flow and CM
// server discovery.
mod http;

pub use error::Error;

/// Convenience `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
