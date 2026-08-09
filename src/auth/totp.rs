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
use sha1::Sha1;

use crate::error::Error;

type HmacSha1 = Hmac<Sha1>;

/// The 26 characters the Steam authenticator uses to display the 2FA code.
const STEAM_ALPHABET: &[u8; 26] = b"23456789BCDFGHJKMNPQRTVWXY";

/// Length of the time-step in seconds. Matches `node-steam-totp` and `SteamKit2`.
const TIME_STEP_SECS: u64 = 30;

/// Generate the 5-character Steam 2FA code for the given shared secret.
///
/// If `timestamp` is `None`, the current wall-clock time is used.
pub fn generate_auth_code(
    shared_secret_b64: &str,
    timestamp: Option<u64>,
) -> Result<String, Error> {
    let secret = base64::engine::general_purpose::STANDARD.decode(shared_secret_b64.trim())?;
    let ts = match timestamp {
        Some(t) => t,
        None => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::InvalidConfig("system clock before epoch".into()))?
            .as_secs(),
    };
    Ok(generate_with_step(&secret, ts / TIME_STEP_SECS))
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
}
