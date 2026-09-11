//! Exact material shapes and the sole neutral material-resolution port.
use std::collections::BTreeSet;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use serde::{Deserialize, Serialize};

use super::{
    CredentialAccess, CredentialMaterialBinding, CredentialMaterialSource, CredentialRef,
    CredentialUsage, HTTP_BASIC_MATERIAL_TYPE, PlaintextHolder, StructuredCredentialMaterial,
    http_effect,
};

/// Exact OAuth token set owned by a provider driver. Refresh tokens never enter
/// snapshots, observations, receipts, logs, or generic process configuration.
#[derive(Debug)]
pub struct OAuthCredentialMaterial {
    pub access_token: RedactedString,
    pub refresh_token: RedactedString,
    pub expires_at_unix_ms: Option<u64>,
    pub account_id: Option<String>,
    pub account_plan: Option<String>,
}

/// Capability-discovery record published by an external last-mile consumer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialExtensionDescriptor {
    pub consumer_id: String,
    pub material_types: BTreeSet<String>,
}

/// Exact, already-admitted input to an external credential consumer. The
/// consumer cannot select another credential, holder, usage, or target.
pub struct CredentialExtensionRequest<'a> {
    pub access: &'a CredentialAccess,
    pub material: &'a StructuredCredentialMaterial,
    pub binding: &'a CredentialMaterialBinding,
}

/// Secret-free proof that one extension consumed the exact material type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialExtensionReceipt {
    pub consumer_id: String,
    pub material_type: String,
    pub target_use_fingerprint: String,
}

/// Open last-mile port for SSH agents, database drivers, signing services, and
/// other externally supplied consumers. Built-in environment/file delivery
/// remains owned by the provisioning SecretBroker and does not pass this port.
#[async_trait]
pub trait CredentialExtensionConsumer: Send + Sync {
    fn descriptor(&self) -> CredentialExtensionDescriptor;

    async fn consume(
        &self,
        request: CredentialExtensionRequest<'_>,
    ) -> Result<CredentialExtensionReceipt, CredentialMaterialError>;
}

/// Material shape returned by the sole exact resolver port. Provider drivers
/// consume OAuth as a bundle; extension-owned schemas remain typed and opaque.
/// Credential material deliberately has no Serde implementation: attempting to
/// place it on any durable JSON path is a compile-time error.
///
/// ```compile_fail
/// use awaken_agent_contract::RedactedString;
/// use awaken_credential_contract::CredentialMaterial;
///
/// let material = CredentialMaterial::secret(RedactedString::new("credential-plaintext"));
/// let _ = serde_json::to_vec(&material);
/// ```
#[derive(Debug)]
pub enum CredentialMaterial {
    /// One opaque secret. Whether it becomes an API-key header, a Bearer header,
    /// or a process secret is decided exclusively by [`CredentialUsage`].
    Secret(RedactedString),
    Structured(StructuredCredentialMaterial),
    OAuth(OAuthCredentialMaterial),
}

impl CredentialMaterial {
    #[must_use]
    pub fn secret(value: RedactedString) -> Self {
        Self::Secret(value)
    }

    pub fn single_secret(&self) -> Result<&RedactedString, CredentialMaterialError> {
        match self {
            Self::Secret(value) => Ok(value),
            Self::OAuth(bundle) => Ok(&bundle.access_token),
            Self::Structured(_) => Err(CredentialMaterialError::MaterialKindMismatch),
        }
    }

    pub fn into_secret(self) -> Result<RedactedString, CredentialMaterialError> {
        match self {
            Self::Secret(value) => Ok(value),
            Self::Structured(_) | Self::OAuth(_) => {
                Err(CredentialMaterialError::MaterialKindMismatch)
            }
        }
    }

    /// Enforce the one material/application compatibility table before a
    /// last-mile adapter can observe plaintext.
    pub fn validate_usage(&self, usage: &CredentialUsage) -> Result<(), CredentialMaterialError> {
        usage
            .validate()
            .map_err(|_| CredentialMaterialError::MaterialKindMismatch)?;
        // A core delivery carrier transports material without parsing it, so
        // it admits every resolved shape. An external extension consumer must
        // still pin the exact material type it knows how to read.
        if usage.is_core_delivery() {
            return Ok(());
        }
        let valid = match (self, usage) {
            (Self::Structured(material), CredentialUsage::HttpBasicAuth) => {
                material.type_id == HTTP_BASIC_MATERIAL_TYPE
                    && material.fields.len() == 2
                    && material.fields.contains_key("username")
                    && material.fields.contains_key("password")
            }
            (
                Self::Structured(material),
                CredentialUsage::Extension {
                    material_type,
                    consumer_id,
                    ..
                },
            ) => {
                !consumer_id.trim().is_empty()
                    && !material_type.trim().is_empty()
                    && material.type_id == *material_type
            }
            (
                Self::Secret(_),
                CredentialUsage::ProviderAdapter
                | CredentialUsage::HttpHeader { .. }
                | CredentialUsage::QueryParameter { .. },
            )
            | (Self::OAuth(_), CredentialUsage::ProviderAdapter) => true,
            (Self::Secret(_), CredentialUsage::HttpEffect { fields }) => {
                http_effect::named_material_shape_is_exact(
                    http_effect::NamedMaterialShape::Secret,
                    fields.len(),
                    false,
                )
            }
            (Self::Structured(material), CredentialUsage::HttpEffect { fields }) => {
                http_effect::named_material_shape_is_exact(
                    http_effect::NamedMaterialShape::Structured,
                    fields.len(),
                    material.fields.keys().eq(fields.keys()),
                )
            }
            (Self::Secret(_), CredentialUsage::SignatureVerification { fields }) => {
                http_effect::named_material_shape_is_exact(
                    http_effect::NamedMaterialShape::Secret,
                    fields.len(),
                    false,
                )
            }
            (Self::Structured(material), CredentialUsage::SignatureVerification { fields }) => {
                http_effect::named_material_shape_is_exact(
                    http_effect::NamedMaterialShape::Structured,
                    fields.len(),
                    material.fields.keys().eq(fields.keys()),
                )
            }
            _ => false,
        };
        valid
            .then_some(())
            .ok_or(CredentialMaterialError::MaterialKindMismatch)
    }
}

/// Material returned only to the exact selected holder by a resolver adapter.
#[derive(Debug)]
pub struct ResolvedCredentialMaterial {
    pub credential: CredentialRef,
    pub holder: PlaintextHolder,
    pub material: CredentialMaterial,
}

/// One already-selected resolution request. This is the complete input to every
/// Control reference, Worker-private, or recipient-bound envelope adapter.
#[derive(Debug, Clone, Copy)]
pub struct CredentialMaterialRequest<'a> {
    pub access: &'a CredentialAccess,
    pub selected_holder: &'a PlaintextHolder,
    pub binding: &'a CredentialMaterialBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialMaterialError {
    #[error("credential material unavailable")]
    Unavailable,
    #[error("credential material revision mismatch")]
    RevisionMismatch,
    #[error("credential material recipient mismatch")]
    RecipientMismatch,
    #[error("credential material envelope expired")]
    EnvelopeExpired,
    #[error("credential material target binding mismatch")]
    BindingMismatch,
    #[error("credential material payload fingerprint mismatch")]
    PayloadMismatch,
    #[error("credential material resolver returned another credential or holder")]
    ResolverMismatch,
    #[error("credential material kind is unsupported by the selected driver")]
    MaterialKindMismatch,
    #[error("credential login is required")]
    LoginRequired,
    #[error("credential is expired")]
    Expired,
    #[error("credential is invalid")]
    Invalid,
    #[error("credential is disabled")]
    Disabled,
    #[error("local credential probe failed")]
    ProbeFailed,
}

/// One indivisible installed material-resolution capability. This is adapter
/// evidence, not policy or credential selection. Envelope presence is exact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CredentialMaterialCapability {
    pub holder: PlaintextHolder,
    pub source: CredentialMaterialSource,
    pub envelope: bool,
}

/// Sole neutral source port for an already-selected exact credential access.
#[async_trait]
pub trait CredentialMaterialResolver: Send + Sync {
    /// Exact installed material tuples. Empty is fail-closed; a source or
    /// envelope capability cannot be borrowed from a different holder/profile.
    fn material_capabilities(&self) -> BTreeSet<CredentialMaterialCapability> {
        BTreeSet::new()
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Cause-effect graph: material shape and application usage are independent
    /// inputs; only an explicitly supported pair may reach a last-mile adapter.
    /// Typed material must never degrade to one opaque secret, while external
    /// consumers are admitted only for an exact namespaced material type.
    ///
    /// | Rule | material | usage | effect |
    /// |---|---|---|---|
    /// | M1 | Secret | HTTP header | admitted |
    /// | M2 | http-basic typed | HTTP Basic | admitted |
    /// | M3 | http-basic typed | HTTP header | rejected |
    /// | M4 | Secret | HTTP Basic | rejected |
    /// | M5 | OAuth | Provider adapter | admitted |
    /// | M6 | external type A | extension expecting A | admitted |
    /// | M7 | external type A | extension expecting B | rejected |
    #[test]
    fn material_usage_compatibility_is_explicit_and_fail_closed() {
        let secret = || CredentialMaterial::secret(RedactedString::new("secret"));
        let typed = |type_id: &str| {
            CredentialMaterial::Structured(StructuredCredentialMaterial {
                type_id: type_id.into(),
                fields: BTreeMap::from([
                    ("username".into(), RedactedString::new("user")),
                    ("password".into(), RedactedString::new("password")),
                ]),
            })
        };
        let oauth = || {
            CredentialMaterial::OAuth(OAuthCredentialMaterial {
                access_token: RedactedString::new("access"),
                refresh_token: RedactedString::new("refresh"),
                expires_at_unix_ms: None,
                account_id: None,
                account_plan: None,
            })
        };
        let header = CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        let rules = [
            ("M1", secret().validate_usage(&header), true),
            (
                "M2",
                typed(HTTP_BASIC_MATERIAL_TYPE).validate_usage(&CredentialUsage::HttpBasicAuth),
                true,
            ),
            (
                "M3",
                typed(HTTP_BASIC_MATERIAL_TYPE).validate_usage(&header),
                false,
            ),
            (
                "M4",
                secret().validate_usage(&CredentialUsage::HttpBasicAuth),
                false,
            ),
            (
                "M5",
                oauth().validate_usage(&CredentialUsage::ProviderAdapter),
                true,
            ),
            (
                "M6",
                typed("acme.ssh-key/v1").validate_usage(&CredentialUsage::Extension {
                    consumer_id: "acme.ssh-agent/v1".into(),
                    material_type: "acme.ssh-key/v1".into(),
                    public_config: serde_json::json!({"identity_path":"/run/secrets/id"}),
                }),
                true,
            ),
            (
                "M7",
                typed("acme.ssh-key/v1").validate_usage(&CredentialUsage::Extension {
                    consumer_id: "acme.ssh-agent/v1".into(),
                    material_type: "acme.other/v1".into(),
                    public_config: serde_json::Value::Null,
                }),
                false,
            ),
        ];
        for (id, result, admitted) in rules {
            assert_eq!(result.is_ok(), admitted, "decision rule {id}: {result:?}");
        }
    }

    /// Coverage rationale: a structured pair has no meaningful single-secret
    /// projection. Rejecting the conversion prevents callers from silently using
    /// the password as an API key or Bearer token.
    #[test]
    fn structured_credentials_cannot_be_downgraded_to_one_secret() {
        let material = CredentialMaterial::Structured(StructuredCredentialMaterial {
            type_id: "acme.ssh-key/v1".into(),
            fields: BTreeMap::from([("private_key".into(), RedactedString::new("pem"))]),
        });
        assert_eq!(
            material.single_secret().unwrap_err(),
            CredentialMaterialError::MaterialKindMismatch
        );
    }
}
