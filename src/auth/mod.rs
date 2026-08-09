//! Authentication primitives.
//!
//! - [`credentials`] — input types (password / refresh-token).
//! - [`totp`] — Steam's HMAC-SHA1 mobile-authenticator code derivation.
//! - [`signin`] — the high-level [`SignIn`] builder, public entry point for
//!   logging an account in: the RSA password flow with optional mobile 2FA, or
//!   a previously issued refresh token.
//! - [`token_store`]: the [`TokenStore`] hook plus
//!   [`SignIn::execute_with_store`](signin::SignIn::execute_with_store), for
//!   transparent refresh-token reuse and persistence.

pub mod credentials;
pub mod signin;
pub mod token_store;
pub mod totp;

// Crate-private implementation details for the WebAPI auth flow.
mod jwt;
mod rsa_pw;
mod webapi;

pub use credentials::{Credentials, PasswordCredentials, RefreshToken};
pub use signin::{SignIn, SignInOutcome};
pub use token_store::{TokenStore, TokenStoreError};
