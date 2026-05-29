//! JWT payload extraction for Steam refresh tokens.
//!
//! Steam-issued refresh tokens are JWTs whose payload contains a `sub` claim
//! holding the 64-bit Steam ID as a decimal string. We do **not** verify the
//! token's signature here — we don't have Steam's signing key, and the
//! signature is irrelevant for our purposes (Steam re-validates server-side
//! every time we hit `GenerateAccessTokenForApp`). We only need the `SteamID`
//! so we can populate the request.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;

use crate::{Error, Result};

/// Decode `jwt`'s middle (payload) segment and return the `sub` claim parsed
/// as a `u64`.
///
/// Errors map to [`Error::AuthRejected`] with a short reason — callers
/// surface this as a `Err`, not a `SignInOutcome` (a malformed JWT is a
/// caller-side input error, not a Steam-side rejection).
pub(crate) fn steam_id_from_refresh_token(jwt: &str) -> Result<u64> {
    let mid = jwt
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::AuthRejected("jwt: no payload segment".into()))?;

    let bytes = B64URL
        .decode(mid)
        .map_err(|e| Error::AuthRejected(format!("jwt b64: {e}")))?;

    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::AuthRejected(format!("jwt json: {e}")))?;

    let sub = v
        .get("sub")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::AuthRejected("jwt: no sub claim".into()))?;

    sub.parse::<u64>()
        .map_err(|e| Error::AuthRejected(format!("jwt sub: {e}")))
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
}
