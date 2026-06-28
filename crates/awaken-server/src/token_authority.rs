//! Server-side access-token signing authority.
//!
//! [`AccessTokenAuthority`] holds the server's own key material and exposes
//! the public JWKS for relying-party trust via [`AccessTokenAuthority::jwks`].
//! Relying parties fetch `/.well-known/jwks.json` to obtain the key set and
//! verify tokens offline.
//!
//! **Provisional** — JWKS is not yet promoted to the public facade (Gap E);
//! this type lives directly in `awaken-server` until the facade contract is
//! extended.

use jsonwebtoken::jwk::JwkSet;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Configuration for the server's own access-token signing authority.
///
/// Set `token_authority` inside `AdminApiConfig` to enable the
/// `/.well-known/jwks.json` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenAuthorityConfig {
    /// Public key set to publish at `/.well-known/jwks.json`.
    ///
    /// Must be a valid JWKS document: `{ "keys": [...] }`.  The operator
    /// generates the key pair externally and supplies only the public JWKS
    /// here; the private key is used by whichever component mints tokens.
    pub jwks: JwkSet,
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TokenAuthorityError {
    #[error("invalid JWKS document: {0}")]
    InvalidJwks(String),
}

// ---------------------------------------------------------------------------
// AccessTokenAuthority
// ---------------------------------------------------------------------------

/// Owns the server's public access-token key material.
///
/// Construct via [`AccessTokenAuthority::new`] and share as
/// `Arc<AccessTokenAuthority>` from [`crate::app::AdminModuleState`].
/// The `/.well-known/jwks.json` handler calls [`Self::jwks`] to obtain the
/// key set document returned to relying parties.
pub struct AccessTokenAuthority {
    jwks: JwkSet,
}

impl AccessTokenAuthority {
    /// Build an authority from a config value.
    pub fn new(config: TokenAuthorityConfig) -> Self {
        Self { jwks: config.jwks }
    }

    /// Return the public JWKS.
    ///
    /// Relying parties use this to verify tokens offline.
    pub fn jwks(&self) -> &JwkSet {
        &self.jwks
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_jwks_json() -> serde_json::Value {
        serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "kid": "test-key-1",
                "n": "sIwr-LPqPFMILBQ4Y4mEHM8VJHPTqbFXcL7r_NG4V9MOhz56yWzxXWEgF9yvl0LJMzgq1iFTQcEn-C-jI0YM_XCBmN2rnzMoG2z4X9MtFvFjilkF7bBaJ7IQ5lKkbf7YXGAr_PU2Z6X9KtW5-zKDkTtQk",
                "e": "AQAB",
                "alg": "RS256"
            }]
        })
    }

    #[test]
    fn new_from_config_roundtrip() {
        let json = minimal_jwks_json();
        let key_set: JwkSet = serde_json::from_value(json.clone()).expect("valid JWKS");
        let config = TokenAuthorityConfig { jwks: key_set };
        let authority = AccessTokenAuthority::new(config);

        let returned = authority.jwks();
        assert_eq!(returned.keys.len(), 1);
        assert_eq!(
            returned.keys[0].common.key_id.as_deref(),
            Some("test-key-1")
        );
    }

    #[test]
    fn jwks_serializes_to_standard_document() {
        let json = minimal_jwks_json();
        let key_set: JwkSet = serde_json::from_value(json).expect("valid JWKS");
        let authority = AccessTokenAuthority::new(TokenAuthorityConfig { jwks: key_set });
        let serialized = serde_json::to_value(authority.jwks()).expect("serializable");
        assert!(serialized.get("keys").is_some(), "must have 'keys' field");
    }
}
