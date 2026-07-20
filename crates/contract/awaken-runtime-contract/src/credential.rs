//! Credential access across the config-to-execution boundary.
//!
//! A published snapshot carries [`CredentialAccess`], which is durable and
//! secret-free.  Immediately before an outbound call, the execution composition
//! turns that descriptor into an [`InjectedCredential`].  The latter deliberately
//! has no serde implementation, so plaintext can never enter a snapshot, queue row,
//! or idempotency record.

use awaken_agent_contract::RedactedString;
use serde::{Deserialize, Serialize};

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub revision: u64,
}

/// How credential material crosses into the execution boundary.
///
/// These are capability descriptions, not deployment modes: no variant names a
/// local, cloud, or gateway topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialInjectionKind {
    /// Exchange an opaque reference for request-scoped material.
    Reference,
    /// Deliver material encrypted to the selected workload identity.
    SealedEnvelope,
    /// Hand material across an already-trusted in-process boundary.
    Direct,
}

/// The injection order fixed by config resolution.
///
/// Runtime infrastructure may try only these entries, in this order.  An outage
/// cannot silently broaden the policy to a weaker delivery mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialInjectionPolicy {
    pub allowed: Vec<CredentialInjectionKind>,
}

impl<'de> Deserialize<'de> for CredentialInjectionPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            allowed: Vec<CredentialInjectionKind>,
        }

        let policy = Self {
            allowed: Wire::deserialize(deserializer)?.allowed,
        };
        policy.validate().map_err(serde::de::Error::custom)?;
        Ok(policy)
    }
}

impl CredentialInjectionPolicy {
    /// Construct a non-empty, duplicate-free policy.
    pub fn new(
        preferred: CredentialInjectionKind,
        fallbacks: impl IntoIterator<Item = CredentialInjectionKind>,
    ) -> Result<Self, CredentialPolicyError> {
        let mut allowed = vec![preferred];
        for fallback in fallbacks {
            if allowed.contains(&fallback) {
                return Err(CredentialPolicyError::Duplicate(fallback));
            }
            allowed.push(fallback);
        }
        Ok(Self { allowed })
    }

    #[must_use]
    pub fn preferred(&self) -> CredentialInjectionKind {
        // Construction and deserialization validation guarantee non-empty.  Keep
        // this total for values assembled inside this crate as well.
        self.allowed
            .first()
            .copied()
            .unwrap_or(CredentialInjectionKind::Reference)
    }

    #[must_use]
    pub fn allows(&self, kind: CredentialInjectionKind) -> bool {
        self.allowed.contains(&kind)
    }

    /// Validate a value read from durable storage or the wire.
    pub fn validate(&self) -> Result<(), CredentialPolicyError> {
        if self.allowed.is_empty() {
            return Err(CredentialPolicyError::Empty);
        }
        let mut seen = std::collections::HashSet::new();
        for kind in &self.allowed {
            if !seen.insert(*kind) {
                return Err(CredentialPolicyError::Duplicate(*kind));
            }
        }
        Ok(())
    }
}

/// A malformed credential-injection policy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialPolicyError {
    #[error("credential injection policy must allow at least one mechanism")]
    Empty,
    #[error("credential injection mechanism {0:?} is listed more than once")]
    Duplicate(CredentialInjectionKind),
}

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUsage {
    /// The selected inference adapter owns the protocol-specific placement
    /// (for example `Authorization: Bearer` or `x-api-key`).
    ProviderAdapter,
    HttpHeader {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    QueryParameter {
        name: String,
    },
    ClientCertificate,
    EnvironmentVariable {
        name: String,
    },
    File {
        path: String,
    },
}

/// Secret-free credential instructions pinned into an executable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialAccess {
    pub credential: CredentialRef,
    pub injection: CredentialInjectionPolicy,
    pub usage: CredentialUsage,
}

impl CredentialAccess {
    pub fn validate(&self) -> Result<(), CredentialPolicyError> {
        self.injection.validate()
    }
}

/// Request-ready secret material.  This type is intentionally not serializable.
#[derive(Clone)]
pub struct InjectedCredential {
    pub material: RedactedString,
    pub usage: CredentialUsage,
    pub expires_at_ms: Option<u64>,
}

impl std::fmt::Debug for InjectedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InjectedCredential")
            .field("material", &"***")
            .field("usage", &self.usage)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_policy_preserves_preference_and_rejects_implicit_broadening() {
        let policy = CredentialInjectionPolicy::new(
            CredentialInjectionKind::Reference,
            [CredentialInjectionKind::SealedEnvelope],
        )
        .unwrap();
        assert_eq!(policy.preferred(), CredentialInjectionKind::Reference);
        assert!(policy.allows(CredentialInjectionKind::SealedEnvelope));
        assert!(!policy.allows(CredentialInjectionKind::Direct));

        let duplicate = CredentialInjectionPolicy::new(
            CredentialInjectionKind::Direct,
            [CredentialInjectionKind::Direct],
        );
        assert_eq!(
            duplicate,
            Err(CredentialPolicyError::Duplicate(
                CredentialInjectionKind::Direct
            ))
        );
    }

    #[test]
    fn injected_credential_debug_never_contains_secret_material() {
        let injected = InjectedCredential {
            material: RedactedString::new("do-not-log-this-secret"),
            usage: CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            expires_at_ms: Some(42),
        };
        let debug = format!("{injected:?}");
        assert!(!debug.contains("do-not-log-this-secret"));
        assert!(debug.contains("***"));
    }

    #[test]
    fn durable_policy_decode_rejects_empty_and_duplicate_downgrade_lists() {
        let empty = serde_json::from_value::<CredentialInjectionPolicy>(serde_json::json!({
            "allowed": []
        }));
        assert!(empty.unwrap_err().to_string().contains("at least one"));

        let duplicate = serde_json::from_value::<CredentialInjectionPolicy>(serde_json::json!({
            "allowed": ["reference", "reference"]
        }));
        assert!(
            duplicate
                .unwrap_err()
                .to_string()
                .contains("more than once")
        );
    }
}
