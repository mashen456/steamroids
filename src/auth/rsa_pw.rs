//! RSA-encrypt the account password for `BeginAuthSessionViaCredentials`.
//!
//! Steam's `WebAPI` auth flow expects the password to be PKCS#1 v1.5 encrypted
//! with the RSA public key returned by `GetPasswordRSAPublicKey`, then encoded
//! as standard base64. The modulus and exponent in the public-key response are
//! lowercase hex strings.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rsa::rand_core::OsRng;
use rsa::{BigUint, Pkcs1v15Encrypt, RsaPublicKey};

use crate::{Error, Result};

/// Encrypt `password` with the RSA public key described by `mod_hex` and
/// `exp_hex`. Returns the standard-base64-encoded ciphertext.
///
/// The hex strings are the `publickey_mod` / `publickey_exp` fields from
/// `CAuthentication_GetPasswordRSAPublicKey_Response`. Padding is PKCS#1 v1.5
/// — the same padding the official Steam clients use.
pub(crate) fn encrypt_password(password: &str, mod_hex: &str, exp_hex: &str) -> Result<String> {
    let n = BigUint::parse_bytes(mod_hex.as_bytes(), 16)
        .ok_or_else(|| Error::AuthRejected("rsa: bad modulus hex".into()))?;
    let e = BigUint::parse_bytes(exp_hex.as_bytes(), 16)
        .ok_or_else(|| Error::AuthRejected("rsa: bad exponent hex".into()))?;

    let pk = RsaPublicKey::new(n, e).map_err(|err| Error::AuthRejected(format!("rsa: {err}")))?;

    let mut rng = OsRng;
    let ct = pk
        .encrypt(&mut rng, Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|err| Error::AuthRejected(format!("rsa encrypt: {err}")))?;

    Ok(B64.encode(ct))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;

    #[test]
    fn round_trip_against_known_key() {
        // Generate a small key in-test (1024 bits — only for the test, never
        // used for real auth). PKCS#1 v1.5 padding is randomized, so we can
        // only assert by decrypting and comparing plaintexts.
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).expect("rsa keygen");
        let pub_key = RsaPublicKey::from(&priv_key);

        let mod_hex = format!("{:x}", pub_key.n());
        let exp_hex = format!("{:x}", pub_key.e());

        let password = "hunter2-the-real-one";
        let b64_ct = encrypt_password(password, &mod_hex, &exp_hex).expect("encrypt");

        let ct = B64.decode(b64_ct).expect("b64 decode");
        let pt = priv_key.decrypt(Pkcs1v15Encrypt, &ct).expect("rsa decrypt");

        assert_eq!(pt, password.as_bytes());
    }

    #[test]
    fn bad_mod_hex_is_an_error() {
        let err = encrypt_password("x", "not-hex", "010001").unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }

    #[test]
    fn bad_exp_hex_is_an_error() {
        // Valid (even-length) hex modulus paired with garbage exponent.
        let err = encrypt_password("x", "ab", "zzz").unwrap_err();
        assert!(matches!(err, Error::AuthRejected(_)));
    }
}
