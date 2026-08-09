//! Live Connection Manager session: discovery, connection, driver, state.
//!
//! - [`discover_cm_servers`]: the current [`CmServer`] endpoints to dial.
//! - [`CmConnection`]: one CM socket with the [`codec`](crate::codec) framing
//!   on top; `logon` turns it into a [`LoggedOn`] session.
//! - [`spawn_session`] is the usual entry point: it moves a logged-on connection
//!   onto a background task that heartbeats, multiplexes `request` / `notify` /
//!   `subscribe` by job id, and reconnects on transport failure, handing back a
//!   cloneable [`SessionHandle`].
//! - [`SessionState`]: the session's lifecycle as an outside observer sees it.

pub mod connection;
pub mod discovery;
pub mod driver;
pub mod state;

pub use connection::{CmConnection, LoggedOn};
pub use discovery::{discover_cm_servers, CmServer};
pub use driver::{spawn_session, SessionConfig, SessionHandle};
pub use state::SessionState;
