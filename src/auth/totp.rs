//! Steam TOTP — the 5-character code shown by the Steam Mobile Authenticator.
//!
//! Steam uses a non-standard variant of TOTP/HOTP:
//!
//! 1. HMAC-SHA1 of the shared secret over a 30-second time-step counter (big-endian u64).
//! 2. Dynamic truncation per RFC 4226 §5.3 — pick a 4-byte slice using the low
//!    nibble of the last hash byte, mask off the top bit.
//! 3. Map the 31-bit integer to **5 base-26 characters** from a custom alphabet
//!    (the standard RFC produces decimal digits — this is the Steam twist).
//!
//! The alphabet, taken from Steam Mobile sources and verified across
//! `node-steam-totp`, `SteamKit2`, `python-steam`:
//!
//! ```text
//! 23456789BCDFGHJKMNPQRTVWXY
//! ```
//!
//! ## Example
//!
//! ```no_run
//! use steamroids::auth::totp::generate_auth_code;
//!
//! let code = generate_auth_code("YourSharedSecretBase64==", None).unwrap();
//! assert_eq!(code.len(), 5);
//! ```

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha1::Sha1;

use crate::error::Error;
use crate::transport::proxy::ProxyConfig;

type HmacSha1 = Hmac<Sha1>;

/// The 26 characters the Steam authenticator uses to display the 2FA code.
const STEAM_ALPHABET: &[u8; 26] = b"23456789BCDFGHJKMNPQRTVWXY";

/// Length of the time-step in seconds. Matches `node-steam-totp` and `SteamKit2`.
const TIME_STEP_SECS: u64 = 30;

/// Steam's server-time endpoint. No auth, no request body.
const QUERY_TIME_URL: &str = "https://api.steampowered.com/ITwoFactorService/QueryTime/v0001/";

/// Generate the 5-character Steam 2FA code for the given shared secret.
///
/// If `timestamp` is `None`, the current wall-clock time is used.
pub fn generate_auth_code(
    shared_secret_b64: &str,
    timestamp: Option<u64>,
) -> Result<String, Error> {
    let secret = base64::engine::general_purpose::STANDARD.decode(shared_secret_b64.trim())?;
    match timestamp {
        // caller gave an explicit instant, no offset to apply
        Some(t) => Ok(generate_with_step(&secret, t / TIME_STEP_SECS)),
        None => generate_with_offset(&secret, 0),
    }
}

/// Generate the 5-character Steam 2FA code from a decoded secret, using the
/// local wall clock plus `offset_secs`.
///
/// `offset_secs` corrects for drift between the local clock and Steam's own,
/// as reported by [`query_server_time_offset`]: `local_time + offset_secs`
/// approximates Steam's clock. Pass `0` for plain local-clock generation,
/// which is exactly what [`generate_auth_code`] does.
///
/// The offset is applied with saturating arithmetic, so an absurd value
/// clamps to the unix epoch rather than underflowing.
///
/// # Errors
///
/// [`Error::InvalidConfig`] if the local clock reads before the unix epoch.
pub fn generate_with_offset(secret: &[u8], offset_secs: i64) -> Result<String, Error> {
    let local = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::InvalidConfig("system clock before epoch".into()))?
        .as_secs();
    let ts = apply_offset(local, offset_secs);
    Ok(generate_with_step(secret, ts / TIME_STEP_SECS))
}

/// Add a signed offset to a unix timestamp without panicking, clamping to
/// `0` on the low end and `u64::MAX` on the high end.
fn apply_offset(base_secs: u64, offset_secs: i64) -> u64 {
    if offset_secs >= 0 {
        // offset_secs >= 0 always fits in u64
        base_secs.saturating_add(offset_secs.unsigned_abs())
    } else {
        base_secs.saturating_sub(offset_secs.unsigned_abs())
    }
}

/// Lower-level entry point — caller passes the time-step counter directly.
/// Exposed for deterministic testing.
pub fn generate_with_step(secret: &[u8], step: u64) -> String {
    let mut mac = HmacSha1::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(&step.to_be_bytes());
    let hash = mac.finalize().into_bytes();

    let offset = (hash[19] & 0x0F) as usize;
    let mut code: u32 = u32::from_be_bytes([
        hash[offset] & 0x7F,
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ]);

    let mut out = [0u8; 5];
    for slot in &mut out {
        *slot = STEAM_ALPHABET[(code % 26) as usize];
        code /= 26;
    }
    // SAFETY-equivalent: STEAM_ALPHABET only contains ASCII, so this is valid UTF-8.
    String::from_utf8(out.to_vec()).expect("alphabet is ASCII")
}

/// Ask Steam what its own clock reads right now, and return the offset
/// (`server_time - local_time`, in seconds) to feed into
/// [`generate_with_offset`].
///
/// The offset only changes if the local machine's clock is re-synced or
/// drifts, so it is stable over the lifetime of a process. **Fetch it once
/// and reuse it**: do not call this before every code generation. There is
/// no caching or refresh built in here; that policy is the caller's.
///
/// # Errors
///
/// [`Error::Network`] if the request fails or Steam's response can't be
/// parsed. [`Error::InvalidConfig`] if the local clock reads before the unix
/// epoch.
pub async fn query_server_time_offset(proxy: Option<&ProxyConfig>) -> Result<i64, Error> {
    let client = crate::http::client(proxy)?;
    let response = client
        .post(QUERY_TIME_URL)
        // Akamai 411s a bodyless POST, so send an explicit empty body.
        .body(Vec::new())
        .send()
        .await
        .map_err(|e| Error::Network(format!("QueryTime: {e}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| Error::Network(format!("QueryTime body: {e}")))?;
    if !status.is_success() {
        return Err(Error::Network(format!("QueryTime: HTTP {status}")));
    }

    let server_time = parse_server_time(&body)?;

    let local = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::InvalidConfig("system clock before epoch".into()))?
        .as_secs();
    let local = i64::try_from(local)
        .map_err(|_| Error::InvalidConfig("local unix time overflows i64".into()))?;

    Ok(server_time.saturating_sub(local))
}

/// JSON shape of `ITwoFactorService/QueryTime/v0001`'s response (only the
/// field this crate uses). Verified against a live call: `server_time` comes
/// back as a **json string**, not a number.
#[derive(Deserialize)]
struct QueryTimeResponse {
    response: QueryTimeInner,
}

#[derive(Deserialize)]
struct QueryTimeInner {
    server_time: String,
}

/// Parse `server_time` out of a `QueryTime` response body.
fn parse_server_time(body: &str) -> Result<i64, Error> {
    let parsed: QueryTimeResponse = serde_json::from_str(body)
        .map_err(|e| Error::Network(format!("QueryTime: unparseable response: {e}")))?;
    parsed
        .response
        .server_time
        .parse::<i64>()
        .map_err(|e| Error::Network(format!("QueryTime: server_time not an integer: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 zero bytes — the smallest valid HMAC-SHA1 key. Lets us write
    /// deterministic tests without a real Steam shared secret.
    const ZERO_SECRET_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    /// Known-answer vectors for a 20-byte zero key, derived from an
    /// independent implementation of the algorithm (HMAC-SHA1 over the
    /// big-endian step, RFC 4226 §5.3 truncation, base-26 over
    /// `STEAM_ALPHABET`). Any change here means the derivation drifted.
    const ZERO_SECRET_VECTORS: &[(u64, &str)] = &[
        (0, "RYH4D"),
        (1, "DR2DK"),
        (2, "2BYG9"),
        (42, "HWCM5"),
        (1000, "R55XV"),
        (56_000_000, "3F9QW"),
    ];

    #[test]
    fn matches_known_answer_vectors() {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(ZERO_SECRET_B64)
            .unwrap();
        assert_eq!(secret, vec![0u8; 20], "test vector key is 20 zero bytes");
        for &(step, expected) in ZERO_SECRET_VECTORS {
            assert_eq!(generate_with_step(&secret, step), expected, "step {step}");
        }
    }

    #[test]
    fn code_is_five_chars_from_the_steam_alphabet() {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(ZERO_SECRET_B64)
            .unwrap();
        let code = generate_with_step(&secret, 1);
        assert_eq!(code.len(), 5);
        for ch in code.chars() {
            assert!(
                STEAM_ALPHABET.contains(&(ch as u8)),
                "char {ch} outside Steam alphabet",
            );
        }
    }

    #[test]
    fn generate_auth_code_uses_the_30s_time_step() {
        // A timestamp anywhere in step N must produce step N's code.
        let secret = base64::engine::general_purpose::STANDARD
            .decode(ZERO_SECRET_B64)
            .unwrap();
        let expected = generate_with_step(&secret, 42);
        for ts in [42 * TIME_STEP_SECS, 42 * TIME_STEP_SECS + 29] {
            assert_eq!(
                generate_auth_code(ZERO_SECRET_B64, Some(ts)).unwrap(),
                expected
            );
        }
        assert_ne!(
            generate_auth_code(ZERO_SECRET_B64, Some(43 * TIME_STEP_SECS)).unwrap(),
            expected
        );
    }

    #[test]
    fn same_input_same_output() {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(ZERO_SECRET_B64)
            .unwrap();
        assert_eq!(
            generate_with_step(&secret, 42),
            generate_with_step(&secret, 42)
        );
    }

    #[test]
    fn different_step_different_code() {
        let secret = base64::engine::general_purpose::STANDARD
            .decode(ZERO_SECRET_B64)
            .unwrap();
        // Astronomically unlikely to collide, but assert anyway — flaky here
        // would mean the algorithm collapsed.
        assert_ne!(
            generate_with_step(&secret, 1),
            generate_with_step(&secret, 2)
        );
    }

    #[test]
    fn rejects_invalid_base64() {
        let err = generate_auth_code("not_base64!!!", Some(0)).unwrap_err();
        assert!(matches!(err, Error::Base64(_)));
    }

    #[test]
    fn alphabet_is_26_chars() {
        // Pin the alphabet size — base-26 division depends on this exact length.
        assert_eq!(STEAM_ALPHABET.len(), 26);
    }

    #[test]
    fn offset_shifts_the_time_step() {
        // 30s step: a +30s offset must land on the next step's code.
        let secret = [0u8; 20];
        let now = generate_with_offset(&secret, 0).unwrap();
        let plus_one_step = generate_with_offset(&secret, 30).unwrap();
        assert_ne!(now, plus_one_step);
    }

    #[test]
    fn a_zero_offset_matches_the_plain_generator() {
        // same secret, base64'd, through the existing public entry point
        let secret = [0u8; 20];
        assert_eq!(
            generate_with_offset(&secret, 0).unwrap(),
            generate_auth_code(ZERO_SECRET_B64, None).unwrap()
        );
    }

    #[test]
    fn a_negative_offset_does_not_underflow() {
        // absurd negative offset must error or clamp, never panic
        let secret = [0u8; 20];
        let _ = generate_with_offset(&secret, i64::MIN);
    }

    #[test]
    fn apply_offset_saturates_at_zero_for_i64_min() {
        // the largest possible negative offset must clamp, not underflow
        assert_eq!(apply_offset(1_700_000_000, i64::MIN), 0);
    }

    #[test]
    fn apply_offset_adds_a_positive_offset() {
        assert_eq!(apply_offset(1000, 30), 1030);
    }

    #[test]
    fn apply_offset_subtracts_a_negative_offset() {
        assert_eq!(apply_offset(1000, -30), 970);
    }

    #[test]
    fn parses_query_time_response_shape() {
        // captured from a live call to ITwoFactorService/QueryTime/v0001:
        // server_time is a json string, not a number.
        let body = r#"{"response":{"server_time":"1786573085","skew_tolerance_seconds":"60","large_time_jink":"86400","probe_frequency_seconds":3600,"adjusted_time_probe_frequency_seconds":300,"hint_probe_frequency_seconds":60,"sync_timeout":60,"try_again_seconds":900,"max_attempts":3}}"#;
        assert_eq!(parse_server_time(body).unwrap(), 1_786_573_085);
    }

    #[test]
    fn rejects_a_garbage_query_time_body() {
        assert!(parse_server_time("not json").is_err());
    }
}
