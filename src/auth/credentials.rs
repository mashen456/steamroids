//! Credential types used by the login flow.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Placeholder rendered in `Debug` output in place of a secret value.
const REDACTED: &str = "<redacted>";

/// Password + optional 2FA secret. The shared secret is the same value the
/// Steam mobile authenticator was provisioned with — base64-encoded bytes.
///
/// `Debug` is implemented by hand to keep the password and shared secret out
/// of logs and traces; only the account name and whether a secret is present
/// are shown.
#[derive(Clone, Serialize, Deserialize)]
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

impl fmt::Debug for PasswordCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordCredentials")
            .field("account_name", &self.account_name)
            .field("password", &REDACTED)
            // Show presence (so 2FA-vs-not is visible) without the value.
            .field("shared_secret", &self.shared_secret.as_ref().map(|_| REDACTED))
            .finish()
    }
}

/// An opaque refresh token issued by Steam.
///
/// Persisting and reusing this lets later logins skip the 2FA round-trip and
/// (often) bypass the password challenge entirely. Treat as a secret.
///
/// `Debug` is redacted; use [`RefreshToken::expose`] to read the value.
#[derive(Clone, Serialize, Deserialize)]
pub struct RefreshToken(pub String);

impl fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RefreshToken").field(&REDACTED).finish()
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_debug_redacts_secrets() {
        let creds = PasswordCredentials {
            account_name: "bot01".into(),
            password: "hunter2-super-secret".into(),
            shared_secret: Some("base64SharedSecret==".into()),
        };
        let dbg = format!("{creds:?}");
        assert!(
            !dbg.contains("hunter2-super-secret"),
            "password leaked: {dbg}"
        );
        assert!(
            !dbg.contains("base64SharedSecret"),
            "shared secret leaked: {dbg}"
        );
        // Non-secret context is still useful.
        assert!(dbg.contains("bot01"));
        assert!(dbg.contains(REDACTED));
    }

    #[test]
    fn password_debug_distinguishes_missing_shared_secret() {
        let creds = PasswordCredentials {
            account_name: "bot01".into(),
            password: "pw".into(),
            shared_secret: None,
        };
        // Presence of 2FA should be observable without leaking the value.
        assert!(format!("{creds:?}").contains("None"));
    }

    #[test]
    fn refresh_token_debug_redacts_value() {
        let token = RefreshToken::new("eyJ.super.secret.jwt.payload");
        let dbg = format!("{token:?}");
        assert!(!dbg.contains("secret.jwt"), "token leaked: {dbg}");
        assert!(dbg.contains(REDACTED));
        // The value is still reachable explicitly.
        assert_eq!(token.expose(), "eyJ.super.secret.jwt.payload");
    }

    #[test]
    fn credentials_debug_delegates_to_redacted_inner() {
        let pw = Credentials::password("bot01", "leak-me", None);
        assert!(!format!("{pw:?}").contains("leak-me"));

        let rt = Credentials::refresh_token("leak-this-token");
        assert!(!format!("{rt:?}").contains("leak-this-token"));
    }
}
