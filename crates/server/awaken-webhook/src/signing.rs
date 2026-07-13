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
///
/// A valid signature never expires on its own — pair this with
/// [`timestamp_within_tolerance`] (or use [`verify_fresh`]) to reject replays of a
/// captured, correctly-signed past delivery.
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

/// Standard Webhooks' recommended replay-tolerance window, in seconds (±5 minutes).
/// A `webhook-timestamp` outside `[now - TOLERANCE, now + TOLERANCE]` is rejected
/// even when the signature verifies.
pub const DEFAULT_TOLERANCE_SECS: i64 = 300;

/// True iff `timestamp` (the `webhook-timestamp` header value) is within
/// `tolerance_secs` of `now` — both unix seconds, in *either* direction (a
/// too-future timestamp is clock skew or a forged-ahead replay). The staleness
/// bound a receiver enforces so a captured, authentic past delivery cannot be
/// replayed later; the HMAC signature alone carries no expiry.
#[must_use]
pub fn timestamp_within_tolerance(timestamp: i64, now: i64, tolerance_secs: i64) -> bool {
    now.saturating_sub(timestamp).saturating_abs() <= tolerance_secs
}

/// The full receiver-side check: the timestamp is fresh AND the signature verifies.
/// Returns `Ok(false)` for a stale timestamp *or* a bad signature (both mean
/// "reject", and staying indistinguishable gives a replayer no oracle); `Err` only
/// for a malformed secret. Checking freshness first also avoids signing work for an
/// obviously-stale replay.
pub fn verify_fresh(
    secret: &str,
    msg_id: &str,
    timestamp: i64,
    payload: &str,
    header: &str,
    now: i64,
    tolerance_secs: i64,
) -> Result<bool, SignError> {
    if !timestamp_within_tolerance(timestamp, now, tolerance_secs) {
        return Ok(false);
    }
    verify(secret, msg_id, timestamp, payload, header)
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

    #[test]
    fn a_minted_secret_is_prefixed_decodable_and_usable() {
        let secret = generate_secret();
        assert!(
            secret.starts_with(SECRET_PREFIX),
            "carries the whsec_ prefix"
        );
        let body = secret.strip_prefix(SECRET_PREFIX).expect("prefix present");
        assert_eq!(
            B64.decode(body).expect("body is base64").len(),
            24,
            "24 bytes of OS entropy"
        );
        // The minted secret signs and verifies end-to-end.
        let header = signature_header(&secret, "m", 1, "{}").unwrap();
        assert!(verify(&secret, "m", 1, "{}", &header).unwrap());
    }

    #[test]
    fn verify_accepts_a_valid_signature_among_other_versions() {
        // Standard Webhooks headers are a space-separated multi-version list; a valid
        // `v1` entry must still verify when other (unknown/newer) versions precede it.
        let good = sign_bytes(SECRET, "m", 1, "{}").unwrap();
        let header = format!("v2,someothersig v1,{good}");
        assert!(
            verify(SECRET, "m", 1, "{}", &header).unwrap(),
            "a good v1 among other versions verifies"
        );
    }

    #[test]
    fn verify_rejects_degenerate_headers_without_panic() {
        // Empty, no `v1,` prefix at all, and a wrong-length signature (the unequal-
        // length ct_eq branch) must each return Ok(false) — never panic.
        for header in ["", "v2,nope", "v1,short", "garbage"] {
            assert!(
                !verify(SECRET, "m", 1, "{}", header).unwrap(),
                "degenerate header {header:?} is a clean false"
            );
        }
    }

    #[test]
    fn timestamp_tolerance_bounds_both_directions() {
        let now = 1_700_000_000;
        // Inside the window (past and future skew) is fresh; exactly at the bound is
        // inclusive; just outside — in either direction — is stale.
        assert!(timestamp_within_tolerance(now, now, DEFAULT_TOLERANCE_SECS));
        assert!(timestamp_within_tolerance(
            now - DEFAULT_TOLERANCE_SECS,
            now,
            DEFAULT_TOLERANCE_SECS
        ));
        assert!(timestamp_within_tolerance(
            now + DEFAULT_TOLERANCE_SECS,
            now,
            DEFAULT_TOLERANCE_SECS
        ));
        assert!(!timestamp_within_tolerance(
            now - DEFAULT_TOLERANCE_SECS - 1,
            now,
            DEFAULT_TOLERANCE_SECS
        ));
        assert!(!timestamp_within_tolerance(
            now + DEFAULT_TOLERANCE_SECS + 1,
            now,
            DEFAULT_TOLERANCE_SECS
        ));
    }

    #[test]
    fn verify_fresh_rejects_a_replay_of_an_authentic_delivery() {
        // A delivery signed at t0 is authentic and verifies fresh at t0…
        let t0 = 1_700_000_000;
        let header = signature_header(SECRET, "event_1", t0, "{\"a\":1}").unwrap();
        assert!(
            verify_fresh(
                SECRET,
                "event_1",
                t0,
                "{\"a\":1}",
                &header,
                t0,
                DEFAULT_TOLERANCE_SECS
            )
            .unwrap()
        );
        // …but a byte-identical re-POST replayed an hour later is rejected even though
        // the signature is still valid — the timestamp is now stale.
        let much_later = t0 + 3_600;
        assert!(
            !verify_fresh(
                SECRET,
                "event_1",
                t0,
                "{\"a\":1}",
                &header,
                much_later,
                DEFAULT_TOLERANCE_SECS
            )
            .unwrap(),
            "a stale but correctly-signed replay is rejected"
        );
        // A stale timestamp with a bad signature is likewise Ok(false), indistinguishable.
        assert!(
            !verify_fresh(
                SECRET,
                "event_1",
                t0,
                "{\"a\":2}",
                &header,
                t0,
                DEFAULT_TOLERANCE_SECS
            )
            .unwrap()
        );
    }
}
