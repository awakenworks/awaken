// @formal surface requirement=credential.lifecycle.credential_lifecycle
// @formal reason: Static owner: the single admin credential CRUD wire representation and secret-free
// @formal reason: source projection. Dynamic path: parent routes consume its create, rotate, archive,
// @formal reason: idempotency, replacement, descriptor, material, and revision facts, convert write-only
// @formal reason: material into the canonical redacted boundary, and return no internal material
// @formal reason: reference. Evidence: credential.lifecycle.credential_lifecycle and adjacent admin
// @formal reason: credential route tests; this is a product-requirement classification, not a direct
// @formal reason: formal-proof claim.
//! Credential HTTP wire types and secret-free source projection.
//!
//! Route handlers remain in the parent adapter; this module owns the one wire
//! representation so hosted helpers, OpenAPI, and CRUD cannot define parallel
//! request or response shapes.

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::CredentialRef;
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
#[serde(deny_unknown_fields)]
pub struct EnterCredentialRequest {
    pub(super) workspace_id: String,
    /// Stable hosted-governance operation identity. When present, the canonical
    /// provider comes from `descriptor` or the legacy `provider_id`; described
    /// writes reject `provider_id` so that tuple has one provider truth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) idempotency_key: Option<String>,
    /// Exact active predecessor fenced while this request publishes a distinct
    /// deterministic source. The predecessor is not mutated or retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) replacement_of: Option<CredentialRef>,
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
    /// Write-only auxiliary inputs declared by the acquisition profile. The
    /// canonical Vault entry validates their exact slots and seals them with
    /// the primary material; no response contains these values.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) auxiliary_materials: std::collections::BTreeMap<String, String>,
    /// Optional initial grant evidence, stored with the same sealed generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) grant_lifetime: Option<awaken_credential_contract::CredentialGrantLifetime>,
    /// A server-owned OAuth refresh helper. This is an allowlisted identifier,
    /// never an operator-supplied command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) oauth_helper: Option<OAuthHelper>,
}

/// Rotate the primary material of one exact active Vault credential revision.
/// Scalar or typed material is write-only and the response remains secret-free.
#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
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
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ArchiveCredentialRequest {
    pub expected_version: i64,
}

/// Renew an exact described source under the original hosted operation identity.
/// This command carries no token, password or auxiliary secret value.
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RenewCredentialRequest {
    pub workspace_id: String,
    pub expected_version: u64,
    pub idempotency_key: String,
    pub descriptor: CredentialDescriptor,
}

/// Exact metadata-only observation; it never grants materialization authority.
#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CredentialRenewalObservation {
    pub workspace_id: String,
    pub reference: CredentialRef,
    pub status: Option<awaken_credential_contract::CredentialRenewalStatus>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement_of: Option<CredentialRef>,
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
            replacement_of: source.replacement_of,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key a real producer sends today, so the closure below is provably
    /// not a break. Sources: `web/src/surfaces/credentials.tsx` (create sends
    /// `workspace_id?`, `kind`, `provider_id`, `env_key`, `secret`; archive
    /// sends `expected_version`), `e2e/management_*_e2e.mjs`,
    /// `e2e/managed_web_tools_e2e.mjs` (`idempotency_key`), and
    /// `crates/apps/awaken-process-startup/tests/management_*.rs`. No producer sends a key
    /// this type does not model, which is why the attribute can go on now.
    const PRODUCER_KEYS: &[&str] = &[
        "workspace_id",
        "kind",
        "provider_id",
        "env_key",
        "secret",
        "idempotency_key",
        "replacement_of",
        "descriptor",
        "material",
        "oauth_helper",
        "auxiliary_materials",
        "grant_lifetime",
    ];

    #[test]
    fn credential_authoring_accepts_every_key_a_client_sends_and_no_other() {
        // The credential-entry body carries two pairs of mutually exclusive
        // fields — `descriptor` against the legacy `provider_id`, and
        // `material` against the legacy `secret`. Without the attribute a
        // near-miss spelling of either (`credential_material`, the domain name
        // `material_input`, a field renamed by a later revision) decoded to
        // `None` and the request silently took the *other* branch: a structured
        // material became no material at all, and a described write became a
        // legacy one. The response is a 200 either way, so nothing observes it.
        //
        // Causes: C1 the body carries only keys the type models; C2 it carries
        // a key no field models; C3 it omits a required key.
        // Effects: E1 decode, E2 refuse.
        // | Rule | body                                   | Effect |
        // | K1   | every key a producer sends (C1)        | E1     |
        // | K2   | a near-miss beside the real field (C2) | E2     |
        // | K3   | rotate/archive with an extra key (C2)  | E2     |
        // | K4   | archive without expected_version (C3)  | E2     |
        let mut authored = serde_json::json!({
            "workspace_id": "wrkspc_1",
            "kind": "vault",
            "provider_id": "anthropic",
            "env_key": "ANTHROPIC_API_KEY",
            "secret": "sk-entry", // awaken-allow(secret):
            "idempotency_key": "cmd-1",
            "material": {"type_id": "anthropic.api_key.v1", "fields": {"api_key": "sk-x"}},
        });
        assert!(
            serde_json::from_value::<EnterCredentialRequest>(authored.clone()).is_ok(),
            "K1"
        );
        // Round-trip closure: every key a real producer sends must be a key
        // this reader models, or the attribute would 400 a request that works
        // today. `authored` above carries them all except the two the entry
        // route accepts only with a described write, which are added here.
        let mut every_producer_key = authored.clone();
        every_producer_key["replacement_of"] = serde_json::Value::Null;
        every_producer_key["oauth_helper"] = serde_json::Value::Null;
        every_producer_key["descriptor"] = serde_json::Value::Null;
        every_producer_key["auxiliary_materials"] = serde_json::json!({});
        every_producer_key["grant_lifetime"] = serde_json::Value::Null;
        let object = every_producer_key.as_object().expect("object body");
        for key in PRODUCER_KEYS {
            assert!(
                object.contains_key(*key),
                "{key} is sent by a real client but is missing from this round-trip body"
            );
        }
        assert!(
            serde_json::from_value::<EnterCredentialRequest>(every_producer_key).is_ok(),
            "K1: every key a documented producer sends still decodes after the closure"
        );

        authored["credential_material"] = serde_json::json!({"type_id": "x", "fields": {}});
        assert!(
            serde_json::from_value::<EnterCredentialRequest>(authored).is_err(),
            "K2: a near-miss beside `material` must fail, not decode as no material"
        );

        assert!(
            serde_json::from_value::<RotateCredentialRequest>(serde_json::json!({
                "expected_version": 3,
                "secret": "sk-rotated", // awaken-allow(secret):
            }))
            .is_ok(),
            "K1 rotate"
        );
        assert!(
            serde_json::from_value::<RotateCredentialRequest>(serde_json::json!({
                "expected_version": 3, "credential_material": {"type_id": "x", "fields": {}},
            }))
            .is_err(),
            "K3 rotate"
        );
        assert!(
            serde_json::from_value::<ArchiveCredentialRequest>(
                serde_json::json!({"expected_version": 3})
            )
            .is_ok(),
            "K1 archive"
        );
        assert!(
            serde_json::from_value::<ArchiveCredentialRequest>(
                serde_json::json!({"expected_version": 3, "force": true})
            )
            .is_err(),
            "K3 archive"
        );
        assert!(
            serde_json::from_value::<ArchiveCredentialRequest>(serde_json::json!({})).is_err(),
            "K4"
        );
        assert!(
            serde_json::from_value::<CredentialMaterialInput>(serde_json::json!({
                "type_id": "anthropic.api_key.v1",
                "fields": {"api_key": "sk-x"},
                "field": {"api_key": "sk-x"},
            }))
            .is_err(),
            "K2 material: the singular near-miss must fail rather than seal an empty document"
        );
    }
}
