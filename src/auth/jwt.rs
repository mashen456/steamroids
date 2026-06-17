//! JWT payload extraction for Steam refresh tokens.
//!
//! Steam-issued refresh tokens are JWTs whose payload contains a `sub` claim
//! holding the 64-bit Steam ID as a decimal string and an `exp` claim with the
//! Unix expiry. We do **not** verify the token's signature here — we don't have
//! Steam's signing key, and the signature is irrelevant for our purposes (Steam
//! re-validates server-side on every logon). We only read the claims we need.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;

use crate::{Error, Result};

/// The refresh-token JWT claims we care about.
pub(crate) struct RefreshTokenClaims {
    /// `sub` — the 64-bit Steam ID.
    pub steam_id: u64,
    /// `exp` — Unix expiry in seconds, if present.
    pub exp: Option<u64>,
}

/// Decode `jwt`'s middle (payload) segment as JSON.
///
/// Errors map to [`Error::AuthRejected`] with a short reason — callers surface
/// this as an `Err`, not a `SignInOutcome` (a malformed JWT is a caller-side
/// input error, not a Steam-side rejection).
fn decode_payload(jwt: &str) -> Result<serde_json::Value> {
    let mid = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::AuthRejected("jwt: no payload segment".into()))?;

    let bytes = B64URL
        .decode(mid)
        .map_err(|e| Error::AuthRejected(format!("jwt b64: {e}")))?;

    serde_json::from_slice(&bytes).map_err(|e| Error::AuthRejected(format!("jwt json: {e}")))
}

/// Read the `sub` (Steam ID) and `exp` (expiry) claims from a refresh-token JWT.
pub(crate) fn refresh_token_claims(jwt: &str) -> Result<RefreshTokenClaims> {
    let v = decode_payload(jwt)?;

    let sub = v
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::AuthRejected("jwt: no sub claim".into()))?;
    let steam_id = sub
        .parse::<u64>()
        .map_err(|e| Error::AuthRejected(format!("jwt sub: {e}")))?;

    let exp = v.get("exp").and_then(serde_json::Value::as_u64);

    Ok(RefreshTokenClaims { steam_id, exp })
}

/// Convenience: just the `sub` claim parsed as a `u64`.
pub(crate) fn steam_id_from_refresh_token(jwt: &str) -> Result<u64> {
    Ok(refresh_token_claims(jwt)?.steam_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
    use base64::Engine;

    fn build_jwt(payload_json: &str) -> String {
        // Header / signature segments don't matter — only the middle is parsed.
        let header = B64URL.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = B64URL.encode(payload_json.as_bytes());
        let signature = B64URL.encode(b"signature-not-checked");
        format!("{header}.{payload}.{signature}")
    }

    #[test]
    fn decodes_steam_id_from_well_formed_jwt() {
        let jwt = build_jwt(r#"{"sub":"76561198000000001","aud":["derive"]}"#);
        let sid = steam_id_from_refresh_token(&jwt).unwrap();
        assert_eq!(sid, 76_561_198_000_000_001);
    }

    #[test]
    fn rejects_garbage_string() {
        let err = steam_id_from_refresh_token("not.a.real.jwt").unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn rejects_single_segment() {
        let err = steam_id_from_refresh_token("singleseg").unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn rejects_payload_without_sub() {
        let jwt = build_jwt(r#"{"aud":["derive"]}"#);
        let err = steam_id_from_refresh_token(&jwt).unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn rejects_non_numeric_sub() {
        let jwt = build_jwt(r#"{"sub":"hello"}"#);
        let err = steam_id_from_refresh_token(&jwt).unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn reads_sub_and_exp_claims() {
        let jwt = build_jwt(r#"{"sub":"76561198000000001","exp":1799839262,"aud":["client"]}"#);
        let claims = refresh_token_claims(&jwt).unwrap();
        assert_eq!(claims.steam_id, 76_561_198_000_000_001);
        assert_eq!(claims.exp, Some(1_799_839_262));
    }

    #[test]
    fn exp_is_none_when_absent() {
        let jwt = build_jwt(r#"{"sub":"76561198000000001"}"#);
        let claims = refresh_token_claims(&jwt).unwrap();
        assert_eq!(claims.exp, None);
    }
}
