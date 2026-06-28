//! LicenseClaim: signed, offline-verifiable capability token for subscriptions.
//!
//! `LicenseAuthority::mint()` produces a compact JWT (EdDSA / OKP) that
//! encodes the subscription subject, billing epoch, entitlements, and a
//! bounded validity window.  The matching JWKS document can be published at a
//! well-known endpoint so that edge services verify the claim without a
//! network round-trip to the authority.
//!
//! # Seam
//!
//! Signing is decoupled via `ClaimSigner`.  The bundled `Ed25519ClaimSigner`
//! covers the common case; production deployments may substitute an HSM- or
//! KMS-backed implementation.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ── error ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum LicenseError {
    #[error("sign error: {0}")]
    Sign(String),
    #[error("token malformed: {0}")]
    Malformed(String),
    #[error("signature invalid")]
    InvalidSignature,
    #[error("token expired")]
    Expired,
    #[error("token not yet valid")]
    NotYetValid,
    #[error("key not found in JWKS: kid={0}")]
    KeyNotFound(String),
    #[error("unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
}

// ── claims ───────────────────────────────────────────────────────────────────

/// JWT payload issued on subscription activation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LicenseClaim {
    /// Subscription/tenant identifier (JWT `sub`).
    pub sub: String,
    /// Issued-at epoch (Unix seconds).
    pub iat: u64,
    /// Not-before epoch (Unix seconds).
    pub nbf: u64,
    /// Expiry epoch (Unix seconds).
    pub exp: u64,
    /// Billing cycle counter — monotonically increments with each renewal.
    pub epoch: u64,
    /// Capability entitlements granted for this billing period.
    pub entitlements: Vec<String>,
}

// ── issuance policy ──────────────────────────────────────────────────────────

/// Controls the temporal window written into every minted `LicenseClaim`.
#[derive(Debug, Clone)]
pub struct LicenseIssuancePolicy {
    /// Number of seconds the token remains valid after issuance.
    pub validity_secs: u64,
}

impl Default for LicenseIssuancePolicy {
    fn default() -> Self {
        Self {
            validity_secs: 86_400 * 30, // 30 days
        }
    }
}

// ── JWKS ─────────────────────────────────────────────────────────────────────

/// A single JSON Web Key (public half only — safe to publish).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwksKey {
    /// Key type: `"OKP"` for Ed25519.
    pub kty: String,
    /// Key identifier referenced in the JWT `kid` header.
    pub kid: String,
    /// Signature algorithm: `"EdDSA"`.
    pub alg: String,
    /// Intended use: always `"sig"`.
    #[serde(rename = "use")]
    pub use_: String,
    /// Algorithm-specific public key parameters (e.g. `crv`, `x` for OKP).
    #[serde(flatten)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// JSON Web Key Set — a list of public keys for offline token verification.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Jwks {
    pub keys: Vec<JwksKey>,
}

// ── Signer seam ───────────────────────────────────────────────────────────────

/// Abstract signing interface used by `LicenseAuthority`.
///
/// Implementors supply the raw signature bytes for a given message;
/// `LicenseAuthority` handles JWT framing and base64url encoding.
pub trait ClaimSigner: Send + Sync {
    fn key_id(&self) -> &str;
    fn algorithm_name(&self) -> &str;
    /// Return the public-key half of this signer formatted as a JWKS entry.
    fn public_jwk(&self) -> JwksKey;
    /// Sign `message` (the UTF-8 `header_b64.payload_b64` string bytes) and
    /// return the raw signature bytes.
    fn sign_bytes(&self, message: &[u8]) -> Result<Vec<u8>, LicenseError>;
}

// ── LicenseAuthority ──────────────────────────────────────────────────────────

/// Mints signed `LicenseClaim` JWTs on subscription activation.
pub struct LicenseAuthority {
    signer: Box<dyn ClaimSigner>,
    policy: LicenseIssuancePolicy,
}

impl LicenseAuthority {
    pub fn new(signer: impl ClaimSigner + 'static, policy: LicenseIssuancePolicy) -> Self {
        Self {
            signer: Box::new(signer),
            policy,
        }
    }

    /// Mint a signed JWT for the given subscription.
    ///
    /// `now_secs` is the current Unix timestamp in seconds; callers supply
    /// this explicitly so the authority is time-source–agnostic (easy testing,
    /// no hidden `SystemTime` dependency).
    pub fn mint(
        &self,
        sub: &str,
        epoch: u64,
        entitlements: Vec<String>,
        now_secs: u64,
    ) -> Result<String, LicenseError> {
        let claim = LicenseClaim {
            sub: sub.to_owned(),
            iat: now_secs,
            nbf: now_secs,
            exp: now_secs + self.policy.validity_secs,
            epoch,
            entitlements,
        };

        let header_json = serde_json::json!({
            "alg": self.signer.algorithm_name(),
            "typ": "JWT",
            "kid": self.signer.key_id(),
        });

        let header_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&header_json)
                .map_err(|e| LicenseError::Sign(e.to_string()))?
                .as_bytes(),
        );
        let payload_b64 = URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&claim)
                .map_err(|e| LicenseError::Sign(e.to_string()))?
                .as_bytes(),
        );

        let message = format!("{header_b64}.{payload_b64}");
        let sig_bytes = self.signer.sign_bytes(message.as_bytes())?;
        let sig_b64 = URL_SAFE_NO_PAD.encode(&sig_bytes);

        Ok(format!("{message}.{sig_b64}"))
    }

    /// Return the JWKS document containing this authority's public key.
    ///
    /// Publish at `/.well-known/jwks.json` (or equivalent) so that verifiers
    /// can perform offline signature checks without contacting the authority.
    pub fn jwks(&self) -> Jwks {
        Jwks {
            keys: vec![self.signer.public_jwk()],
        }
    }
}

// ── offline verification ──────────────────────────────────────────────────────

/// Verify a compact JWT issued by `LicenseAuthority` using only the JWKS.
///
/// Returns the decoded `LicenseClaim` on success.  `now_secs` is the current
/// Unix timestamp used for `nbf`/`exp` boundary checks.
pub fn verify_license_token(
    token: &str,
    jwks: &Jwks,
    now_secs: u64,
) -> Result<LicenseClaim, LicenseError> {
    let mut parts = token.splitn(3, '.');
    let header_b64 = parts
        .next()
        .ok_or_else(|| LicenseError::Malformed("missing header".into()))?;
    let payload_b64 = parts
        .next()
        .ok_or_else(|| LicenseError::Malformed("missing payload".into()))?;
    let sig_b64 = parts
        .next()
        .ok_or_else(|| LicenseError::Malformed("missing signature".into()))?;

    // Parse header to find kid and alg.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| LicenseError::Malformed(format!("header base64: {e}")))?;
    let header: serde_json::Value = serde_json::from_slice(&header_bytes)
        .map_err(|e| LicenseError::Malformed(format!("header json: {e}")))?;
    let kid = header["kid"]
        .as_str()
        .ok_or_else(|| LicenseError::Malformed("missing kid".into()))?;
    let alg = header["alg"]
        .as_str()
        .ok_or_else(|| LicenseError::Malformed("missing alg".into()))?;

    // Locate the matching key in the JWKS.
    let jwk = jwks
        .keys
        .iter()
        .find(|k| k.kid == kid)
        .ok_or_else(|| LicenseError::KeyNotFound(kid.to_owned()))?;

    if jwk.alg != alg {
        return Err(LicenseError::UnsupportedAlgorithm(format!(
            "JWKS key alg {} ≠ token alg {}",
            jwk.alg, alg
        )));
    }

    // Verify signature over the raw header.payload ASCII bytes.
    let message = format!("{header_b64}.{payload_b64}");
    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| LicenseError::Malformed(format!("sig base64: {e}")))?;

    verify_signature(alg, jwk, message.as_bytes(), &sig)?;

    // Decode and validate claims.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| LicenseError::Malformed(format!("payload base64: {e}")))?;
    let claim: LicenseClaim = serde_json::from_slice(&payload_bytes)
        .map_err(|e| LicenseError::Malformed(format!("payload json: {e}")))?;

    if now_secs < claim.nbf {
        return Err(LicenseError::NotYetValid);
    }
    if now_secs >= claim.exp {
        return Err(LicenseError::Expired);
    }

    Ok(claim)
}

fn verify_signature(
    alg: &str,
    jwk: &JwksKey,
    message: &[u8],
    sig: &[u8],
) -> Result<(), LicenseError> {
    match alg {
        "EdDSA" => verify_eddsa(jwk, message, sig),
        other => Err(LicenseError::UnsupportedAlgorithm(other.to_owned())),
    }
}

fn verify_eddsa(jwk: &JwksKey, message: &[u8], sig: &[u8]) -> Result<(), LicenseError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let x = jwk
        .params
        .get("x")
        .and_then(|v| v.as_str())
        .ok_or_else(|| LicenseError::Malformed("EdDSA JWK missing 'x'".into()))?;

    let key_bytes = URL_SAFE_NO_PAD
        .decode(x)
        .map_err(|e| LicenseError::Malformed(format!("EdDSA x base64: {e}")))?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| LicenseError::Malformed("EdDSA x must be 32 bytes".into()))?;

    let verifying_key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| LicenseError::Malformed(format!("EdDSA verifying key: {e}")))?;

    let sig_bytes: [u8; 64] = sig
        .try_into()
        .map_err(|_| LicenseError::Malformed("EdDSA signature must be 64 bytes".into()))?;
    let signature = Signature::from_bytes(&sig_bytes);

    verifying_key
        .verify(message, &signature)
        .map_err(|_| LicenseError::InvalidSignature)
}

// ── Ed25519ClaimSigner ────────────────────────────────────────────────────────

/// Ed25519 implementation of `ClaimSigner` using a 32-byte seed.
///
/// For production, derive the seed from a KMS-managed key material export.
/// For tests, use any fixed 32-byte value; the signer is deterministic.
pub struct Ed25519ClaimSigner {
    key_id: String,
    signing_key: ed25519_dalek::SigningKey,
}

impl Ed25519ClaimSigner {
    /// Construct from an explicit 32-byte seed.  The same seed always
    /// produces the same key pair, which makes this suitable for tests and
    /// for scenarios where the key is derived from stable KMS material.
    pub fn from_seed(key_id: impl Into<String>, seed: &[u8; 32]) -> Self {
        Self {
            key_id: key_id.into(),
            signing_key: ed25519_dalek::SigningKey::from_bytes(seed),
        }
    }
}

impl ClaimSigner for Ed25519ClaimSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn algorithm_name(&self) -> &str {
        "EdDSA"
    }

    fn public_jwk(&self) -> JwksKey {
        let vk = self.signing_key.verifying_key();
        let x = URL_SAFE_NO_PAD.encode(vk.as_bytes());
        let mut params = serde_json::Map::new();
        params.insert("crv".into(), "Ed25519".into());
        params.insert("x".into(), x.into());
        JwksKey {
            kty: "OKP".into(),
            kid: self.key_id.clone(),
            alg: "EdDSA".into(),
            use_: "sig".into(),
            params,
        }
    }

    fn sign_bytes(&self, message: &[u8]) -> Result<Vec<u8>, LicenseError> {
        use ed25519_dalek::Signer as _;
        let sig = self.signing_key.sign(message);
        Ok(sig.to_bytes().to_vec())
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &[u8; 32] = b"awaken-license-test-seed-000000!";
    const NOW: u64 = 1_700_000_000;

    fn authority() -> LicenseAuthority {
        LicenseAuthority::new(
            Ed25519ClaimSigner::from_seed("key-1", SEED),
            LicenseIssuancePolicy {
                validity_secs: 3_600,
            },
        )
    }

    #[test]
    fn mint_produces_three_part_jwt() {
        let auth = authority();
        let token = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        assert_eq!(token.split('.').count(), 3, "compact JWT must have 3 parts");
    }

    #[test]
    fn round_trip_offline_verify() {
        let auth = authority();
        let token = auth
            .mint("tenant-abc", 1, vec!["pro".into(), "api".into()], NOW)
            .unwrap();
        let jwks = auth.jwks();
        let claim = verify_license_token(&token, &jwks, NOW + 60).unwrap();

        assert_eq!(claim.sub, "tenant-abc");
        assert_eq!(claim.epoch, 1);
        assert_eq!(claim.entitlements, vec!["pro", "api"]);
        assert_eq!(claim.iat, NOW);
        assert_eq!(claim.exp, NOW + 3_600);
    }

    #[test]
    fn verify_rejects_expired_token() {
        let auth = authority();
        let token = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        let jwks = auth.jwks();
        // Advance clock past expiry.
        let err = verify_license_token(&token, &jwks, NOW + 3_601).unwrap_err();
        assert!(matches!(err, LicenseError::Expired), "got: {err}");
    }

    #[test]
    fn verify_rejects_not_yet_valid() {
        let auth = authority();
        let token = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        let jwks = auth.jwks();
        // Check before nbf (same as iat here).
        let err = verify_license_token(&token, &jwks, NOW - 1).unwrap_err();
        assert!(matches!(err, LicenseError::NotYetValid), "got: {err}");
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let auth = authority();
        let token = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        let jwks = auth.jwks();

        // Flip one character in the middle segment.
        let mut parts: Vec<&str> = token.splitn(3, '.').collect();
        let mut tampered = parts[1].to_string();
        let idx = tampered.len() / 2;
        tampered.replace_range(
            idx..idx + 1,
            if &tampered[idx..idx + 1] == "A" {
                "B"
            } else {
                "A"
            },
        );
        let bad_token = format!("{}.{}.{}", parts[0], tampered, parts[2]);
        parts[1] = &bad_token; // keep borrow alive

        let err = verify_license_token(&bad_token, &jwks, NOW + 60).unwrap_err();
        assert!(
            matches!(
                err,
                LicenseError::InvalidSignature | LicenseError::Malformed(_)
            ),
            "got: {err}"
        );
    }

    #[test]
    fn verify_rejects_unknown_kid() {
        let auth = authority();
        let token = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        // Empty JWKS has no matching key.
        let empty_jwks = Jwks::default();
        let err = verify_license_token(&token, &empty_jwks, NOW + 60).unwrap_err();
        assert!(matches!(err, LicenseError::KeyNotFound(_)), "got: {err}");
    }

    #[test]
    fn jwks_roundtrip_via_json() {
        let auth = authority();
        let jwks = auth.jwks();
        let json = serde_json::to_string(&jwks).unwrap();
        let decoded: Jwks = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.keys.len(), 1);
        assert_eq!(decoded.keys[0].kid, "key-1");
        assert_eq!(decoded.keys[0].alg, "EdDSA");
        assert_eq!(decoded.keys[0].kty, "OKP");
    }

    #[test]
    fn different_epochs_produce_different_tokens() {
        let auth = authority();
        let t1 = auth.mint("tenant-abc", 1, vec!["pro".into()], NOW).unwrap();
        let t2 = auth.mint("tenant-abc", 2, vec!["pro".into()], NOW).unwrap();
        assert_ne!(t1, t2);
        // Both must still verify.
        let jwks = auth.jwks();
        verify_license_token(&t1, &jwks, NOW + 60).unwrap();
        verify_license_token(&t2, &jwks, NOW + 60).unwrap();
    }
}
