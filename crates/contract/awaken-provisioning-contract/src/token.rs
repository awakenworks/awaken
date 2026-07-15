//! Lease-bound callback token (oversight-lease parity) — neutral and **crypto-free**.
//!
//! A worker that calls back to the control plane presents a token binding the call to
//! its `(lease, run, worker)` tuple, an expiry, and a nonce. This module owns the
//! token *vocabulary* and *verification policy* (tuple + time window + MAC match +
//! replay), but the actual HMAC is an **injected signer** (`Fn(&[u8]) -> Vec<u8>`), so
//! the data-only contract never depends on a crypto crate — the real HMAC lives in a
//! signing adapter. Replay protection is a small stateful [`NonceWatermark`].

use std::collections::HashMap;

/// The signed fields of a lease-bound callback token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCallbackClaims {
    pub lease_id: String,
    pub run_id: String,
    pub worker_id: String,
    /// When the token was minted (wall-clock ms).
    pub issued_ms: u64,
    /// Hard expiry (wall-clock ms) — should be `<= lease_expiry` (see `capped_expiry`).
    pub expires_ms: u64,
    /// Monotonic per-lease nonce (replay watermark input).
    pub nonce: u64,
}

impl LeaseCallbackClaims {
    /// Canonical bytes the MAC is computed over — a schema-tagged, fixed-order framing
    /// (`olct1`) so signer and verifier agree byte-for-byte.
    fn signing_bytes(&self) -> Vec<u8> {
        format!(
            "olct1|{}|{}|{}|{}|{}|{}",
            self.lease_id, self.run_id, self.worker_id, self.issued_ms, self.expires_ms, self.nonce
        )
        .into_bytes()
    }

    /// Mint a token by attaching the MAC the injected `signer` produces over the
    /// canonical bytes.
    #[must_use]
    pub fn sign(self, signer: impl Fn(&[u8]) -> Vec<u8>) -> LeaseCallbackToken {
        let mac = signer(&self.signing_bytes());
        LeaseCallbackToken { claims: self, mac }
    }
}

/// A minted token: its claims plus their MAC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseCallbackToken {
    pub claims: LeaseCallbackClaims,
    pub mac: Vec<u8>,
}

/// Why a callback token is rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    /// The recomputed MAC does not match (wrong key or tampered).
    #[error("token MAC does not verify")]
    BadSignature,
    /// Past its expiry.
    #[error("token expired")]
    Expired,
    /// Presented before it was valid (beyond the clock-skew window).
    #[error("token not yet valid (clock skew)")]
    NotYetValid,
    /// The token's tuple does not match the expected lease/run/worker.
    #[error("token does not match the expected lease/run/worker")]
    TupleMismatch,
}

/// Constant-time byte-slice equality (avoids a MAC-comparison timing leak).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl LeaseCallbackToken {
    /// Verify the token against the `expected` tuple at `now_ms` (± `skew_ms`),
    /// recomputing the MAC with the injected `signer`. Replay is a **separate**
    /// concern the caller enforces via [`NonceWatermark`].
    pub fn verify(
        &self,
        expected_lease: &str,
        expected_run: &str,
        expected_worker: &str,
        now_ms: u64,
        skew_ms: u64,
        signer: impl Fn(&[u8]) -> Vec<u8>,
    ) -> Result<(), TokenError> {
        let c = &self.claims;
        if c.lease_id != expected_lease
            || c.run_id != expected_run
            || c.worker_id != expected_worker
        {
            return Err(TokenError::TupleMismatch);
        }
        if !ct_eq(&signer(&c.signing_bytes()), &self.mac) {
            return Err(TokenError::BadSignature);
        }
        if now_ms > c.expires_ms {
            return Err(TokenError::Expired);
        }
        if now_ms.saturating_add(skew_ms) < c.issued_ms {
            return Err(TokenError::NotYetValid);
        }
        Ok(())
    }
}

/// Per-lease monotonic nonce watermark — replay protection. `admit` accepts a nonce
/// strictly greater than the highest seen for that lease and advances the watermark;
/// a repeated or lower nonce is a replay and is rejected without advancing.
#[derive(Debug, Default)]
pub struct NonceWatermark {
    highest: HashMap<String, u64>,
}

impl NonceWatermark {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit `nonce` for `lease_id`: `true` (and advance) iff it exceeds the watermark.
    pub fn admit(&mut self, lease_id: &str, nonce: u64) -> bool {
        match self.highest.get(lease_id) {
            Some(&hi) if nonce <= hi => false, // replay / stale
            _ => {
                self.highest.insert(lease_id.to_string(), nonce);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic stand-in HMAC keyed by `key`: prefix the key then a rolling sum.
    fn signer(key: u8) -> impl Fn(&[u8]) -> Vec<u8> {
        move |bytes: &[u8]| {
            let sum = bytes.iter().fold(key, |a, b| a.wrapping_add(*b));
            vec![key, sum]
        }
    }

    fn claims(nonce: u64) -> LeaseCallbackClaims {
        LeaseCallbackClaims {
            lease_id: "lease-1".into(),
            run_id: "run-1".into(),
            worker_id: "w-1".into(),
            issued_ms: 1_000,
            expires_ms: 2_000,
            nonce,
        }
    }

    #[test]
    fn a_signed_token_verifies_against_its_tuple_and_window() {
        let token = claims(1).sign(signer(7));
        assert!(
            token
                .verify("lease-1", "run-1", "w-1", 1_500, 100, signer(7))
                .is_ok()
        );
    }

    #[test]
    fn a_wrong_key_or_tampered_mac_fails_signature() {
        let token = claims(1).sign(signer(7));
        assert_eq!(
            token.verify("lease-1", "run-1", "w-1", 1_500, 100, signer(9)),
            Err(TokenError::BadSignature)
        );
        let mut tampered = claims(1).sign(signer(7));
        tampered.mac[0] ^= 0xFF;
        assert_eq!(
            tampered.verify("lease-1", "run-1", "w-1", 1_500, 100, signer(7)),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_mismatched_tuple_or_bad_window_is_rejected() {
        let token = claims(1).sign(signer(7));
        assert_eq!(
            token.verify("lease-X", "run-1", "w-1", 1_500, 100, signer(7)),
            Err(TokenError::TupleMismatch)
        );
        assert_eq!(
            token.verify("lease-1", "run-1", "w-1", 2_500, 100, signer(7)),
            Err(TokenError::Expired)
        );
        // now(500)+skew(100) < issued(1000) → not yet valid.
        assert_eq!(
            token.verify("lease-1", "run-1", "w-1", 500, 100, signer(7)),
            Err(TokenError::NotYetValid)
        );
    }

    #[test]
    fn a_wrong_tuple_masks_a_bad_signature() {
        // Cause-effect masking: the tuple is checked before the MAC, so a token that
        // fails BOTH reports `TupleMismatch` — it never leaks that the signature is also
        // bad (precedence short-circuit).
        let mut token = claims(1).sign(signer(7));
        token.mac[0] ^= 0xFF; // corrupt the signature too
        assert_eq!(
            token.verify("lease-X", "run-1", "w-1", 1_500, 100, signer(7)),
            Err(TokenError::TupleMismatch)
        );
    }

    #[test]
    fn a_bad_signature_is_reported_before_expiry() {
        // Cause-effect precedence: the MAC is verified *before* the time window, so a
        // token with the right tuple but a corrupt signature that is ALSO past expiry
        // reports `BadSignature`, not `Expired` — an unauthenticated token never has its
        // (in)validity in time leaked. Completes the tuple > signature > time chain.
        let mut token = claims(1).sign(signer(7));
        token.mac[1] ^= 0xFF; // corrupt the signature
        assert_eq!(
            // now=2_500 is past expires_ms=2_000, so expiry would also fire.
            token.verify("lease-1", "run-1", "w-1", 2_500, 100, signer(7)),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_tuple_mismatch_on_run_or_worker_is_rejected() {
        // The full tuple is fenced — previously only a differing lease_id was tested.
        let token = claims(1).sign(signer(7));
        assert_eq!(
            token.verify("lease-1", "run-X", "w-1", 1_500, 100, signer(7)),
            Err(TokenError::TupleMismatch)
        );
        assert_eq!(
            token.verify("lease-1", "run-1", "w-X", 1_500, 100, signer(7)),
            Err(TokenError::TupleMismatch)
        );
    }

    #[test]
    fn the_validity_window_is_inclusive_at_both_edges() {
        // Boundary: `now == expires` is still valid (expiry is exclusive-above), and
        // `now + skew == issued` is valid (not-yet-valid is strictly-before).
        let token = claims(1).sign(signer(7));
        assert!(
            token
                .verify("lease-1", "run-1", "w-1", 2_000, 0, signer(7))
                .is_ok(),
            "now == expires_ms is inside the window"
        );
        assert!(
            token
                .verify("lease-1", "run-1", "w-1", 900, 100, signer(7))
                .is_ok(),
            "now + skew == issued_ms is inside the window"
        );
    }

    #[test]
    fn the_nonce_watermark_rejects_replays_and_advances_on_higher() {
        let mut wm = NonceWatermark::new();
        assert!(wm.admit("lease-1", 5), "first nonce admitted");
        assert!(!wm.admit("lease-1", 5), "same nonce is a replay");
        assert!(!wm.admit("lease-1", 3), "a lower nonce is stale");
        assert!(wm.admit("lease-1", 6), "a higher nonce advances");
        // A different lease has its own independent watermark.
        assert!(wm.admit("lease-2", 1));
    }
}
