//! Credential HTTP wire types and secret-free source projection.
//!
//! Route handlers remain in the parent adapter; this module owns the one wire
//! representation so hosted helpers, OpenAPI, and CRUD cannot define parallel
//! request or response shapes.

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{CredentialDescriptor, CredentialSourceId};
use awaken_credential_vault::{
    CredentialError, CredentialKind, CredentialSource, CredentialStatus, OAuthHelper,
};
use sha2::{Digest, Sha256};

/// The credential-entry wire body. `secret` is write-only: it is sealed into the
/// SecretStore and never appears on any response. `RedactedString` is
/// intentionally not `Deserialize`, so the raw secret crosses the wire once.
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnterCredentialRequest {
    pub(super) workspace_id: String,
    /// Stable hosted-governance operation identity. When present, the canonical
    /// provider comes from `descriptor` or the legacy `provider_id`; described
    /// writes reject `provider_id` so that tuple has one provider truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) idempotency_key: Option<String>,
    pub(super) kind: CredentialKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) provider_id: Option<String>,
    /// Canonical secret-free provider, material, and exact target/usage facts.
    /// Legacy clients may omit it; described clients receive it on exact reads
    /// and must omit the mutually exclusive legacy `provider_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) descriptor: Option<CredentialDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) env_key: Option<String>,
    /// The secret to seal — required for `vault`. Environment-backed credentials
    /// are not accepted; environment discovery is exposed only as proposals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) secret: Option<String>,
    /// Structured material sealed as one versioned Vault document. Mutually
    /// exclusive with the legacy `secret` field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) material: Option<CredentialMaterialInput>,
    /// A server-owned OAuth refresh helper. This is an allowlisted identifier,
    /// never an operator-supplied command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) oauth_helper: Option<OAuthHelper>,
}

/// Rotate the primary material of one exact active Vault credential revision.
/// Scalar or typed material is write-only and the response remains secret-free.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RotateCredentialRequest {
    pub(super) expected_version: i64,
    #[serde(default)]
    pub(super) secret: Option<String>,
    #[serde(default)]
    pub(super) material: Option<CredentialMaterialInput>,
    /// Replacement metadata published by the same expected-version CAS.
    #[serde(default)]
    pub(super) descriptor: Option<CredentialDescriptor>,
}

/// Retire one exact credential revision. The expected version is mandatory so
/// an operator cannot erase material that was rotated after its read.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArchiveCredentialRequest {
    pub(super) expected_version: i64,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub(super) struct CredentialMaterialInput {
    /// Namespaced, versioned type owned by the installed consumer extension.
    pub(super) type_id: String,
    /// Opaque named secret fields; the matching consumer owns validation.
    pub(super) fields: std::collections::BTreeMap<String, String>,
}

impl CredentialMaterialInput {
    fn encode(self) -> Result<RedactedString, CredentialError> {
        let material = awaken_credential_vault::StructuredCredentialMaterial {
            type_id: self.type_id,
            fields: self
                .fields
                .into_iter()
                .map(|(name, value)| (name, RedactedString::new(value)))
                .collect(),
        };
        awaken_credential_vault::encode_structured_material(material)
    }
}

pub(super) fn credential_material(
    secret: Option<String>,
    material: Option<CredentialMaterialInput>,
) -> Result<Option<RedactedString>, CredentialError> {
    let secret = secret.filter(|secret| !secret.is_empty());
    match (secret, material) {
        (Some(_), Some(_)) => Err(CredentialError::InvalidSource(
            "secret and structured material are mutually exclusive".into(),
        )),
        (Some(secret), None) => Ok(Some(RedactedString::new(secret))),
        (None, Some(material)) => material.encode().map(Some),
        (None, None) => Ok(None),
    }
}

/// Secret-free credential projection. Internal token-source argv and Vault refs
/// never cross the admin boundary; consumers bind this stable source id.
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CredentialSourceView {
    pub id: CredentialSourceId,
    pub workspace_id: String,
    pub kind: CredentialKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<CredentialDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_endpoint_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_helper: Option<OAuthHelper>,
    pub status: CredentialStatus,
    pub version: i64,
}

pub(super) fn hosted_credential_source_id(
    workspace_id: &str,
    provider_ref: &str,
    idempotency_key: &str,
) -> CredentialSourceId {
    let mut digest = Sha256::new();
    for part in [
        "hosted-governance-credential-v1",
        workspace_id,
        provider_ref,
        idempotency_key,
    ] {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    CredentialSourceId(format!("cred:hosted-business:{:x}", digest.finalize()))
}

pub(super) fn validate_hosted_credential_identity(
    workspace_id: &str,
    provider_ref: &str,
    idempotency_key: &str,
) -> Result<(), CredentialError> {
    if workspace_id.trim().is_empty()
        || provider_ref.trim().is_empty()
        || idempotency_key.trim().is_empty()
        || idempotency_key.len() > 200
    {
        return Err(CredentialError::InvalidSource(
            "hosted credential identity requires a Workspace, provider_ref, and 1..200 character idempotency_key"
                .into(),
        ));
    }
    Ok(())
}

pub(super) fn is_hosted_credential(source: &CredentialSource) -> bool {
    source.id.0.starts_with("cred:hosted-business:")
}

pub(super) fn hosted_credential_matches(
    source: &CredentialSource,
    workspace_id: &str,
    provider_ref: &str,
) -> bool {
    source.workspace_id == workspace_id
        && source
            .authorization_scope()
            .belongs_to_provider(provider_ref)
        && source.kind == CredentialKind::Vault
        && source.status == CredentialStatus::Active
        && source.material_ref.is_some()
        && source.worker_local_binding.is_none()
}

impl From<CredentialSource> for CredentialSourceView {
    fn from(source: CredentialSource) -> Self {
        let oauth_helper = source
            .oauth_command
            .as_deref()
            .and_then(OAuthHelper::from_command);
        Self {
            id: source.id,
            workspace_id: source.workspace_id,
            kind: source.kind,
            descriptor: source.descriptor,
            provider_id: source.provider_id,
            protocol_endpoint_id: source.protocol_endpoint_id,
            env_key: source.env_key,
            oauth_helper,
            status: source.status,
            version: source.version,
        }
    }
}
