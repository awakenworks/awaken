//! Standard Webhooks signing (<https://www.standardwebhooks.com>): the signed
//! content is `{msg_id}.{timestamp}.{payload}`, HMAC-SHA256 with the subscription
//! secret's key, base64-encoded, presented as a space-separated list of versioned
//! signatures (`v1,<sig>`). This is the exact scheme the `standardwebhooks` npm
//! verifier expects, so the TS e2e can verify a delivery without any custom code.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// The `whsec_` prefix on a subscription secret. The bytes after it are the
/// base64-encoded HMAC key.
pub const SECRET_PREFIX: &str = "whsec_";

/// Signing / verification failure.
#[derive(Debug, PartialEq, Eq)]
pub enum SignError {
    /// The secret did not start with `whsec_` or its body was not valid base64.
    MalformedSecret,
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::MalformedSecret => write!(f, "malformed webhook secret"),
        }
    }
}

impl std::error::Error for SignError {}

/// Mint a fresh `whsec_<base64>` signing secret from OS entropy (24 random bytes)
/// — handed to the subscriber once at create time and stored to sign deliveries.
pub fn generate_secret() -> String {
    let mut key = [0u8; 24];
    getrandom::getrandom(&mut key).expect("OS entropy for a webhook secret");
    format!("{SECRET_PREFIX}{}", B64.encode(key))
}

fn key_bytes(secret: &str) -> Result<Vec<u8>, SignError> {
    let body = secret.strip_prefix(SECRET_PREFIX).unwrap_or(secret);
    B64.decode(body).map_err(|_| SignError::MalformedSecret)
}

/// The base64 signature (no `v1,` prefix) over `{msg_id}.{timestamp}.{payload}`.
pub fn sign_bytes(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    payload: &str,
) -> Result<String, SignError> {
    let key = key_bytes(secret)?;
    let mut mac = HmacSha256::new_from_slice(&key).expect("HMAC accepts any key length");
    mac.update(format!("{msg_id}.{timestamp}.{payload}").as_bytes());
    Ok(B64.encode(mac.finalize().into_bytes()))
}

/// The full `webhook-signature` header value: a single `v1,<sig>` entry.
pub fn signature_header(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    payload: &str,
) -> Result<String, SignError> {
    Ok(format!(
        "v1,{}",
        sign_bytes(secret, msg_id, timestamp, payload)?
    ))
}

/// Constant-time verification of a `webhook-signature` header (space-separated
/// `v1,<sig>` list): true iff any listed `v1` signature matches. This is what a
/// receiver runs; the e2e uses it to prove the delivered signature is authentic.
pub fn verify(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    payload: &str,
    header: &str,
) -> Result<bool, SignError> {
    let expected = sign_bytes(secret, msg_id, timestamp, payload)?;
    let expected = expected.as_bytes();
    let matched = header.split(' ').any(|part| {
        part.strip_prefix("v1,")
            .map(|sig| sig.as_bytes().ct_eq(expected).into())
            .unwrap_or(false)
    });
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw"; // sample key bytes

    #[test]
    fn sign_then_verify_round_trips() {
        let header = signature_header(SECRET, "msg_1", 1_700_000_000, "{\"a\":1}").unwrap();
        assert!(header.starts_with("v1,"));
        assert!(verify(SECRET, "msg_1", 1_700_000_000, "{\"a\":1}", &header).unwrap());
    }

    #[test]
    fn a_tampered_payload_fails_verification() {
        let header = signature_header(SECRET, "msg_1", 1_700_000_000, "{\"a\":1}").unwrap();
        assert!(!verify(SECRET, "msg_1", 1_700_000_000, "{\"a\":2}", &header).unwrap());
        assert!(!verify(SECRET, "msg_1", 1_700_000_001, "{\"a\":1}", &header).unwrap());
        assert!(!verify(SECRET, "other", 1_700_000_000, "{\"a\":1}", &header).unwrap());
    }

    #[test]
    fn a_malformed_secret_is_an_error() {
        assert_eq!(
            sign_bytes("whsec_!!!not-base64!!!", "m", 1, "{}"),
            Err(SignError::MalformedSecret)
        );
    }

    #[test]
    fn the_prefix_is_optional_for_the_key() {
        // Bare base64 (no `whsec_`) signs identically — receivers store either form.
        let bare = "MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        assert_eq!(
            sign_bytes(SECRET, "m", 1, "{}"),
            sign_bytes(bare, "m", 1, "{}"),
        );
    }
}
