// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Static owner of the secret-free credential reference vocabulary pinned into
// @formal reason: executable snapshots: exact plaintext holders, material source, recipient-bound
// @formal reason: envelope values and model exposure. Realization admission, envelope validation and
// @formal reason: custody stay in awaken-credential-contract (ADR-0086 §7), whose admission decision
// @formal reason: tables exercise these values; this is a product-requirement classification, not a
// @formal reason: direct formal-proof claim.
//! Credential references pinned into executable Agent snapshots.
//!
//! A reference names one credential source revision, the plaintext holders
//! allowed to see it, how the target consumes it and what the model may see.
//! It never carries material. Custody, sealing, opening and realization
//! admission stay with the Platform credential contract.

use std::collections::BTreeSet;

use serde::{Deserialize, Deserializer, Serialize};

use super::target::CredentialTarget;
use super::usage::CredentialUsage;

/// Stable built-in trust domains used by the self-hosted execution profile.
/// Hosted deployments publish their own opaque domains instead.
pub const SELF_HOSTED_WORKER_TRUST_DOMAIN: &str = "awaken.worker";
pub const SELF_HOSTED_ACP_TRUST_DOMAIN: &str = "awaken.workload.acp";
pub const SELF_HOSTED_SANDBOX_TRUST_DOMAIN: &str = "awaken.workload.sandbox";

/// A stable reference to one credential source revision.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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

/// Who implements safe use after delivery; this never grants access or changes
/// the selected plaintext holder. Historical policies remain platform-managed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialHandlingMode {
    #[default]
    PlatformManaged,
    ApplicationManaged,
}
impl CredentialHandlingMode {
    #[must_use]
    pub const fn is_platform_managed(&self) -> bool {
        matches!(self, Self::PlatformManaged)
    }
}

/// Published authorization for execution-time plaintext and model exposure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialExecutionPolicy {
    #[serde(
        default,
        skip_serializing_if = "CredentialHandlingMode::is_platform_managed"
    )]
    pub handling_mode: CredentialHandlingMode,
    pub allowed_plaintext_holders: BTreeSet<PlaintextHolder>,
    pub model_exposure: ModelExposurePolicy,
}

impl CredentialExecutionPolicy {
    fn self_hosted_holders() -> [PlaintextHolder; 3] {
        [
            PlaintextHolder::new(PlaintextBoundary::Worker, SELF_HOSTED_WORKER_TRUST_DOMAIN),
            PlaintextHolder::new(PlaintextBoundary::Workload, SELF_HOSTED_ACP_TRUST_DOMAIN),
            PlaintextHolder::new(
                PlaintextBoundary::Workload,
                SELF_HOSTED_SANDBOX_TRUST_DOMAIN,
            ),
        ]
    }

    #[must_use]
    pub fn exact(holder: PlaintextHolder, model_exposure: ModelExposurePolicy) -> Self {
        Self {
            handling_mode: CredentialHandlingMode::PlatformManaged,
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
            handling_mode: CredentialHandlingMode::PlatformManaged,
            allowed_plaintext_holders: allowed_plaintext_holders.into_iter().collect(),
            model_exposure,
        }
    }

    /// The registered extension consumer owns plaintext use, not authorization.
    #[must_use]
    pub fn application_managed(holder: PlaintextHolder) -> Self {
        Self {
            handling_mode: CredentialHandlingMode::ApplicationManaged,
            allowed_plaintext_holders: BTreeSet::from([holder]),
            model_exposure: ModelExposurePolicy::Forbidden,
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
    /// The sealed material this envelope transports.
    #[must_use]
    pub fn envelope_ref(&self) -> &SealedCredentialEnvelopeRef {
        match self {
            Self::SealedForWorker { envelope_ref, .. }
            | Self::SealedForWorkload { envelope_ref, .. } => envelope_ref,
        }
    }

    /// The trust domain the envelope is sealed for.
    #[must_use]
    pub fn recipient(&self) -> &TrustDomainRef {
        match self {
            Self::SealedForWorker { recipient, .. } | Self::SealedForWorkload { recipient, .. } => {
                recipient
            }
        }
    }

    /// When the sealed transport stops being usable.
    #[must_use]
    pub fn expires_at_unix_ms(&self) -> u64 {
        match self {
            Self::SealedForWorker {
                expires_at_unix_ms, ..
            }
            | Self::SealedForWorkload {
                expires_at_unix_ms, ..
            } => *expires_at_unix_ms,
        }
    }

    /// The plaintext boundary the recipient must be.
    #[must_use]
    pub fn required_boundary(&self) -> PlaintextBoundary {
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
        crate::fingerprint::stable_fingerprint(&(
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
    /// Exact provider-neutral purpose/audience selected by the consumer. Legacy
    /// rows may omit it; a described source requires it at materialization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CredentialTarget>,
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
    #[serde(default)]
    target: Option<CredentialTarget>,
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
            target: wire.target,
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
            target: None,
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

    /// Bind this access to one exact source-declared purpose/audience.
    #[must_use]
    pub fn with_target(mut self, target: CredentialTarget) -> Self {
        self.target = Some(target);
        self
    }

    #[must_use]
    pub fn with_refresh(mut self, refresh: CredentialRefreshAccess) -> Self {
        self.refresh = Some(refresh);
        self
    }

    /// Whether this access was decoded from the retired `direct` injection.
    /// The provenance survives persistence so realization admission keeps
    /// rejecting it.
    #[must_use]
    pub fn is_legacy_direct(&self) -> bool {
        self.legacy_direct
    }
}

/// Content identity of one complete published model candidate. Attempt-level
/// credential decisions and their receipts name the candidate by this value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CandidateFingerprint(pub String);
