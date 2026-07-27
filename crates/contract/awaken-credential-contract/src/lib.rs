//! Shared secret-free credential execution facts (ADR-0067).
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

/// Worker-local state observed for one exact non-secret credential revision.
/// This is execution evidence, never credential selection or secret material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialObservationState {
    Available,
    LoginRequired,
    Expired,
    Invalid,
    Disabled,
    ProbeFailed,
}

/// A point-in-time Worker observation used by placement and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CredentialObservation {
    pub credential: CredentialRef,
    pub state: CredentialObservationState,
    pub observed_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
}

impl CredentialObservation {
    #[must_use]
    pub fn available(credential: CredentialRef, observed_at_ms: u64) -> Self {
        Self {
            credential,
            state: CredentialObservationState::Available,
            observed_at_ms,
            reason_code: None,
        }
    }
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
    fn self_hosted_holders() -> [PlaintextHolder; 2] {
        [
            PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN),
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN),
        ]
    }

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
        Self::new(Self::self_hosted_holders(), ModelExposurePolicy::Forbidden)
    }

    /// Canonical authenticated MCP policy. A mediated ACP projection may expose
    /// only the generation-scoped synthetic relay capability; backing material
    /// remains Worker-held and is never model-visible.
    #[must_use]
    pub fn self_hosted_mcp() -> Self {
        Self::new(
            Self::self_hosted_holders(),
            ModelExposurePolicy::VirtualOnly,
        )
    }
}

/// Exact environment/deployment selection; it contains no fallback ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationProfile {
    pub inference_holder: PlaintextHolder,
    pub mcp_holder: PlaintextHolder,
    /// Exact holder for Session Resource credentials such as host-mediated Git
    /// transport. Older persisted Environment snapshots predate this purpose and
    /// therefore decode to the canonical self-hosted Worker boundary.
    #[serde(default = "self_hosted_worker_holder")]
    pub resource_holder: PlaintextHolder,
}

fn self_hosted_worker_holder() -> PlaintextHolder {
    PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN)
}

impl CredentialRealizationProfile {
    /// Canonical self-hosted profile for the in-process Native runtime.
    #[must_use]
    pub fn self_hosted_native() -> Self {
        let worker = self_hosted_worker_holder();
        Self {
            inference_holder: worker.clone(),
            mcp_holder: worker.clone(),
            resource_holder: worker,
        }
    }

    /// Canonical self-hosted profile for an ACP workload. MCP remains mediated by
    /// the Worker until a distinct trusted workload MCP client is installed.
    #[must_use]
    pub fn self_hosted_acp() -> Self {
        Self {
            inference_holder: PlaintextHolder::new(
                PlaintextBoundary::Workload,
                SELF_HOSTED_ACP_TRUST_DOMAIN,
            ),
            mcp_holder: self_hosted_worker_holder(),
            resource_holder: self_hosted_worker_holder(),
        }
    }
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
    fn envelope_ref(&self) -> &SealedCredentialEnvelopeRef {
        match self {
            Self::SealedForWorker { envelope_ref, .. }
            | Self::SealedForWorkload { envelope_ref, .. } => envelope_ref,
        }
    }

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

    /// Revalidate the transport binding immediately before material resolution.
    /// Admission may have happened earlier; expiry and recipient are therefore
    /// checked again at the material boundary.
    pub fn validate_for_holder(
        &self,
        selected_holder: &PlaintextHolder,
        now_unix_ms: u64,
    ) -> Result<(), CredentialMaterialError> {
        if self.recipient() != &selected_holder.trust_domain
            || self.required_boundary() != selected_holder.boundary
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if self.expires_at_unix_ms() <= now_unix_ms {
            return Err(CredentialMaterialError::EnvelopeExpired);
        }
        Ok(())
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
    /// Provider account/workspace required by externally managed OAuth drivers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Optional provider plan/tenant classification passed only to the driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_plan: Option<String>,
    /// Known access-token expiry. `None` delegates expiry detection to the driver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl CredentialRefreshAccess {
    /// Compile one exact refresh/reseal instruction and bind every executable
    /// configuration field into its owning-domain fingerprint.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        credential_revision: u64,
        token_endpoint: String,
        client_id: String,
        token_endpoint_auth: TokenEndpointAuth,
        client_secret_ref: Option<String>,
        refresh_token_ref: String,
        access_token_ref: String,
        scope: Option<String>,
        resource: Option<String>,
    ) -> Self {
        let mut access = Self {
            credential_revision,
            configuration_fingerprint: String::new(),
            token_endpoint,
            client_id,
            token_endpoint_auth,
            client_secret_ref,
            refresh_token_ref,
            access_token_ref,
            account_id: None,
            account_plan: None,
            expires_at_unix_ms: None,
            scope,
            resource,
        };
        access.configuration_fingerprint = access.expected_configuration_fingerprint();
        access
    }

    /// Recompute the digest from executable facts only. Revision is checked
    /// separately so drift and configuration tampering remain distinguishable.
    #[must_use]
    pub fn expected_configuration_fingerprint(&self) -> String {
        awaken_agent_contract::stable_fingerprint(&(
            &self.token_endpoint,
            &self.client_id,
            self.token_endpoint_auth,
            &self.client_secret_ref,
            &self.refresh_token_ref,
            &self.access_token_ref,
            &self.account_id,
            &self.account_plan,
            self.expires_at_unix_ms,
            &self.scope,
            &self.resource,
        ))
    }

    #[must_use]
    pub fn with_provider_metadata(
        mut self,
        account_id: Option<String>,
        account_plan: Option<String>,
        expires_at_unix_ms: Option<u64>,
    ) -> Self {
        self.account_id = account_id;
        self.account_plan = account_plan;
        self.expires_at_unix_ms = expires_at_unix_ms;
        self.configuration_fingerprint = self.expected_configuration_fingerprint();
        self
    }

    #[must_use]
    pub fn has_valid_configuration_fingerprint(&self) -> bool {
        self.configuration_fingerprint == self.expected_configuration_fingerprint()
    }

    #[must_use]
    pub fn has_valid_client_authentication_binding(&self) -> bool {
        matches!(
            (self.token_endpoint_auth, self.client_secret_ref.as_ref()),
            (TokenEndpointAuth::None, None)
                | (
                    TokenEndpointAuth::ClientSecretBasic | TokenEndpointAuth::ClientSecretPost,
                    Some(_)
                )
        )
    }
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
    #[serde(default, skip_serializing_if = "is_false")]
    legacy_direct: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    #[serde(default)]
    legacy_direct: bool,
}

impl<'de> Deserialize<'de> for CredentialAccess {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;

        let wire = CredentialAccessWire::deserialize(deserializer)?;
        let (material_source, decoded_direct) = match (wire.material_source, wire.injection) {
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
            legacy_direct: decoded_direct || wire.legacy_direct,
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
        if !capabilities.supports(
            requested_holder,
            self.material_source,
            realization,
            self.envelope.is_some(),
        ) {
            if !capabilities.holders.contains(requested_holder)
                && !capabilities
                    .alternatives
                    .iter()
                    .any(|profile| profile.holders.contains(requested_holder))
            {
                return Err(CredentialAdmissionError::HolderUnsupported);
            }
            if !capabilities
                .material_sources
                .contains(&self.material_source)
                && !capabilities
                    .alternatives
                    .iter()
                    .any(|profile| profile.material_sources.contains(&self.material_source))
            {
                return Err(CredentialAdmissionError::MaterialSourceUnsupported);
            }
            if self.envelope.is_some() {
                return Err(CredentialAdmissionError::EnvelopeUnsupported);
            }
            return Err(CredentialAdmissionError::HolderUnsupported);
        }
        if let Some(envelope) = &self.envelope {
            if envelope.envelope_ref().id.trim().is_empty() {
                return Err(CredentialAdmissionError::EnvelopeReferenceEmpty);
            }
            if envelope
                .envelope_ref()
                .payload_fingerprint
                .trim()
                .is_empty()
            {
                return Err(CredentialAdmissionError::EnvelopeFingerprintEmpty);
            }
            if envelope.recipient() != &requested_holder.trust_domain
                || envelope.required_boundary() != requested_holder.boundary
            {
                return Err(CredentialAdmissionError::EnvelopeRecipientMismatch);
            }
            if envelope.expires_at_unix_ms() <= now_unix_ms {
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
        if self
            .refresh
            .as_ref()
            .is_some_and(|refresh| !refresh.has_valid_client_authentication_binding())
        {
            return Err(CredentialAdmissionError::RefreshClientAuthenticationInvalid);
        }
        if self
            .refresh
            .as_ref()
            .is_some_and(|refresh| !refresh.has_valid_configuration_fingerprint())
        {
            return Err(CredentialAdmissionError::RefreshConfigurationMismatch);
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
    /// A trusted downstream platform adapter consumes the exact published
    /// credential reference without exposing provider plaintext to the Worker
    /// or workload.
    PlatformProviderAdapter,
}

/// Installed last-mile capabilities.  This is evidence, not preference policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRealizationCapabilities {
    pub holders: BTreeSet<PlaintextHolder>,
    pub material_sources: BTreeSet<CredentialMaterialSource>,
    pub realization_kinds: BTreeSet<CredentialRealizationKind>,
    #[serde(default)]
    pub recipient_bound_envelopes: bool,
    /// Independent adapter profiles installed in one process/Worker.
    ///
    /// Keeping profiles separate is security-significant: flattening a Native
    /// Worker-holder profile and an ACP Workload-holder profile into three unions
    /// would synthesize holder/source/realization combinations that no adapter
    /// actually implements. Legacy single-profile declarations leave this empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<CredentialRealizationCapabilities>,
}

/// WorkerManifest capability namespace for one canonical credential realization
/// evidence payload. The Worker contract remains credential-domain agnostic; the
/// publishing and consuming contexts own this codec.
pub const CREDENTIAL_REALIZATION_CAPABILITY_PREFIX: &str = "credential-realization.awaken.dev/v1:";

impl CredentialRealizationCapabilities {
    /// Whether this adapter/provider advertises no credential realization at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.holders.is_empty()
            && self.material_sources.is_empty()
            && self.realization_kinds.is_empty()
            && !self.recipient_bound_envelopes
            && self.alternatives.iter().all(Self::is_empty)
    }

    /// Compose independent adapter evidence without creating a Cartesian-product
    /// capability. Nested compositions are flattened and empty profiles discarded.
    #[must_use]
    pub fn alternatives(
        profiles: impl IntoIterator<Item = CredentialRealizationCapabilities>,
    ) -> Self {
        let mut alternatives = Vec::new();
        for mut profile in profiles {
            alternatives.append(&mut profile.alternatives);
            if !profile.is_empty() {
                alternatives.push(profile);
            }
        }
        Self {
            alternatives,
            ..Self::default()
        }
    }

    fn supports(
        &self,
        holder: &PlaintextHolder,
        source: CredentialMaterialSource,
        realization: CredentialRealizationKind,
        envelope: bool,
    ) -> bool {
        (self.holders.contains(holder)
            && self.material_sources.contains(&source)
            && self.realization_kinds.contains(&realization)
            && (!envelope || self.recipient_bound_envelopes))
            || self
                .alternatives
                .iter()
                .any(|profile| profile.supports(holder, source, realization, envelope))
    }

    /// Encode this evidence as one canonical Worker capability. Empty evidence
    /// emits no capability.
    pub fn manifest_capability(&self) -> Result<Option<String>, serde_json::Error> {
        if self.is_empty() {
            return Ok(None);
        }
        serde_json::to_string(self).map(|payload| {
            Some(format!(
                "{CREDENTIAL_REALIZATION_CAPABILITY_PREFIX}{payload}"
            ))
        })
    }

    /// Decode the unique credential realization capability from an otherwise
    /// domain-neutral Worker capability set. Multiple declarations fail closed.
    pub fn from_manifest_capabilities(capabilities: &BTreeSet<String>) -> Result<Self, String> {
        let mut encoded = capabilities.iter().filter_map(|capability| {
            capability.strip_prefix(CREDENTIAL_REALIZATION_CAPABILITY_PREFIX)
        });
        let Some(payload) = encoded.next() else {
            return Ok(Self::default());
        };
        if encoded.next().is_some() {
            return Err(
                "Worker manifest declares multiple credential realization capabilities".to_string(),
            );
        }
        serde_json::from_str(payload)
            .map_err(|error| format!("invalid credential realization capability: {error}"))
    }
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
    #[error("recipient-bound credential envelopes are unsupported")]
    EnvelopeUnsupported,
    #[error("sealed credential envelope reference is empty")]
    EnvelopeReferenceEmpty,
    #[error("sealed credential envelope payload fingerprint is empty")]
    EnvelopeFingerprintEmpty,
    #[error("sealed envelope recipient does not match the selected holder")]
    EnvelopeRecipientMismatch,
    #[error("sealed credential envelope is expired")]
    EnvelopeExpired,
    #[error("direct credential publication is rejected")]
    DirectPublicationRejected,
    #[error("credential refresh revision does not match credential revision")]
    CredentialRevisionMismatch,
    #[error("credential refresh client authentication binding is invalid")]
    RefreshClientAuthenticationInvalid,
    #[error("credential refresh configuration fingerprint does not match its executable facts")]
    RefreshConfigurationMismatch,
}

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

/// Material shape returned by the sole exact resolver port. Provider drivers
/// consume OAuth as a bundle; legacy bearer consumers can only accept `Bearer`.
#[derive(Debug)]
pub enum CredentialMaterial {
    Bearer(RedactedString),
    OAuth(OAuthCredentialMaterial),
}

impl CredentialMaterial {
    #[must_use]
    pub fn bearer(value: RedactedString) -> Self {
        Self::Bearer(value)
    }

    #[must_use]
    pub fn access_token(&self) -> &RedactedString {
        match self {
            Self::Bearer(value) => value,
            Self::OAuth(bundle) => &bundle.access_token,
        }
    }

    pub fn into_bearer(self) -> Result<RedactedString, CredentialMaterialError> {
        match self {
            Self::Bearer(value) => Ok(value),
            Self::OAuth(_) => Err(CredentialMaterialError::MaterialKindMismatch),
        }
    }
}

/// Material returned only to the exact selected holder by a resolver adapter.
#[derive(Debug)]
pub struct ResolvedCredentialMaterial {
    pub credential: CredentialRef,
    pub holder: PlaintextHolder,
    pub material: CredentialMaterial,
}

/// Exact non-secret execution binding supplied to the material-source adapter.
/// The fingerprint is computed from the authoritative consumer target together
/// with its [`CredentialUsage`]; adapters compare it with recipient-bound sealed
/// claims and never infer or enumerate a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialMaterialBinding {
    pub workspace_id: String,
    pub target_use_fingerprint: String,
}

impl CredentialMaterialBinding {
    #[must_use]
    pub fn for_target<T: Serialize>(
        workspace_id: impl Into<String>,
        target: &T,
        usage: &CredentialUsage,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            target_use_fingerprint: awaken_agent_contract::stable_fingerprint(&(target, usage)),
        }
    }

    pub fn validate(&self) -> Result<(), CredentialMaterialError> {
        if self.workspace_id.trim().is_empty() || self.target_use_fingerprint.trim().is_empty() {
            return Err(CredentialMaterialError::BindingMismatch);
        }
        Ok(())
    }
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

/// Sole neutral source port for an already-selected exact credential access.
#[async_trait]
pub trait CredentialMaterialResolver: Send + Sync {
    /// Exact material sources this adapter can resolve. Empty is fail-closed.
    fn supported_material_sources(&self) -> BTreeSet<CredentialMaterialSource> {
        BTreeSet::new()
    }

    /// Whether this adapter validates and opens recipient-bound envelope claims.
    fn supports_recipient_bound_envelopes(&self) -> bool {
        false
    }

    /// Current non-secret observations for exact Worker-local revisions owned by
    /// this resolver. Control-plane resolvers return an empty set.
    async fn credential_observations(
        &self,
    ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
        Ok(BTreeSet::new())
    }

    /// Re-probe one already-selected exact Worker-local reference immediately
    /// before launch. This is a liveness check, not credential selection. An
    /// adapter may override it with a cheaper exact provider operation.
    async fn revalidate_worker_reference(
        &self,
        credential: &CredentialRef,
    ) -> Result<CredentialObservation, CredentialMaterialError> {
        let observation = self
            .credential_observations()
            .await?
            .into_iter()
            .find(|observation| &observation.credential == credential)
            .ok_or(CredentialMaterialError::Unavailable)?;
        match observation.state {
            CredentialObservationState::Available => Ok(observation),
            CredentialObservationState::LoginRequired => {
                Err(CredentialMaterialError::LoginRequired)
            }
            CredentialObservationState::Expired => Err(CredentialMaterialError::Expired),
            CredentialObservationState::Invalid => Err(CredentialMaterialError::Invalid),
            CredentialObservationState::Disabled => Err(CredentialMaterialError::Disabled),
            CredentialObservationState::ProbeFailed => Err(CredentialMaterialError::ProbeFailed),
        }
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cause-effect graph for the credential evidence codec:
    ///
    /// C1 credential declaration exists
    ///  ├─ F -> E1 empty installed capabilities
    ///  └─ T -> C2 declaration is unique
    ///           ├─ F -> E2 reject ambiguous evidence
    ///           └─ T -> C3 payload is valid
    ///                    ├─ F -> E3 reject malformed evidence
    ///                    └─ T -> E4 recover exact installed capabilities.
    ///
    /// Decision table:
    ///
    /// | Rule | C1 | C2 | C3 | Result |
    /// |---|---|---|---|---|
    /// | M1 | F | - | - | empty |
    /// | M2 | T | T | T | exact round trip |
    /// | M3 | T | T | F | reject malformed |
    /// | M4 | T | F | - | reject ambiguous |
    #[test]
    fn worker_manifest_capability_codec_follows_decision_table() {
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker-a");
        let exact = CredentialRealizationCapabilities {
            holders: BTreeSet::from([holder]),
            material_sources: BTreeSet::from([CredentialMaterialSource::WorkerReference]),
            realization_kinds: BTreeSet::from([CredentialRealizationKind::WorkerProviderAdapter]),
            recipient_bound_envelopes: true,
            alternatives: Vec::new(),
        };
        let encoded = exact
            .manifest_capability()
            .expect("encode exact capabilities")
            .expect("nonempty capabilities emit evidence");

        let rules = [
            (
                "M1",
                BTreeSet::from(["unrelated.capability".to_string()]),
                Ok(CredentialRealizationCapabilities::default()),
            ),
            ("M2", BTreeSet::from([encoded.clone()]), Ok(exact)),
            (
                "M3",
                BTreeSet::from([format!(
                    "{CREDENTIAL_REALIZATION_CAPABILITY_PREFIX}not-json"
                )]),
                Err("invalid"),
            ),
            (
                "M4",
                BTreeSet::from([
                    encoded,
                    format!("{CREDENTIAL_REALIZATION_CAPABILITY_PREFIX}{{}}"),
                ]),
                Err("multiple"),
            ),
        ];

        for (id, capabilities, expected) in rules {
            let actual =
                CredentialRealizationCapabilities::from_manifest_capabilities(&capabilities);
            match expected {
                Ok(expected) => assert_eq!(actual, Ok(expected), "decision rule {id}"),
                Err(fragment) => assert!(
                    actual
                        .expect_err("decision rule must reject")
                        .contains(fragment),
                    "decision rule {id}"
                ),
            }
        }
        assert_eq!(
            CredentialRealizationCapabilities::default()
                .manifest_capability()
                .expect("encode empty capabilities"),
            None
        );
    }

    /// Multi-adapter cause graph: C1 holder, C2 source, and C3 realization must
    /// coexist in one installed profile. Evidence from separate profiles never
    /// combines into synthetic authority.
    ///
    /// | Rule | holder profile | source profile | kind profile | Result |
    /// |---|---|---|---|---|
    /// | A1 | ACP | ACP | ACP | admit |
    /// | A2 | Native | Native | Native | admit |
    /// | A3 | Native | ACP | ACP | reject cross-profile product |
    #[test]
    fn independent_adapter_profiles_do_not_create_cartesian_authority() {
        let worker = PlaintextHolder::new(PlaintextBoundary::Worker, "worker");
        let workload = PlaintextHolder::new(PlaintextBoundary::Workload, "workload");
        let native = CredentialRealizationCapabilities {
            holders: BTreeSet::from([worker.clone()]),
            material_sources: BTreeSet::from([CredentialMaterialSource::ControlPlaneReference]),
            realization_kinds: BTreeSet::from([CredentialRealizationKind::WorkerProviderAdapter]),
            ..CredentialRealizationCapabilities::default()
        };
        let acp = CredentialRealizationCapabilities {
            holders: BTreeSet::from([workload.clone()]),
            material_sources: BTreeSet::from([CredentialMaterialSource::WorkerReference]),
            realization_kinds: BTreeSet::from([
                CredentialRealizationKind::ProcessSecretEnvironment,
            ]),
            ..CredentialRealizationCapabilities::default()
        };
        let installed = CredentialRealizationCapabilities::alternatives([native, acp]);

        assert!(installed.supports(
            &workload,
            CredentialMaterialSource::WorkerReference,
            CredentialRealizationKind::ProcessSecretEnvironment,
            false,
        ));
        assert!(installed.supports(
            &worker,
            CredentialMaterialSource::ControlPlaneReference,
            CredentialRealizationKind::WorkerProviderAdapter,
            false,
        ));
        assert!(!installed.supports(
            &worker,
            CredentialMaterialSource::WorkerReference,
            CredentialRealizationKind::ProcessSecretEnvironment,
            false,
        ));
    }

    /// Exposure decision table: provider material never needs a model-visible
    /// credential representation; authenticated MCP may expose only a synthetic
    /// generation capability. Both reuse the identical allowed-holder set.
    ///
    /// | Rule | purpose | exposure |
    /// |---|---|---|
    /// | E1 | inference/provider | Forbidden |
    /// | E2 | MCP relay | VirtualOnly |
    #[test]
    fn self_hosted_exposure_profiles_share_holders_but_not_exposure() {
        let provider = CredentialExecutionPolicy::self_hosted_provider();
        let mcp = CredentialExecutionPolicy::self_hosted_mcp();
        assert_eq!(
            provider.allowed_plaintext_holders,
            mcp.allowed_plaintext_holders
        );
        assert_eq!(
            provider.model_exposure,
            ModelExposurePolicy::Forbidden,
            "E1"
        );
        assert_eq!(mcp.model_exposure, ModelExposurePolicy::VirtualOnly, "E2");
    }

    #[test]
    fn retained_environment_profiles_default_only_the_new_resource_purpose() {
        let profile: CredentialRealizationProfile = serde_json::from_value(serde_json::json!({
            "inference_holder": {
                "boundary": "workload",
                "trust_domain": "retained.workload"
            },
            "mcp_holder": {
                "boundary": "worker",
                "trust_domain": "retained.worker"
            }
        }))
        .expect("decode retained two-purpose profile");
        assert_eq!(
            profile.inference_holder,
            PlaintextHolder::new(PlaintextBoundary::Workload, "retained.workload")
        );
        assert_eq!(
            profile.mcp_holder,
            PlaintextHolder::new(PlaintextBoundary::Worker, "retained.worker")
        );
        assert_eq!(profile.resource_holder, self_hosted_worker_holder());
    }

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
        refresh_configuration_matches: bool,
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
            recipient_bound_envelopes: true,
            alternatives: Vec::new(),
        }
    }

    /// Cause-effect graph:
    ///
    /// C0 new secret-free publication
    ///  -> C1 policy nonempty -> C2 exact holder allowed
    ///  -> C3 installed holder/kind -> C4 material source supported
    ///  -> C5 envelope recipient exact -> C6 envelope live
    ///  -> C7 refresh revision exact -> C8 refresh client auth well formed
    ///  -> C9 refresh fingerprint exact
    ///  -> E1 exact plan admitted.
    ///
    /// Each failed cause yields its stable error E2 and terminates the chain.
    /// The decision table uses `T` for the satisfied cause and `F` for the sole
    /// failing cause; later causes are don't-care because evaluation has stopped.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | C9 | Result |
    /// |---|---|---|---|---|---|---|---|---|---|---|---|
    /// | R1 | T | T | T | T | T | T | T | T | T | T | admitted |
    /// | R2 | F | - | - | - | - | - | - | - | - | - | direct rejected |
    /// | R3 | T | F | - | - | - | - | - | - | - | - | empty policy |
    /// | R4 | T | T | F | - | - | - | - | - | - | - | holder forbidden |
    /// | R5 | T | T | T | F | - | - | - | - | - | - | holder unsupported |
    /// | R6 | T | T | T | T | F | - | - | - | - | - | source unsupported |
    /// | R7 | T | T | T | T | T | F | - | - | - | - | recipient mismatch |
    /// | R8 | T | T | T | T | T | T | F | - | - | - | envelope expired |
    /// | R9 | T | T | T | T | T | T | T | F | - | - | revision mismatch |
    /// | R10 | T | T | T | T | T | T | T | T | T | F | config mismatch |
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
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
                refresh_configuration_matches: true,
                expected: Err(CredentialAdmissionError::CredentialRevisionMismatch),
            },
            AdmissionRule {
                id: "R10",
                legacy_direct: false,
                policy_nonempty: true,
                holder_allowed: true,
                holder_supported: true,
                source_supported: true,
                envelope_recipient_matches: true,
                envelope_live: true,
                refresh_revision_matches: true,
                refresh_configuration_matches: false,
                expected: Err(CredentialAdmissionError::RefreshConfigurationMismatch),
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
            if !rule.refresh_revision_matches || !rule.refresh_configuration_matches {
                let mut refresh = CredentialRefreshAccess::new(
                    if rule.refresh_revision_matches { 7 } else { 8 },
                    "https://auth.example/token".into(),
                    "client".into(),
                    TokenEndpointAuth::None,
                    None,
                    "refresh-ref".into(),
                    "access-ref".into(),
                    None,
                    None,
                );
                if !rule.refresh_configuration_matches {
                    refresh.token_endpoint.push_str("/tampered");
                }
                access = access.with_refresh(refresh);
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

    /// Cause-effect graph for the recipient-bound envelope gate:
    ///
    /// C0 installed adapter supports recipient-bound envelopes
    ///  -> C1 envelope reference is nonempty
    ///  -> C2 payload fingerprint is nonempty
    ///  -> C3 recipient trust domain and boundary match the selected holder
    ///  -> C4 expiry is strictly after admission time
    ///  -> E1 admit the exact realization plan.
    ///
    /// The first false cause terminates evaluation with its stable error. This
    /// prevents an empty opaque handle or an unverified payload identity from
    /// being treated as sealed transport evidence.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | Result |
    /// |---|---|---|---|---|---|---|
    /// | S1 | T | T | T | T | T | admitted |
    /// | S2 | T | F | - | - | - | empty reference |
    /// | S3 | T | T | F | - | - | empty fingerprint |
    /// | S4 | T | T | T | F | - | recipient mismatch |
    /// | S5 | T | T | T | T | F | expired at boundary |
    /// | S6 | F | - | - | - | - | unsupported adapter |
    #[test]
    fn sealed_envelope_admission_follows_the_decision_table() {
        struct Rule {
            id: &'static str,
            reference: &'static str,
            fingerprint: &'static str,
            recipient: &'static str,
            expiry: u64,
            adapter_support: bool,
            expected: Result<(), CredentialAdmissionError>,
        }

        let rules = [
            Rule {
                id: "S1",
                reference: "envelope-1",
                fingerprint: "sha256:payload",
                recipient: "worker-a",
                expiry: 11,
                adapter_support: true,
                expected: Ok(()),
            },
            Rule {
                id: "S2",
                reference: " ",
                fingerprint: "sha256:payload",
                recipient: "worker-a",
                expiry: 11,
                adapter_support: true,
                expected: Err(CredentialAdmissionError::EnvelopeReferenceEmpty),
            },
            Rule {
                id: "S3",
                reference: "envelope-1",
                fingerprint: " ",
                recipient: "worker-a",
                expiry: 11,
                adapter_support: true,
                expected: Err(CredentialAdmissionError::EnvelopeFingerprintEmpty),
            },
            Rule {
                id: "S4",
                reference: "envelope-1",
                fingerprint: "sha256:payload",
                recipient: "worker-b",
                expiry: 11,
                adapter_support: true,
                expected: Err(CredentialAdmissionError::EnvelopeRecipientMismatch),
            },
            Rule {
                id: "S5",
                reference: "envelope-1",
                fingerprint: "sha256:payload",
                recipient: "worker-a",
                expiry: 10,
                adapter_support: true,
                expected: Err(CredentialAdmissionError::EnvelopeExpired),
            },
            Rule {
                id: "S6",
                reference: "envelope-1",
                fingerprint: "sha256:payload",
                recipient: "worker-a",
                expiry: 11,
                adapter_support: false,
                expected: Err(CredentialAdmissionError::EnvelopeUnsupported),
            },
        ];

        for rule in rules {
            let selected = holder(PlaintextBoundary::Worker, "worker-a");
            let access = access(BTreeSet::from([selected.clone()])).with_envelope(
                CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: rule.reference.into(),
                        payload_fingerprint: rule.fingerprint.into(),
                    },
                    recipient: TrustDomainRef(rule.recipient.into()),
                    expires_at_unix_ms: rule.expiry,
                },
            );
            let mut installed = capabilities(&selected);
            installed.recipient_bound_envelopes = rule.adapter_support;
            let actual = access
                .admit(
                    &selected,
                    CredentialRealizationKind::WorkerProviderAdapter,
                    &installed,
                    10,
                )
                .map(|_| ());
            assert_eq!(actual, rule.expected, "decision rule {}", rule.id);
        }
    }

    /// Cause-effect graph for refresh client authentication:
    /// C1 auth is public -> C2 client ref absent -> admit;
    /// C1 confidential -> C3 client ref present -> admit. Every other pairing
    /// fails before its fingerprint can authorize secret access.
    ///
    /// | Rule | Auth | Client ref | Result |
    /// |---|---|---|---|
    /// | A1 | none | absent | admitted |
    /// | A2 | none | present | reject |
    /// | A3 | basic | present | admitted |
    /// | A4 | basic | absent | reject |
    /// | A5 | post | present | admitted |
    /// | A6 | post | absent | reject |
    #[test]
    fn refresh_client_authentication_tests_are_generated_from_the_decision_table() {
        let selected = holder(PlaintextBoundary::Worker, "worker-a");
        let rules = [
            ("A1", TokenEndpointAuth::None, None, true),
            ("A2", TokenEndpointAuth::None, Some("client-ref"), false),
            (
                "A3",
                TokenEndpointAuth::ClientSecretBasic,
                Some("client-ref"),
                true,
            ),
            ("A4", TokenEndpointAuth::ClientSecretBasic, None, false),
            (
                "A5",
                TokenEndpointAuth::ClientSecretPost,
                Some("client-ref"),
                true,
            ),
            ("A6", TokenEndpointAuth::ClientSecretPost, None, false),
        ];
        for (id, auth, client_ref, admitted) in rules {
            let refresh = CredentialRefreshAccess::new(
                7,
                "https://auth.example/token".into(),
                "client".into(),
                auth,
                client_ref.map(str::to_owned),
                "refresh-ref".into(),
                "access-ref".into(),
                None,
                None,
            );
            let result = access(BTreeSet::from([selected.clone()]))
                .with_refresh(refresh)
                .admit(
                    &selected,
                    CredentialRealizationKind::WorkerProviderAdapter,
                    &capabilities(&selected),
                    10,
                );
            assert_eq!(result.is_ok(), admitted, "decision rule {id}: {result:?}");
            if !admitted {
                assert_eq!(
                    result.unwrap_err(),
                    CredentialAdmissionError::RefreshClientAuthenticationInvalid,
                    "decision rule {id}"
                );
            }
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
        let persisted = serde_json::to_value(&decoded).unwrap();
        assert!(persisted.get("injection").is_none());
        assert_eq!(
            persisted.get("legacy_direct"),
            Some(&serde_json::Value::Bool(true))
        );
        serde_json::from_value(persisted).expect("decode-only Direct provenance survives storage")
    }

    /// Cause-effect graph: legacy Direct wire value -> decode-only provenance
    /// -> serialization/persistence -> decode -> admission rejection. Losing the
    /// provenance at either serde edge would turn Direct into a Control reference.
    ///
    /// | Rule | Direct input | Persistence round trip | Result |
    /// |---|---|---|---|
    /// | D1 | T | T | DirectPublicationRejected |
    #[test]
    fn legacy_direct_provenance_survives_persistence_and_remains_rejected() {
        let selected = holder(PlaintextBoundary::Worker, "worker-a");
        let access = legacy_direct_access(&selected);
        assert_eq!(
            access
                .admit(
                    &selected,
                    CredentialRealizationKind::WorkerProviderAdapter,
                    &capabilities(&selected),
                    10,
                )
                .unwrap_err(),
            CredentialAdmissionError::DirectPublicationRejected
        );
    }
}
