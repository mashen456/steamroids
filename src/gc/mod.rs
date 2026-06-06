//! Game Coordinator layer — generic, app-agnostic.
//!
//! A Game Coordinator (GC) is a per-app server that Steam relays messages to and
//! from. To talk to one you:
//!
//! 1. tell Steam you're "playing" the app (`CMsgClientGamesPlayed`) so it routes
//!    GC traffic to your session,
//! 2. wrap each GC message in a [`CMsgGcClient`](crate::proto::CMsgGcClient) and
//!    send it as `k_EMsgClientToGC`; replies come back as `k_EMsgClientFromGC`.
//!
//! Inside that envelope the GC payload uses the *same* framing as the CM codec
//! (`msgtype | proto-bit`, header length, header, body) — except the header is
//! the GC's own [`CMsgProtoBufHeader`](crate::proto::gc::CMsgProtoBufHeader),
//! which has its own field layout. [`wrap`] / [`unwrap`] handle that framing;
//! [`GameCoordinator`] rides on a CM [`SessionHandle`](crate::session::SessionHandle)
//! and does the launch, the GC handshake, and reply correlation.
//!
//! This module knows nothing about CS2 — see [`crate::cs2`] for the first
//! consumer.

mod coordinator;
mod envelope;

pub use coordinator::GameCoordinator;
pub use envelope::{unwrap, wrap, GcMessage};

// GC system message types (`EGCBaseClientMsg`), from
// `protos/csgo/gcsystemmsgs.proto`. These are shared by every GC.
/// `k_EMsgGCClientWelcome` — the GC sends this once it's ready for requests.
pub const GC_CLIENT_WELCOME: u32 = 4004;
/// `k_EMsgGCClientHello` — we send this to prompt a [`GC_CLIENT_WELCOME`].
pub const GC_CLIENT_HELLO: u32 = 4006;
/// `k_EMsgGCClientConnectionStatus` — periodic GC reachability updates.
pub const GC_CLIENT_CONNECTION_STATUS: u32 = 4009;
