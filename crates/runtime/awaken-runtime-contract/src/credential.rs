//! Secret-free credential execution facts embedded in a published snapshot.
//!
//! Storage, material location, target usage, plaintext authorization and model
//! exposure are intentionally separate.  This module is the one admission
//! kernel shared by model, MCP and resource adapters; adapters realize an exact
//! admitted plan and never choose a different holder after failure.

use std::collections::BTreeSet;

use async_trait::async_trait;
use awaken_agent_contract::RedactedString;
use serde::{Deserialize, Deserializer, Serialize};

/// Stable built-in trust domains used by the self-hosted execution profile.
/// Hosted deployments publish their own opaque domains instead.
pub const SELF_HOSTED_WORKER_TRUST_DOMAIN: &str = "awaken.worker";
pub const SELF_HOSTED_ACP_TRUST_DOMAIN: &str = "awaken.workload.acp";

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CredentialRef {
    pub id: String,
    pub revision: u64,
}

/// The trusted adapter from which exact credential material may be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMaterialSource {
    ControlPlaneReference,
    WorkerReference,
}

/// How the resolved endpoint consumes injected material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialUsage {
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

/// The process/trust boundary permitted to hold plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaintextBoundary {
    Workload,
    Worker,
    Platform,
}

/// Opaque trust-domain identity.  It is deliberately not an IAM principal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TrustDomainRef(pub String);

/// One exact plaintext holder; boundaries are not ordered by strength.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlaintextHolder {
    pub boundary: PlaintextBoundary,
    pub trust_domain: TrustDomainRef,
}

impl PlaintextHolder {
    #[must_use]
    pub fn new(boundary: PlaintextBoundary, trust_domain: impl Into<String>) -> Self {
        Self {
            boundary,
            trust_domain: TrustDomainRef(trust_domain.into()),
        }
    }
}

/// What credential-like content may be model-visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelExposurePolicy {
    Forbidden,
    VirtualOnly,
}

/// Published authorization for execution-time plaintext and model exposure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialExecutionPolicy {
    pub allowed_plaintext_holders: BTreeSet<PlaintextHolder>,
    pub model_exposure: ModelExposurePolicy,
}

impl CredentialExecutionPolicy {
    #[must_use]
    pub fn exact(holder: PlaintextHolder, model_exposure: ModelExposurePolicy) -> Self {
        Self {
            allowed_plaintext_holders: BTreeSet::from([holder]),
            model_exposure,
        }
    }

    #[must_use]
    pub fn new(
        allowed_plaintext_holders: impl IntoIterator<Item = PlaintextHolder>,
        model_exposure: ModelExposurePolicy,
    ) -> Self {
        Self {
            allowed_plaintext_holders: allowed_plaintext_holders.into_iter().collect(),
            model_exposure,
        }
    }

    /// Default policy compiled by the self-hosted deployment profile.
    #[must_use]
    pub fn self_hosted_provider() -> Self {
        Self::new(
            [
                PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN),
                PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN),
            ],
            ModelExposurePolicy::Forbidden,
        )
    }
}

/// Exact environment/deployment selection; it contains no fallback ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationProfile {
    pub inference_holder: PlaintextHolder,
    pub mcp_holder: PlaintextHolder,
}

/// Opaque reference to sealed credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedCredentialEnvelopeRef {
    pub id: String,
    pub payload_fingerprint: String,
}

/// Recipient-bound sealed transport; it grants no plaintext authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialEnvelope {
    SealedForWorker {
        envelope_ref: SealedCredentialEnvelopeRef,
        recipient: TrustDomainRef,
        expires_at_unix_ms: u64,
    },
    SealedForWorkload {
        envelope_ref: SealedCredentialEnvelopeRef,
        recipient: TrustDomainRef,
        expires_at_unix_ms: u64,
    },
}

impl CredentialEnvelope {
    fn recipient(&self) -> &TrustDomainRef {
        match self {
            Self::SealedForWorker { recipient, .. } | Self::SealedForWorkload { recipient, .. } => {
                recipient
            }
        }
    }

    fn expires_at_unix_ms(&self) -> u64 {
        match self {
            Self::SealedForWorker {
                expires_at_unix_ms, ..
            }
            | Self::SealedForWorkload {
                expires_at_unix_ms, ..
            } => *expires_at_unix_ms,
        }
    }

    fn required_boundary(&self) -> PlaintextBoundary {
        match self {
            Self::SealedForWorker { .. } => PlaintextBoundary::Worker,
            Self::SealedForWorkload { .. } => PlaintextBoundary::Workload,
        }
    }
}

/// Exact OAuth refresh/reseal facts pinned to the same credential revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRefreshAccess {
    pub credential_revision: u64,
    pub configuration_fingerprint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub token_endpoint_auth: TokenEndpointAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_ref: Option<String>,
    pub refresh_token_ref: String,
    pub access_token_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenEndpointAuth {
    None,
    ClientSecretBasic,
    ClientSecretPost,
}

/// Historical wire value accepted only so retained snapshots can be diagnosed
/// and migrated.  New construction has no API for selecting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyCredentialInjection {
    Reference,
    WorkerReference,
    SealedEnvelope,
    Direct,
}

/// Secret-free credential instructions pinned into an executable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialAccess {
    pub credential: CredentialRef,
    pub material_source: CredentialMaterialSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<CredentialEnvelope>,
    pub usage: CredentialUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<CredentialRefreshAccess>,
    pub policy: CredentialExecutionPolicy,
    #[serde(skip)]
    legacy_direct: bool,
}

#[derive(Deserialize)]
struct CredentialAccessWire {
    credential: CredentialRef,
    #[serde(default)]
    material_source: Option<CredentialMaterialSource>,
    #[serde(default)]
    injection: Option<LegacyCredentialInjection>,
    #[serde(default)]
    envelope: Option<CredentialEnvelope>,
    usage: CredentialUsage,
    #[serde(default)]
    refresh: Option<CredentialRefreshAccess>,
    #[serde(default)]
    policy: Option<CredentialExecutionPolicy>,
}

impl<'de> Deserialize<'de> for CredentialAccess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        let wire = CredentialAccessWire::deserialize(deserializer)?;
        let (material_source, legacy_direct) = match (wire.material_source, wire.injection) {
            (Some(source), None) => (source, false),
            (None, Some(LegacyCredentialInjection::Reference)) => {
                (CredentialMaterialSource::ControlPlaneReference, false)
            }
            (None, Some(LegacyCredentialInjection::WorkerReference)) => {
                (CredentialMaterialSource::WorkerReference, false)
            }
            (None, Some(LegacyCredentialInjection::Direct)) => {
                (CredentialMaterialSource::ControlPlaneReference, true)
            }
            (None, Some(LegacyCredentialInjection::SealedEnvelope)) => {
                return Err(D::Error::custom(
                    "legacy sealed_envelope lacks recipient-bound envelope facts",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(D::Error::custom(
                    "credential access cannot contain both material_source and injection",
                ));
            }
            (None, None) => return Err(D::Error::missing_field("material_source")),
        };
        let policy = wire.policy.ok_or_else(|| {
            D::Error::custom("legacy credential access has no plaintext-holder policy")
        })?;
        Ok(Self {
            credential: wire.credential,
            material_source,
            envelope: wire.envelope,
            usage: wire.usage,
            refresh: wire.refresh,
            policy,
            legacy_direct,
        })
    }
}

impl CredentialAccess {
    #[must_use]
    pub fn new(
        credential: CredentialRef,
        material_source: CredentialMaterialSource,
        usage: CredentialUsage,
        policy: CredentialExecutionPolicy,
    ) -> Self {
        Self {
            credential,
            material_source,
            envelope: None,
            usage,
            refresh: None,
            policy,
            legacy_direct: false,
        }
    }

    #[must_use]
    pub fn with_envelope(mut self, envelope: CredentialEnvelope) -> Self {
        self.envelope = Some(envelope);
        self
    }

    #[must_use]
    pub fn with_refresh(mut self, refresh: CredentialRefreshAccess) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Apply the one fail-closed causality chain for a selected realization.
    pub fn admit(
        &self,
        requested_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        capabilities: &CredentialRealizationCapabilities,
        now_unix_ms: u64,
    ) -> Result<CredentialRealizationPlan, CredentialAdmissionError> {
        if self.legacy_direct {
            return Err(CredentialAdmissionError::DirectPublicationRejected);
        }
        if self.policy.allowed_plaintext_holders.is_empty() {
            return Err(CredentialAdmissionError::EmptyAllowedHolders);
        }
        if !self
            .policy
            .allowed_plaintext_holders
            .contains(requested_holder)
        {
            return Err(CredentialAdmissionError::HolderNotAllowed);
        }
        if !capabilities.holders.contains(requested_holder)
            || !capabilities.realization_kinds.contains(&realization)
        {
            return Err(CredentialAdmissionError::HolderUnsupported);
        }
        if !capabilities
            .material_sources
            .contains(&self.material_source)
        {
            return Err(CredentialAdmissionError::MaterialSourceUnsupported);
        }
        if let Some(envelope) = &self.envelope {
            if envelope.recipient() != &requested_holder.trust_domain
                || envelope.required_boundary() != requested_holder.boundary
            {
                return Err(CredentialAdmissionError::EnvelopeRecipientMismatch);
            }
            if envelope.expires_at_unix_ms() < now_unix_ms {
                return Err(CredentialAdmissionError::EnvelopeExpired);
            }
        }
        if self
            .refresh
            .as_ref()
            .is_some_and(|refresh| refresh.credential_revision != self.credential.revision)
        {
            return Err(CredentialAdmissionError::CredentialRevisionMismatch);
        }
        Ok(CredentialRealizationPlan {
            credential: self.credential.clone(),
            selected_plaintext_holder: requested_holder.clone(),
            selected_realization_kind: realization,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRealizationKind {
    ProcessSecretEnvironment,
    PrivateSecretFile,
    WorkerProviderAdapter,
    WorkerRelay,
}

/// Installed last-mile capabilities.  This is evidence, not preference policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialRealizationCapabilities {
    pub holders: BTreeSet<PlaintextHolder>,
    pub material_sources: BTreeSet<CredentialMaterialSource>,
    pub realization_kinds: BTreeSet<CredentialRealizationKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationPlan {
    pub credential: CredentialRef,
    pub selected_plaintext_holder: PlaintextHolder,
    pub selected_realization_kind: CredentialRealizationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialAdmissionError {
    #[error("credential policy has no allowed plaintext holder")]
    EmptyAllowedHolders,
    #[error("selected plaintext holder is not authorized")]
    HolderNotAllowed,
    #[error("selected plaintext holder or realization is unsupported")]
    HolderUnsupported,
    #[error("credential material source is unsupported")]
    MaterialSourceUnsupported,
    #[error("sealed envelope recipient does not match the selected holder")]
    EnvelopeRecipientMismatch,
    #[error("sealed credential envelope is expired")]
    EnvelopeExpired,
    #[error("direct credential publication is rejected")]
    DirectPublicationRejected,
    #[error("credential refresh revision does not match credential revision")]
    CredentialRevisionMismatch,
}

/// Material returned only to the exact selected holder by a resolver adapter.
#[derive(Debug)]
pub struct ResolvedCredentialMaterial {
    pub credential: CredentialRef,
    pub holder: PlaintextHolder,
    pub material: RedactedString,
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
}

/// Sole neutral source port for an already-selected exact credential access.
#[async_trait]
pub trait CredentialMaterialResolver: Send + Sync {
    async fn resolve_exact(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct AdmissionRule {
        id: &'static str,
        legacy_direct: bool,
        policy_nonempty: bool,
        holder_allowed: bool,
        holder_supported: bool,
        source_supported: bool,
        envelope_recipient_matches: bool,
        envelope_live: bool,
        refresh_revision_matches: bool,
        expected: Result<(), CredentialAdmissionError>,
    }

    fn holder(boundary: PlaintextBoundary, domain: &str) -> PlaintextHolder {
        PlaintextHolder::new(boundary, domain)
    }

    fn access(allowed: BTreeSet<PlaintextHolder>) -> CredentialAccess {
        CredentialAccess::new(
            CredentialRef {
                id: "credential-1".into(),
                revision: 7,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy {
                allowed_plaintext_holders: allowed,
                model_exposure: ModelExposurePolicy::Forbidden,
            },
        )
    }

    fn capabilities(selected: &PlaintextHolder) -> CredentialRealizationCapabilities {
        CredentialRealizationCapabilities {
            holders: BTreeSet::from([selected.clone()]),
            material_sources: BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]),
            realization_kinds: BTreeSet::from([CredentialRealizationKind::WorkerProviderAdapter]),
        }
    }

    /// Cause-effect graph:
    ///
    /// C0 new secret-free publication
    ///  -> C1 policy nonempty -> C2 exact holder allowed
    ///  -> C3 installed holder/kind -> C4 material source supported
    ///  -> C5 envelope recipient exact -> C6 envelope live
    ///  -> C7 refresh revision exact -> E1 exact plan admitted.
    ///
    /// Each failed cause yields its stable error E2 and terminates the chain.
    /// The decision table uses `T` for the satisfied cause and `F` for the sole
    /// failing cause; later causes are don't-care because evaluation has stopped.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | C7 | Result |
    /// |---|---|---|---|---|---|---|---|---|---|
    /// | R1 | T | T | T | T | T | T | T | T | admitted |
    /// | R2 | F | - | - | - | - | - | - | - | direct rejected |
    /// | R3 | T | F | - | - | - | - | - | - | empty policy |
    /// | R4 | T | T | F | - | - | - | - | - | holder forbidden |
    /// | R5 | T | T | T | F | - | - | - | - | holder unsupported |
    /// | R6 | T | T | T | T | F | - | - | - | source unsupported |
    /// | R7 | T | T | T | T | T | F | - | - | recipient mismatch |
    /// | R8 | T | T | T | T | T | T | F | - | envelope expired |
    /// | R9 | T | T | T | T | T | T | T | F | revision mismatch |
    #[test]
    fn admission_tests_are_generated_from_the_decision_table() {
        let rules = [
            AdmissionRule {
                id: "R1",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Ok(()),
            },
            AdmissionRule {
                id: "R2",
                legacy_direct: true,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::DirectPublicationRejected),
            },
            AdmissionRule {
                id: "R3",
                legacy_direct: false,
                policy_nonempty: false,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::EmptyAllowedHolders),
            },
            AdmissionRule {
                id: "R4",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: false,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::HolderNotAllowed),
            },
            AdmissionRule {
                id: "R5",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: false,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::HolderUnsupported),
            },
            AdmissionRule {
                id: "R6",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: false,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::MaterialSourceUnsupported),
            },
            AdmissionRule {
                id: "R7",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: false,
                envelope_live: true,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::EnvelopeRecipientMismatch),
            },
            AdmissionRule {
                id: "R8",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: false,
                refresh_revision_matches: true,
                expected: Err(CredentialAdmissionError::EnvelopeExpired),
            },
            AdmissionRule {
                id: "R9",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: false,
                expected: Err(CredentialAdmissionError::CredentialRevisionMismatch),
            },
        ];

        for rule in rules {
            let selected = holder(PlaintextBoundary::Worker, "worker-a");
            let allowed = if !rule.policy_nonempty {
                BTreeSet::new()
            } else if rule.holder_allowed {
                BTreeSet::from([selected.clone()])
            } else {
                BTreeSet::from([holder(PlaintextBoundary::Worker, "worker-b")])
            };
            let mut access = if rule.legacy_direct {
                legacy_direct_access(&selected)
            } else {
                access(allowed)
            };
            if !rule.envelope_recipient_matches || !rule.envelope_live {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "envelope-1".into(),
                        payload_fingerprint: "sha256:payload".into(),
                    },
                    recipient: TrustDomainRef(
                        if rule.envelope_recipient_matches {
                            "worker-a"
                        } else {
                            "worker-b"
                        }
                        .into(),
                    ),
                    expires_at_unix_ms: if rule.envelope_live { 20 } else { 9 },
                });
            }
            if !rule.refresh_revision_matches {
                access = access.with_refresh(CredentialRefreshAccess {
                    credential_revision: 8,
                    configuration_fingerprint: "sha256:refresh".into(),
                    token_endpoint: "https://auth.example/token".into(),
                    client_id: "client".into(),
                    token_endpoint_auth: TokenEndpointAuth::None,
                    client_secret_ref: None,
                    refresh_token_ref: "refresh-ref".into(),
                    access_token_ref: "access-ref".into(),
                    scope: None,
                    resource: None,
                });
            }
            let mut capabilities = capabilities(&selected);
            if !rule.holder_supported {
                capabilities.holders.clear();
            }
            if !rule.source_supported {
                capabilities.material_sources.clear();
            }
            let actual = access
                .admit(
                    &selected,
                    CredentialRealizationKind::WorkerProviderAdapter,
                    &capabilities,
                    10,
                )
                .map(|plan| {
                    assert_eq!(plan.credential.revision, 7, "{}", rule.id);
                    assert_eq!(plan.selected_plaintext_holder, selected, "{}", rule.id);
                });
            assert_eq!(actual, rule.expected, "decision rule {}", rule.id);
        }
    }

    fn legacy_direct_access(selected: &PlaintextHolder) -> CredentialAccess {
        let decoded: CredentialAccess = serde_json::from_value(serde_json::json!({
            "credential": { "id": "credential-1", "revision": 7 },
            "injection": "direct",
            "usage": { "type": "provider_adapter" },
            "policy": {
                "allowed_plaintext_holders": [selected],
                "model_exposure": "forbidden"
            }
        }))
        .unwrap();
        assert!(
            serde_json::to_value(&decoded)
                .unwrap()
                .get("injection")
                .is_none()
        );
        decoded
    }
}
