//! Credential types used by the login flow.

use serde::{Deserialize, Serialize};

/// Password + optional 2FA secret. The shared secret is the same value the
/// Steam mobile authenticator was provisioned with — base64-encoded bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PasswordCredentials {
    /// Steam account name.
    pub account_name: String,
    /// Account password.
    pub password: String,
    /// Base64 of the TOTP shared secret, if 2FA is enabled. Steam Mobile
    /// Authenticator uses HMAC-SHA1 with a custom 26-char alphabet —
    /// implemented in [`crate::auth::totp`].
    pub shared_secret: Option<String>,
}

/// An opaque refresh token issued by Steam.
///
/// Persisting and reusing this lets later logins skip the 2FA round-trip and
/// (often) bypass the password challenge entirely. Treat as a secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshToken(pub String);

impl RefreshToken {
    /// Construct from a raw token string.
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Reveal the underlying token. The wrapper exists so callers can grep
    /// for `RefreshToken` and audit secret handling; the deref is explicit.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// The two ways to log a Steam account in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Credentials {
    /// Username + password, optionally with a TOTP shared secret for 2FA.
    /// Used for first logins and any time the refresh token is missing or
    /// rejected.
    Password(PasswordCredentials),
    /// A previously-obtained refresh token. Avoids 2FA prompts. Returned by
    /// the auth flow whenever Steam emits one.
    RefreshToken(RefreshToken),
}

impl Credentials {
    /// Convenience constructor.
    pub fn password(
        account_name: impl Into<String>,
        password: impl Into<String>,
        shared_secret: Option<String>,
    ) -> Self {
        Self::Password(PasswordCredentials {
            account_name: account_name.into(),
            password: password.into(),
            shared_secret,
        })
    }

    /// Convenience constructor.
    pub fn refresh_token(token: impl Into<String>) -> Self {
        Self::RefreshToken(RefreshToken::new(token))
    }
}
