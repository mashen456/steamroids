//! Session-level state and (later) the typestate FSM.
//!
//! `0.0.x` ships the plain enum that downstream services can read for
//! observability. The full `Session<S>` typestate machine — where `S` is
//! `Disconnected | LoggingOn | LoggedOn | LoggedOff` — lands with the actual
//! login flow in `0.1.x`.

pub mod state;

pub use state::SessionState;
