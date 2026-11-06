// @formal entrypoint credential_effect.boundary_preserves_exact_materialization_authority
// @formal entrypoint credential_reference.exact_revision_never_resolves_another_revision
// @formal entrypoint worker.revoked_credential_blocks_new_materialization
// @formal evidence worker.secret_materialization_is_claim_fenced
//! Exact credential realization for publication-pinned runtime access.
//!
//! This host adapter owns no selection policy and cannot enumerate credentials. It
//! accepts one immutable [`ResolvedModelCandidate`], validates its exact
//! holder, target and claim, then calls the installed material authority once.
//! The Control Vault adapter alone rechecks the persisted source row and opens
//! sealed bytes. Native provider execution, optional brokered inference, and
//! ACP provisioning share this adapter so candidate routing cannot drift.

use awaken_credential_contract::CredentialAccessAdmission;
use awaken_credential_contract::CredentialEnvelopeValidation;

mod credential_artifact;
mod http_credential;
mod inference;
mod platform_egress;
mod platform_relay;
mod secret_broker;

#[cfg(test)]
pub(crate) struct NoopDeltaSink;

#[cfg(test)]
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::DeltaSink for NoopDeltaSink {
    async fn on_text(&self, _: &str) {}
}

pub use http_credential::materialized_http_credential;
pub use inference::CredentialInferenceMaterializer;
pub use platform_egress::{
    MediatedMcpRoute, PlatformMcpEgressAllocation, PlatformMcpEgressProvisioner,
    PlatformMcpEgressRequest,
};

use awaken_sync::InvariantLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

mod admission;
#[cfg(all(test, feature = "authority"))]
mod attempt_delivery_tests;
mod claimed_materialization;
mod material_capabilities;
use admission::{ExpectedProviderTarget, admit_exact_adapter};
use awaken_time::WallClockExt as _;

#[cfg(all(test, feature = "authority"))]
use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::{
    CredentialAccess, CredentialMaterialSource, CredentialUsage, PlaintextHolder,
};
use awaken_credential_contract::{
    CredentialMaterialBinding, CredentialMaterialError, CredentialMaterialRequest,
    CredentialMaterialResolver, CredentialRealizationKind, ResolvedCredentialMaterial,
};
use awaken_credential_contract::{
    candidate_credential_material_binding, credential_access_has_same_authority,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

#[derive(Clone)]
struct RegisteredExtensionConsumer {
    descriptor: awaken_credential_contract::CredentialExtensionDescriptor,
    consumer: Arc<dyn awaken_credential_contract::CredentialExtensionConsumer>,
}

/// Worker/host-side realization of one already-published credential reference.
#[derive(Clone)]
pub struct PinnedCredentialMaterializer {
    clock: Arc<dyn awaken_time::WallClock>,
    #[cfg(feature = "authority")]
    local_material_resolver: Option<Arc<dyn CredentialMaterialResolver>>,
    #[cfg(feature = "authority")]
    local_secret_broker: Option<Arc<dyn awaken_provisioning_contract::SecretBroker>>,
    external_material_resolver: Option<Arc<dyn CredentialMaterialResolver>>,
    extension_consumers: Arc<BTreeMap<String, RegisteredExtensionConsumer>>,
    pending_process_secrets:
        Arc<InvariantLock<HashMap<String, secret_broker::PendingProcessSecret>>>,
    pending_credential_artifacts:
        Arc<InvariantLock<HashMap<String, secret_broker::PendingCredentialArtifact>>>,
}

impl PinnedCredentialMaterializer {
    #[cfg(feature = "authority")]
    #[must_use]
    pub fn new<A>(local_authority: Arc<A>, clock: Arc<dyn awaken_time::WallClock>) -> Self
    where
        A: CredentialMaterialResolver + awaken_provisioning_contract::SecretBroker + 'static,
    {
        Self::from_local_roles(local_authority.clone(), local_authority, clock)
    }

    /// Consume the resolver and broker selected together by the composition root.
    #[cfg(feature = "authority")]
    #[must_use]
    pub fn from_local_roles(
        local_material_resolver: Arc<dyn CredentialMaterialResolver>,
        local_secret_broker: Arc<dyn awaken_provisioning_contract::SecretBroker>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            clock,
            local_material_resolver: Some(local_material_resolver),
            local_secret_broker: Some(local_secret_broker),
            external_material_resolver: None,
            extension_consumers: Arc::new(BTreeMap::new()),
            pending_process_secrets: Arc::new(InvariantLock::new(HashMap::new())),
            pending_credential_artifacts: Arc::new(InvariantLock::new(HashMap::new())),
        }
    }

    /// Construct the same exact materialization adapter without a local Vault.
    ///
    /// Authority-store-isolated Workers use this path for sources supported by
    /// the installed resolver. No locally backed `ControlPlaneReference` is
    /// advertised; callers do not need an empty repository or secret-store
    /// placeholder merely to install the neutral resolver port.
    #[must_use]
    pub fn external_only(
        resolver: Arc<dyn CredentialMaterialResolver>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            clock,
            #[cfg(feature = "authority")]
            local_material_resolver: None,
            #[cfg(feature = "authority")]
            local_secret_broker: None,
            external_material_resolver: Some(resolver),
            extension_consumers: Arc::new(BTreeMap::new()),
            pending_process_secrets: Arc::new(InvariantLock::new(HashMap::new())),
            pending_credential_artifacts: Arc::new(InvariantLock::new(HashMap::new())),
        }
    }

    /// Install the sole non-local material-source adapter. It handles exact
    /// Worker references and recipient-bound envelopes through the same neutral
    /// resolver contract; absence fails closed and never falls back to Vault.
    #[must_use]
    pub fn with_external_material_resolver(
        mut self,
        resolver: Arc<dyn CredentialMaterialResolver>,
    ) -> Self {
        self.external_material_resolver = Some(resolver);
        self
    }

    /// Register one externally owned last-mile consumer. The descriptor is the
    /// single source for both Worker capability discovery and runtime dispatch;
    /// duplicate ids and invalid descriptors fail closed at composition time.
    pub fn with_extension_consumer(
        mut self,
        consumer: Arc<dyn awaken_credential_contract::CredentialExtensionConsumer>,
    ) -> Result<Self, CredentialExtensionRegistryError> {
        let descriptor = consumer.descriptor();
        if descriptor.consumer_id.trim().is_empty()
            || descriptor.material_types.is_empty()
            || descriptor
                .material_types
                .iter()
                .any(|material_type| material_type.trim().is_empty())
        {
            return Err(CredentialExtensionRegistryError::InvalidDescriptor);
        }
        let consumers = Arc::make_mut(&mut self.extension_consumers);
        if consumers.contains_key(&descriptor.consumer_id) {
            return Err(CredentialExtensionRegistryError::DuplicateConsumer(
                descriptor.consumer_id,
            ));
        }
        consumers.insert(
            descriptor.consumer_id.clone(),
            RegisteredExtensionConsumer {
                descriptor,
                consumer,
            },
        );
        Ok(self)
    }

    fn extension_capabilities(&self) -> BTreeMap<String, std::collections::BTreeSet<String>> {
        self.extension_consumers
            .iter()
            .map(|(consumer_id, registration)| {
                (
                    consumer_id.clone(),
                    registration.descriptor.material_types.clone(),
                )
            })
            .collect()
    }

    fn has_local_authority(&self) -> bool {
        #[cfg(feature = "authority")]
        {
            self.local_material_resolver.is_some() && self.local_secret_broker.is_some()
        }
        #[cfg(not(feature = "authority"))]
        {
            false
        }
    }

    /// Resolve and dispatch one exact extension binding without exposing its
    /// plaintext to the caller. Selection, holder admission, revision/workspace
    /// fences, material-type validation, consumer dispatch, and receipt checks
    /// are one fail-closed operation.
    pub async fn consume_extension_for_workspace<T: serde::Serialize>(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        workspace: &str,
        target: &T,
    ) -> Result<awaken_credential_contract::CredentialExtensionReceipt, CredentialMaterialError>
    {
        let CredentialUsage::Extension {
            consumer_id,
            material_type,
            ..
        } = &access.usage
        else {
            return Err(CredentialMaterialError::MaterialKindMismatch);
        };
        let registration = self
            .extension_consumers
            .get(consumer_id)
            .ok_or(CredentialMaterialError::Unavailable)?;
        access
            .admit(
                selected_holder,
                CredentialRealizationKind::WorkerRelay,
                &self.worker_relay_capabilities(),
                self.clock.now_ms(),
            )
            .map_err(|_| CredentialMaterialError::Unavailable)?;
        let binding = CredentialMaterialBinding::for_target(workspace, target, &access.usage);
        let resolved = self
            .resolve_validated(access, selected_holder, binding.clone(), None)
            .await?;
        let awaken_credential_contract::CredentialMaterial::Structured(material) =
            &resolved.material
        else {
            return Err(CredentialMaterialError::MaterialKindMismatch);
        };
        let receipt = registration
            .consumer
            .consume(awaken_credential_contract::CredentialExtensionRequest {
                access,
                material,
                binding: &binding,
            })
            .await?;
        if receipt.consumer_id != *consumer_id
            || receipt.material_type != *material_type
            || receipt.target_use_fingerprint != binding.target_use_fingerprint
        {
            return Err(CredentialMaterialError::ResolverMismatch);
        }
        Ok(receipt)
    }

    /// Materialize exactly the provider credential frozen in `access`.
    ///
    /// Catalog lookup, default selection and failover are intentionally absent.
    /// Every mismatch is terminal for this pin; the caller may only try another
    /// complete candidate that was already included in the published snapshot.
    // Only the authority test module uses this convenience projection. Neutral
    // tests exercise the same production materialize_provider_for boundary.
    #[cfg(all(test, feature = "authority"))]
    async fn materialize_provider(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<RedactedString, String> {
        self.materialize_provider_for(
            candidate,
            None,
            &PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .await?
        .into_secret()
        .map_err(|error| error.to_string())
    }

    /// Materialize for the exact holder and mechanism selected by admission.
    async fn materialize_provider_for(
        &self,
        candidate: &ResolvedModelCandidate,
        delivered_access: Option<&CredentialAccess>,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> Result<awaken_credential_contract::CredentialMaterial, String> {
        let ModelProvisioning::Provider { .. } = candidate.provisioning() else {
            return Err("model candidate does not require a provider credential".into());
        };
        let expectation = awaken_credential_contract::candidate_provider_expectation(candidate)
            .map_err(str::to_string)?
            .ok_or_else(|| "provider candidate has no exact source coordinate".to_string())?;
        let (published, material_binding) = candidate_credential_material_binding(candidate)
            .ok_or_else(|| "published model candidate has no credential pin".to_string())?;
        let credential = delivered_access.unwrap_or(published);
        if !credential_access_has_same_authority(credential, published) {
            return Err("claim delivery changes published credential authority".into());
        }
        if !credential.usage.is_scalar_process_delivery() {
            return Err("published credential injection contract is invalid".to_string());
        }
        admit_exact_adapter(
            credential,
            selected_holder,
            realization,
            &self.realization_capabilities(selected_holder.clone(), realization),
            self.clock.now_ms(),
        )
        .map_err(|error| error.to_string())?;
        let resolved = self
            .resolve_validated(
                credential,
                selected_holder,
                material_binding,
                Some(expectation.as_borrowed()),
            )
            .await
            .map_err(|error| error.to_string())?;
        Ok(resolved.material)
    }

    async fn materialize_remote_for(
        &self,
        candidate: &ResolvedModelCandidate,
        delivered_access: Option<&CredentialAccess>,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
    ) -> Result<awaken_credential_contract::CredentialMaterial, String> {
        let ModelProvisioning::Remote { .. } = candidate.provisioning() else {
            return Err("model candidate does not require remote transport authentication".into());
        };
        let (published, material_binding) = candidate_credential_material_binding(candidate)
            .ok_or_else(|| "remote candidate has no credential pin".to_string())?;
        let credential = delivered_access.unwrap_or(published);
        if !credential_access_has_same_authority(credential, published) {
            return Err("claim delivery changes published credential authority".into());
        }
        if !matches!(credential.usage, CredentialUsage::HttpHeader { .. }) {
            return Err("remote credential injection contract is not an HTTP header".into());
        }
        admit_exact_adapter(
            credential,
            selected_holder,
            realization,
            &self.realization_capabilities(selected_holder.clone(), realization),
            self.clock.now_ms(),
        )
        .map_err(|error| error.to_string())?;
        let resolved = self
            .resolve_validated(credential, selected_holder, material_binding, None)
            .await
            .map_err(|error| error.to_string())?;
        Ok(resolved.material)
    }

    fn exact_resolver(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        binding: &CredentialMaterialBinding,
    ) -> Result<(&dyn CredentialMaterialResolver, bool), CredentialMaterialError> {
        binding.validate()?;
        if !access
            .policy
            .allowed_plaintext_holders
            .contains(selected_holder)
        {
            return Err(CredentialMaterialError::RecipientMismatch);
        }
        if let Some(envelope) = &access.envelope {
            envelope.validate_for_holder(selected_holder, self.clock.now_ms())?;
        }
        awaken_credential_contract::validate_credential_target_usage(
            access.target.as_ref(),
            &access.usage,
        )
        .map_err(|_| CredentialMaterialError::BindingMismatch)?;
        let external = access.envelope.is_some()
            || access.material_source == CredentialMaterialSource::WorkerReference
            || !self.has_local_authority();
        let resolver = if external {
            self.external_material_resolver
                .as_ref()
                .ok_or(CredentialMaterialError::Unavailable)?
        } else {
            if access.material_source != CredentialMaterialSource::ControlPlaneReference {
                return Err(CredentialMaterialError::Unavailable);
            }
            #[cfg(feature = "authority")]
            {
                self.local_material_resolver
                    .as_ref()
                    .ok_or(CredentialMaterialError::Unavailable)?
            }
            #[cfg(not(feature = "authority"))]
            {
                return Err(CredentialMaterialError::Unavailable);
            }
        };
        Ok((resolver.as_ref(), external))
    }

    /// Validate a mediated reference without crossing its plaintext boundary.
    pub async fn validate_reference_for_workspace(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        workspace: &str,
        target: &impl serde::Serialize,
    ) -> Result<(), CredentialMaterialError> {
        admit_exact_adapter(
            access,
            selected_holder,
            realization,
            &self.realization_capabilities(selected_holder.clone(), realization),
            self.clock.now_ms(),
        )
        .map_err(admission_material_error)?;
        let binding = CredentialMaterialBinding::for_target(workspace, target, &access.usage);
        self.exact_resolver(access, selected_holder, &binding)?
            .0
            .validate_reference(CredentialMaterialRequest {
                access,
                selected_holder,
                binding: &binding,
                expected_provider: None,
            })
            .await
    }

    async fn resolve_validated(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        binding: CredentialMaterialBinding,
        expected_provider: Option<ExpectedProviderTarget<'_>>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        let (resolver, external) = self.exact_resolver(access, selected_holder, &binding)?;
        let capability = awaken_credential_contract::CredentialMaterialCapability {
            holder: selected_holder.clone(),
            source: access.material_source,
            envelope: access.envelope.is_some(),
        };
        if external && !resolver.material_capabilities().contains(&capability) {
            return Err(CredentialMaterialError::Unavailable);
        }
        let resolved = resolver
            .resolve_exact(CredentialMaterialRequest {
                access,
                selected_holder,
                binding: &binding,
                expected_provider,
            })
            .await?;
        if resolved.credential != access.credential || resolved.holder != *selected_holder {
            return Err(CredentialMaterialError::ResolverMismatch);
        }
        resolved.material.validate_usage(&access.usage)?;
        Ok(resolved)
    }

    /// Resolve one exact credential within the selected Workspace boundary.
    pub async fn resolve_for_workspace(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        workspace: &str,
        target: &impl serde::Serialize,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        admit_exact_adapter(
            access,
            selected_holder,
            realization,
            &self.realization_capabilities(selected_holder.clone(), realization),
            self.clock.now_ms(),
        )
        .map_err(admission_material_error)?;
        self.resolve_validated(
            access,
            selected_holder,
            CredentialMaterialBinding::for_target(workspace, target, &access.usage),
            None,
        )
        .await
    }

    /// Resolve one exact credential for a provider-owned adapter. A source
    /// explicitly namespaced to another provider is rejected before material
    /// crosses the plaintext boundary.
    pub async fn resolve_for_workspace_and_provider(
        &self,
        access: &CredentialAccess,
        selected_holder: &PlaintextHolder,
        realization: CredentialRealizationKind,
        workspace: &str,
        provider: &str,
        target: &impl serde::Serialize,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        admit_exact_adapter(
            access,
            selected_holder,
            realization,
            &self.realization_capabilities(selected_holder.clone(), realization),
            self.clock.now_ms(),
        )
        .map_err(admission_material_error)?;
        self.resolve_validated(
            access,
            selected_holder,
            CredentialMaterialBinding::for_target(workspace, target, &access.usage),
            Some(ExpectedProviderTarget {
                provider_id: provider,
                protocol_endpoint_id: None,
            }),
        )
        .await
    }
}

/// Keep binding refusals from the public admission step in the same class
/// `resolve_validated` reports for them. Only capability gaps of this
/// installation stay `Unavailable`.
fn admission_material_error(
    error: awaken_credential_contract::CredentialAdmissionError,
) -> CredentialMaterialError {
    use awaken_credential_contract::CredentialAdmissionError as Admission;
    match error {
        Admission::HolderNotAllowed | Admission::EnvelopeRecipientMismatch => {
            CredentialMaterialError::RecipientMismatch
        }
        Admission::CredentialRevisionMismatch => CredentialMaterialError::RevisionMismatch,
        Admission::EnvelopeExpired => CredentialMaterialError::EnvelopeExpired,
        _ => CredentialMaterialError::Unavailable,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialExtensionRegistryError {
    #[error("credential extension descriptor is invalid")]
    InvalidDescriptor,
    #[error("credential extension consumer `{0}` is already registered")]
    DuplicateConsumer(String),
}

#[async_trait::async_trait]
impl CredentialMaterialResolver for PinnedCredentialMaterializer {
    fn material_capabilities(
        &self,
    ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability> {
        self.installed_material_capabilities()
    }

    async fn validate_reference(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<(), CredentialMaterialError> {
        self.exact_resolver(request.access, request.selected_holder, request.binding)?
            .0
            .validate_reference(request)
            .await
    }

    async fn resolve_exact(
        &self,
        request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        self.resolve_validated(
            request.access,
            request.selected_holder,
            request.binding.clone(),
            request.expected_provider,
        )
        .await
    }
}

#[cfg(all(test, feature = "authority"))]
pub(crate) fn pinned_vault(
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> PinnedCredentialMaterializer {
    PinnedCredentialMaterializer::new(
        Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
            credentials,
            secrets,
            clock.clone(),
        )),
        clock,
    )
}

// These fixtures exercise the local Vault authority constructor. The
// database-less Worker path is covered by `S10` in this module when authority
// tests run, by the no-default-features build matrix, and by the standalone
// Worker materialization E2E; compiling local-store fixtures into that feature
// profile would itself violate the boundary under test.
#[cfg(all(test, feature = "authority"))]
mod tests {
    mod binding_matrix;
    mod native_capabilities;
    mod revocation_wait;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use awaken_agent_contract::credential::{
        CredentialEnvelope, CredentialExecutionPolicy, CredentialRef, CredentialRefreshAccess,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, SealedCredentialEnvelopeRef,
        TokenEndpointAuth,
    };
    use awaken_credential_contract::CredentialMaterial;
    use awaken_credential_contract::CredentialRealizationCapabilities;
    use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialError, CredentialKind, CredentialStatus,
        InMemorySecretStore, SecretRef, SecretStore,
    };
    use awaken_runtime_contract::{InferenceEndpoint, ModelBinding};

    #[derive(Clone)]
    struct ExactExternalResolver {
        expected_binding: CredentialMaterialBinding,
        expected_payload_fingerprint: &'static str,
        returned_holder: PlaintextHolder,
    }

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ExactExternalResolver {
        fn material_capabilities(
            &self,
        ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability>
        {
            let mut profiles = std::collections::BTreeSet::new();
            for holder in [
                awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Worker,
                    "awaken.worker",
                ),
                awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Workload,
                    "awaken.workload.acp",
                ),
            ] {
                for source in [
                    awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
                    awaken_agent_contract::credential::CredentialMaterialSource::WorkerReference,
                ] {
                    for envelope in [false, true] {
                        profiles.insert(awaken_credential_contract::CredentialMaterialCapability {
                            holder: holder.clone(),
                            source,
                            envelope,
                        });
                    }
                }
            }
            profiles
        }

        async fn resolve_exact(
            &self,
            request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            request.binding.validate()?;
            if request.binding != &self.expected_binding {
                return Err(CredentialMaterialError::BindingMismatch);
            }
            if let Some(envelope) = &request.access.envelope {
                let payload_fingerprint = match envelope {
                    CredentialEnvelope::SealedForWorker { envelope_ref, .. }
                    | CredentialEnvelope::SealedForWorkload { envelope_ref, .. } => {
                        &envelope_ref.payload_fingerprint
                    }
                };
                if payload_fingerprint != self.expected_payload_fingerprint {
                    return Err(CredentialMaterialError::PayloadMismatch);
                }
            }
            Ok(ResolvedCredentialMaterial {
                credential: request.access.credential.clone(),
                holder: self.returned_holder.clone(),
                material: awaken_credential_contract::CredentialMaterial::secret(
                    RedactedString::new("external-sealed-material"),
                ),
            })
        }
    }

    #[derive(Clone)]
    struct ResolutionRule {
        id: &'static str,
        control_source: bool,
        no_envelope: bool,
        holder_allowed: bool,
        source_exists: bool,
        source_active: bool,
        revision_exact: bool,
        workspace_exact: bool,
        provider_exact: bool,
        material_available: bool,
        expected: Result<(), CredentialMaterialError>,
    }

    fn selected_holder() -> PlaintextHolder {
        PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        )
    }

    /// Cause-effect graph:
    ///
    /// P0 the access carries the exact Provider target/usage; its negative
    /// combinations are owned by the credential-contract decision table.
    /// C0 control-plane material source -> C1 no sealed envelope
    ///  -> C2 selected holder authorized -> C3 source exists -> C4 source active
    ///  -> C5 exact revision -> C6 exact Workspace -> C7 exact provider
    ///  -> C8 material opens -> E1 exact material returned.
    ///
    /// A failed cause terminates resolution with its stable, secret-free E2
    /// error. The table uses `T` for a satisfied cause, `F` for the one failed
    /// cause, and `-` for causes that are not evaluated.
    ///
    /// | Rule | C0 | C1 | C2 | C3 | C4 | C5 | C6 | C7 | C8 | Result |
    /// |---|---|---|---|---|---|---|---|---|---|---|
    /// | R1 | T | T | T | T | T | T | T | T | T | material |
    /// | R2 | F | - | - | - | - | - | - | - | - | unavailable |
    /// | R3 | T | F | - | - | - | - | - | - | - | unavailable |
    /// | R4 | T | T | F | - | - | - | - | - | - | recipient mismatch |
    /// | R5 | T | T | T | F | - | - | - | - | - | unavailable |
    /// | R6 | T | T | T | T | F | - | - | - | - | unavailable |
    /// | R7 | T | T | T | T | T | F | - | - | - | revision mismatch |
    /// | R8 | T | T | T | T | T | T | F | - | - | recipient mismatch |
    /// | R9 | T | T | T | T | T | T | T | F | - | recipient mismatch |
    /// | R10 | T | T | T | T | T | T | T | T | F | unavailable |
    #[tokio::test]
    async fn exact_resolution_tests_are_generated_from_the_decision_table() {
        let valid = ResolutionRule {
            id: "R1",
            control_source: true,
            no_envelope: true,
            holder_allowed: true,
            source_exists: true,
            source_active: true,
            revision_exact: true,
            workspace_exact: true,
            provider_exact: true,
            material_available: true,
            expected: Ok(()),
        };
        let rules = [
            valid.clone(),
            ResolutionRule {
                id: "R2",
                control_source: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R3",
                no_envelope: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R4",
                holder_allowed: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R5",
                source_exists: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R6",
                source_active: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R7",
                revision_exact: false,
                expected: Err(CredentialMaterialError::RevisionMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R8",
                workspace_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R9",
                provider_exact: false,
                expected: Err(CredentialMaterialError::RecipientMismatch),
                ..valid.clone()
            },
            ResolutionRule {
                id: "R10",
                material_available: false,
                expected: Err(CredentialMaterialError::Unavailable),
                ..valid
            },
        ];

        for rule in rules {
            let credentials = Arc::new(InMemoryCredentialRepo::new());
            let secrets = Arc::new(InMemorySecretStore::new());
            let mut credential_id = "missing-credential".to_string();
            if rule.source_exists {
                let mut source = enter_credential(
                    CredentialCreateParams {
                        workspace_id: if rule.workspace_exact {
                            "workspace-a".into()
                        } else {
                            "workspace-b".into()
                        },
                        kind: CredentialKind::Vault,
                        provider_id: Some(if rule.provider_exact {
                            "anthropic".into()
                        } else {
                            "other-provider".into()
                        }),
                        env_key: None,
                        secret: Some(RedactedString::new("decision-table-secret")),
                        oauth_command: None,
                    },
                    secrets.as_ref(),
                    credentials.as_ref(),
                    awaken_time::system_clock().as_ref(),
                )
                .await
                .expect("fixture credential");
                credential_id.clone_from(&source.id.0);
                if !rule.source_active {
                    source.status = CredentialStatus::Disabled;
                }
                if !rule.material_available {
                    source.material_ref = None;
                }
                credentials.put(source).await.expect("fixture row");
            }

            let holder = selected_holder();
            let policy_holder = if rule.holder_allowed {
                holder.clone()
            } else {
                PlaintextHolder::new(PlaintextBoundary::Platform, "another-domain")
            };
            let mut access = CredentialAccess::new(
                CredentialRef {
                    id: credential_id,
                    revision: if rule.revision_exact { 1 } else { 2 },
                },
                if rule.control_source {
                    CredentialMaterialSource::ControlPlaneReference
                } else {
                    CredentialMaterialSource::WorkerReference
                },
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(policy_holder, ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
                "anthropic",
            ));
            if !rule.no_envelope {
                access = access.with_envelope(CredentialEnvelope::SealedForWorker {
                    envelope_ref: SealedCredentialEnvelopeRef {
                        id: "sealed-1".into(),
                        payload_fingerprint: "fp".into(),
                    },
                    recipient: holder.trust_domain.clone(),
                    expires_at_unix_ms: u64::MAX,
                });
            }
            let resolver = pinned_vault(credentials, secrets, awaken_time::system_clock());
            let result = resolver
                .resolve_validated(
                    &access,
                    &holder,
                    CredentialMaterialBinding::for_target(
                        "workspace-a",
                        &"anthropic",
                        &access.usage,
                    ),
                    Some(ExpectedProviderTarget {
                        provider_id: "anthropic",
                        protocol_endpoint_id: None,
                    }),
                )
                .await;
            assert!(
                !format!("{result:?}").contains("decision-table-secret"),
                "{} keeps errors and debug output secret-free",
                rule.id
            );
            match (rule.expected, result) {
                (Ok(()), Ok(material)) => {
                    assert_eq!(material.credential, access.credential, "{}", rule.id);
                    assert_eq!(material.holder, holder, "{}", rule.id);
                    assert_eq!(
                        material.material.single_secret().unwrap().expose_secret(),
                        "decision-table-secret"
                    );
                }
                (Err(expected), Err(actual)) => assert_eq!(actual, expected, "{}", rule.id),
                (expected, actual) => panic!(
                    "{}: expected {expected:?}, got {:?}",
                    rule.id,
                    actual.map(|_| ())
                ),
            }
        }
    }

    #[tokio::test]
    async fn exact_control_reference_resolves_one_oauth_bundle_for_the_provider_driver() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: None,
                secret: Some(RedactedString::new("access-token")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("access-token credential");
        let access_ref = source
            .material_ref
            .as_ref()
            .expect("vault reference")
            .0
            .clone();
        let refresh_ref = SecretRef("oauth-refresh-token".into());
        secrets
            .put(&refresh_ref, RedactedString::new("refresh-token"))
            .await
            .expect("refresh-token material");

        let holder = selected_holder();
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).expect("positive revision"),
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(awaken_agent_contract::credential::CredentialTarget::new(
            awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
            "openai",
        ))
        .with_refresh(
            CredentialRefreshAccess::new(
                u64::try_from(source.version).expect("positive revision"),
                "https://auth.openai.com/oauth/token".into(),
                "client-id".into(),
                TokenEndpointAuth::None,
                None,
                refresh_ref.0,
                access_ref,
                Some("openid profile offline_access".into()),
                None,
            )
            .with_provider_metadata(
                Some("account-1".into()),
                Some("pro".into()),
                Some(u64::MAX),
            ),
        );

        let resolved = pinned_vault(credentials, secrets, awaken_time::system_clock())
            .resolve_validated(
                &access,
                &holder,
                CredentialMaterialBinding::for_target("workspace-a", &"openai", &access.usage),
                Some(ExpectedProviderTarget {
                    provider_id: "openai",
                    protocol_endpoint_id: None,
                }),
            )
            .await
            .expect("exact OAuth bundle");

        let CredentialMaterial::OAuth(bundle) = resolved.material else {
            panic!("OAuth access must not degrade to a bearer-only material")
        };
        assert_eq!(bundle.access_token.expose_secret(), "access-token");
        assert_eq!(bundle.refresh_token.expose_secret(), "refresh-token");
        assert_eq!(bundle.account_id.as_deref(), Some("account-1"));
        assert_eq!(bundle.account_plan.as_deref(), Some("pro"));
        assert_eq!(bundle.expires_at_unix_ms, Some(u64::MAX));
    }

    /// Last-mile source-authority cause/effect graph: C1 persisted source owns
    /// provider A at endpoint 1; C2 the published access targets A; C3 the
    /// realized route is endpoint 1. Effects: E1 all exact causes open the
    /// pinned material; E2 !C2 rejects even without a separate route hint; E3
    /// !C3 rejects an endpoint-confused candidate before material use. C4 the
    /// caller enters through the neutral `CredentialMaterialResolver` trait;
    /// the same exact expectation must survive that adapter boundary.
    ///
    /// | Rule | Source scope | Access target | Route | Effect |
    /// |---|---|---|---|---|
    /// | T1 | A/1 | A | A/1 | E1 material |
    /// | T2 | A/1 | B | none | E2 binding mismatch |
    /// | T3 | A/1 | A | A/2 | E3 recipient mismatch |
    /// | T4 | A/1 | A | A/1 via trait | E1 material |
    /// | T5 | A/1 | A | absent via trait | E2 binding mismatch |
    /// | T6 | A/1 | A | A/2 via trait | E3 recipient mismatch |
    #[tokio::test]
    async fn materialization_revalidates_the_source_owned_exact_target() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let mut source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("provider-a".into()),
                env_key: None,
                secret: Some(RedactedString::new("target-bound-secret")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        source.protocol_endpoint_id = Some("endpoint-1".into());
        credentials.put(source.clone()).await.unwrap();
        let holder = selected_holder();
        let access_for = |audience: &str| {
            CredentialAccess::new(
                CredentialRef {
                    id: source.id.0.clone(),
                    revision: u64::try_from(source.version).unwrap(),
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
                audience,
            ))
        };
        let materializer = pinned_vault(credentials, secrets, awaken_time::system_clock());
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"provider-a",
            &CredentialUsage::ProviderAdapter,
        );

        assert!(
            materializer
                .resolve_validated(
                    &access_for("provider-a"),
                    &holder,
                    binding.clone(),
                    Some(ExpectedProviderTarget {
                        provider_id: "provider-a",
                        protocol_endpoint_id: Some("endpoint-1"),
                    }),
                )
                .await
                .is_ok(),
            "T1/E1"
        );
        assert_eq!(
            materializer
                .resolve_validated(&access_for("provider-b"), &holder, binding, None)
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch,
            "T2/E2"
        );
        assert_eq!(
            materializer
                .resolve_validated(
                    &access_for("provider-a"),
                    &holder,
                    CredentialMaterialBinding::for_target(
                        "workspace-a",
                        &"provider-a/endpoint-2",
                        &CredentialUsage::ProviderAdapter,
                    ),
                    Some(ExpectedProviderTarget {
                        provider_id: "provider-a",
                        protocol_endpoint_id: Some("endpoint-2"),
                    }),
                )
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "T3/E3"
        );
        let access = access_for("provider-a");
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"provider-a",
            &CredentialUsage::ProviderAdapter,
        );
        let resolver: &dyn CredentialMaterialResolver = &materializer;
        let exact = ExpectedProviderTarget {
            provider_id: "provider-a",
            protocol_endpoint_id: Some("endpoint-1"),
        };
        assert!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: Some(exact),
                })
                .await
                .is_ok(),
            "T4/E1"
        );
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: None,
                })
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch,
            "T5/E2"
        );
        assert_eq!(
            resolver
                .resolve_exact(CredentialMaterialRequest {
                    access: &access,
                    selected_holder: &holder,
                    binding: &binding,
                    expected_provider: Some(ExpectedProviderTarget {
                        provider_id: "provider-a",
                        protocol_endpoint_id: Some("endpoint-2"),
                    }),
                })
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "T6/E3"
        );
    }

    /// Cause-effect graph for the one external material-source path:
    ///
    /// C1 external resolver installed -> C2 envelope recipient/live
    /// -> C3 exact target/use binding -> C4 exact payload fingerprint
    /// -> C5 resolver returns the selected credential/holder -> E1 material.
    /// A WorkerReference without an envelope uses the same C1/C3/C5 path. No
    /// failed external cause falls back to the local Vault row.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | Source | Result |
    /// |---|---|---|---|---|---|---|---|
    /// | S1 | T | T | T | T | T | envelope | material |
    /// | S2 | F | - | - | - | - | envelope | unavailable |
    /// | S3 | T | T | F | - | - | envelope | binding mismatch |
    /// | S4 | T | T | T | F | - | envelope | payload mismatch |
    /// | S5 | T | T | T | T | F | envelope | resolver mismatch |
    /// | S6 | T | F(expired) | - | - | - | envelope | expired |
    /// | S7 | T | F(recipient) | - | - | - | envelope | recipient mismatch |
    /// | S8 | T | - | T | - | T | worker reference | material |
    /// | S9 | T | T | T | T | T | inference envelope | material |
    /// | S10 | T | - | T | - | T | external-only Worker ref | material, no local stores |
    #[tokio::test]
    async fn external_resolution_tests_are_generated_from_the_decision_table() {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("must-not-fallback-to-local")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("local fallback trap");
        let holder = selected_holder();
        let binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"https://service.example/mcp",
            &CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        );
        let access = |recipient: &str, expiry: u64, fingerprint: &str| {
            CredentialAccess::new(
                CredentialRef {
                    id: source.id.0.clone(),
                    revision: 1,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::HttpHeader {
                    name: "authorization".into(),
                    scheme: Some("Bearer".into()),
                },
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
                "https://service.example/mcp",
            ))
            .with_envelope(CredentialEnvelope::SealedForWorker {
                envelope_ref: SealedCredentialEnvelopeRef {
                    id: "sealed-1".into(),
                    payload_fingerprint: fingerprint.into(),
                },
                recipient: awaken_agent_contract::credential::TrustDomainRef(recipient.into()),
                expires_at_unix_ms: expiry,
            })
        };
        let resolver = |expected_binding: CredentialMaterialBinding,
                        expected_payload_fingerprint: &'static str,
                        returned_holder: PlaintextHolder| {
            Arc::new(ExactExternalResolver {
                expected_binding,
                expected_payload_fingerprint,
                returned_holder,
            }) as Arc<dyn CredentialMaterialResolver>
        };
        let base = pinned_vault(credentials, secrets, awaken_time::system_clock());

        let exact_access = access(&holder.trust_domain.0, u64::MAX, "sha256:sealed-payload");
        let exact = base.clone().with_external_material_resolver(resolver(
            binding.clone(),
            "sha256:sealed-payload",
            holder.clone(),
        ));
        let resolved = exact
            .resolve_validated(&exact_access, &holder, binding.clone(), None)
            .await
            .expect("S1 exact envelope");
        assert_eq!(
            resolved.material.single_secret().unwrap().expose_secret(),
            "external-sealed-material"
        );

        assert_eq!(
            base.resolve_validated(&exact_access, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::Unavailable,
            "S2"
        );
        let other_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &"https://other.example/mcp",
            &exact_access.usage,
        );
        assert_eq!(
            exact
                .resolve_validated(&exact_access, &holder, other_binding, None)
                .await
                .unwrap_err(),
            CredentialMaterialError::BindingMismatch,
            "S3"
        );
        let bad_payload = access(&holder.trust_domain.0, u64::MAX, "sha256:tampered");
        assert_eq!(
            exact
                .resolve_validated(&bad_payload, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::PayloadMismatch,
            "S4"
        );
        let wrong_return = base.clone().with_external_material_resolver(resolver(
            binding.clone(),
            "sha256:sealed-payload",
            PlaintextHolder::new(PlaintextBoundary::Worker, "another-worker"),
        ));
        assert_eq!(
            wrong_return
                .resolve_validated(&exact_access, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::ResolverMismatch,
            "S5"
        );
        let expired = access(&holder.trust_domain.0, 0, "sha256:sealed-payload");
        assert_eq!(
            exact
                .resolve_validated(&expired, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::EnvelopeExpired,
            "S6"
        );
        let wrong_recipient = access("another-worker", u64::MAX, "sha256:sealed-payload");
        assert_eq!(
            exact
                .resolve_validated(&wrong_recipient, &holder, binding.clone(), None)
                .await
                .unwrap_err(),
            CredentialMaterialError::RecipientMismatch,
            "S7"
        );
        let worker_access = CredentialAccess::new(
            exact_access.credential.clone(),
            CredentialMaterialSource::WorkerReference,
            exact_access.usage.clone(),
            exact_access.policy.clone(),
        )
        .with_target(exact_access.target.clone().expect("S8 exact target"));
        let worker_resolved = exact
            .resolve_validated(&worker_access, &holder, binding.clone(), None)
            .await
            .expect("S8 WorkerReference delegates through the same port");
        assert_eq!(
            worker_resolved
                .material
                .single_secret()
                .unwrap()
                .expose_secret(),
            "external-sealed-material"
        );
        let external_only = PinnedCredentialMaterializer::external_only(
            resolver(
                binding.clone(),
                "sha256:unused-for-worker-reference",
                holder.clone(),
            ),
            awaken_time::system_clock(),
        );
        assert!(!external_only.has_local_authority(), "S10");
        assert_eq!(
            external_only
                .resolve_validated(&worker_access, &holder, binding, None)
                .await
                .expect("S10 external-only resolver")
                .material
                .single_secret()
                .unwrap()
                .expose_secret(),
            "external-sealed-material",
            "S10"
        );

        let provider_access = CredentialAccess::new(
            exact_access.credential.clone(),
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::ProviderAdapter,
            exact_access.policy.clone(),
        )
        .with_target(awaken_agent_contract::credential::CredentialTarget::new(
            awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
            "anthropic",
        ))
        .with_envelope(CredentialEnvelope::SealedForWorker {
            envelope_ref: SealedCredentialEnvelopeRef {
                id: "sealed-provider".into(),
                payload_fingerprint: "sha256:provider-payload".into(),
            },
            recipient: holder.trust_domain.clone(),
            expires_at_unix_ms: u64::MAX,
        });
        let endpoint = InferenceEndpoint {
            adapter_kind: "anthropic".into(),
            api_dialect: "anthropic_messages".into(),
            base_url: "https://provider.example".into(),
            upstream_model: "model".into(),
            processing_placement: None,
        };
        let provider_ref = "anthropic@1";
        let provider_binding = CredentialMaterialBinding::for_target(
            "workspace-a",
            &(provider_ref, &endpoint),
            &provider_access.usage,
        );
        let provider_materializer = base.with_external_material_resolver(resolver(
            provider_binding,
            "sha256:provider-payload",
            holder,
        ));
        let candidate = ResolvedModelCandidate::try_provider(
            ModelBinding::new("anthropic", "model", "native"),
            provider_ref,
            "endpoint@1",
            "workspace-a",
            Some(provider_access),
            endpoint,
        )
        .expect("coherent external-material provider candidate");
        assert_eq!(
            provider_materializer
                .materialize_provider(&candidate)
                .await
                .expect("S9 inference uses the same exact resolver")
                .expose_secret(),
            "external-sealed-material",
            "S9"
        );
    }

    /// Adapter-admission cause graph: exact Provider target -> published source
    /// -> installed source capability -> material lookup. The decision table distinguishes a
    /// supported-but-missing Control reference from a Worker reference this
    /// adapter must reject before any lookup.
    ///
    /// | Rule | Control reference | Row exists | Result |
    /// |---|---|---|---|
    /// | A1 | T | F | material unavailable |
    /// | A2 | F | - | material source unsupported |
    #[tokio::test]
    async fn provider_adapter_capabilities_match_the_decision_table() {
        for (rule, source, expected) in [
            (
                "A1",
                CredentialMaterialSource::ControlPlaneReference,
                "credential material unavailable",
            ),
            (
                "A2",
                CredentialMaterialSource::WorkerReference,
                "credential material source is unsupported",
            ),
        ] {
            let candidate =
                ResolvedModelCandidate::try_provider(
                    ModelBinding::new("anthropic", "model", "native"),
                    "anthropic@1",
                    "endpoint@1",
                    "workspace-a",
                    Some(
                        CredentialAccess::new(
                            CredentialRef {
                                id: "missing".into(),
                                revision: 1,
                            },
                            source,
                            CredentialUsage::ProviderAdapter,
                            CredentialExecutionPolicy::self_hosted_provider(),
                        )
                        .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                            awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
                            "anthropic",
                        )),
                    ),
                    InferenceEndpoint {
                        adapter_kind: "anthropic".into(),
                        api_dialect: "anthropic_messages".into(),
                        base_url: "https://example.invalid".into(),
                        upstream_model: "model".into(),
                        processing_placement: None,
                    },
                )
                .expect("coherent unavailable-material provider candidate");
            let materializer = pinned_vault(
                Arc::new(InMemoryCredentialRepo::new()),
                Arc::new(InMemorySecretStore::new()),
                awaken_time::system_clock(),
            );
            assert_eq!(
                materializer
                    .materialize_provider(&candidate)
                    .await
                    .unwrap_err(),
                expected,
                "{rule}"
            );
        }
    }

    struct CountingSecrets {
        inner: InMemorySecretStore,
        reads: AtomicUsize,
    }

    impl CountingSecrets {
        fn new() -> Self {
            Self {
                inner: InMemorySecretStore::new(),
                reads: AtomicUsize::new(0),
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SecretStore for CountingSecrets {
        async fn put(
            &self,
            reference: &SecretRef,
            secret: RedactedString,
        ) -> Result<(), CredentialError> {
            self.inner.put(reference, secret).await
        }

        async fn get(&self, reference: &SecretRef) -> Result<RedactedString, CredentialError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.get(reference).await
        }

        async fn delete(&self, reference: &SecretRef) -> Result<(), CredentialError> {
            self.inner.delete(reference).await
        }
    }

    struct FixedAttempt(std::result::Result<(), awaken_runtime_contract::AttemptOwnershipError>);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::AttemptOwnershipVerifier for FixedAttempt {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            self.0.clone()
        }
    }

    #[async_trait::async_trait]
    impl awaken_credential_contract::CredentialRealizationRecorder for FixedAttempt {
        async fn record(
            &self,
            _receipt: awaken_credential_contract::CredentialRealizationReceipt,
        ) -> Result<(), awaken_credential_contract::CredentialRealizationRecordError> {
            Ok(())
        }
    }

    struct MutableAttempt(std::sync::atomic::AtomicBool);

    #[async_trait::async_trait]
    impl awaken_runtime_contract::AttemptOwnershipVerifier for MutableAttempt {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                Err(awaken_runtime_contract::AttemptOwnershipError::Lost)
            } else {
                Ok(())
            }
        }
    }

    struct LoseDuringRefresh {
        ownership: Arc<MutableAttempt>,
        delivery: awaken_credential_contract::AttemptCredentialDelivery,
    }

    #[async_trait::async_trait]
    impl awaken_credential_contract::AttemptCredentialDeliverySource for LoseDuringRefresh {
        async fn deliver(
            &self,
            _: awaken_credential_contract::AttemptCredentialDeliveryRequest,
        ) -> Result<awaken_credential_contract::AttemptCredentialDelivery, String> {
            self.ownership
                .0
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(self.delivery.clone())
        }
    }

    #[tokio::test]
    async fn claimed_remote_materialization_is_fenced_before_secret_read() {
        // Test design — Causes: C1 Remote publication and attempt binding are
        // exact; C2 the selected mechanism is WorkerRelay; C3 ownership is
        // absent, current, lost, or unavailable. Effects: E1 read and return the
        // exact bearer then record realization, or E2 reject without any new
        // SecretStore read. Constraints: claim compilation owns C1/C2; this
        // materializer requires a bound ownership even though the shared
        // external-effect gate permits unbound embedded/direct callers.
        // Decision rules: M1 C1+C2+current -> E1; M2 absent -> E2; M3 lost ->
        // E2; M4 unavailable -> E2; M5 ownership lost while a new carrier is
        // being issued -> recheck and E2. Exact binding/mechanism negative rules are
        // independently exhaustive in the shared compiler and receipt suites.
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(CountingSecrets::new());
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("https://agent.example".into()),
                env_key: None,
                secret: Some(RedactedString::new("remote-token")),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        let reads_before_materialization = secrets.reads();
        let holder = selected_holder();
        let candidate = ResolvedModelCandidate::try_remote(
            ModelBinding::new("", "", "a2a:https://agent.example"),
            "workspace-a",
            Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: source.id.0,
                        revision: 1,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    CredentialExecutionPolicy::self_hosted_provider(),
                )
                .with_target(awaken_agent_contract::credential::CredentialTarget::new(
                    awaken_agent_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
                    "https://agent.example",
                )),
            ),
            "sha256:card",
        )
        .expect("coherent remote credential candidate");
        let capabilities = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerRelay]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
            &[&candidate],
            Some(&holder),
            &capabilities,
            3,
            0,
        )
        .unwrap();

        let lost: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier> = Arc::new(
            FixedAttempt(Err(awaken_runtime_contract::AttemptOwnershipError::Lost)),
        );
        let unavailable: Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier> =
            Arc::new(FixedAttempt(Err(
                awaken_runtime_contract::AttemptOwnershipError::Unavailable(
                    "authority down".into(),
                ),
            )));
        let cases = [
            ("M2", None, "has no claim fence"),
            ("M3", Some(lost), "attempt ownership was lost"),
            ("M4", Some(unavailable), "authority down"),
        ];
        let materializer = pinned_vault(credentials, secrets.clone(), awaken_time::system_clock());
        for (rule, ownership, expected_error) in cases {
            let recorder = Arc::new(FixedAttempt(Ok(())));
            let mut context = awaken_runtime_contract::RuntimeRunContext::new()
                .with_credential_realization(
                    awaken_credential_contract::AttemptCredentialRealization::new(
                        bindings.clone(),
                        recorder,
                    ),
                );
            if let Some(ownership) = ownership {
                context = context.with_ownership(ownership);
            }
            let error = materializer
                .materialize_claimed_remote(&candidate, &context)
                .await
                .expect_err(rule);
            assert!(error.contains(expected_error), "{rule}: {error}");
            assert_eq!(secrets.reads(), reads_before_materialization, "{rule}/E2");
        }

        let authority = Arc::new(FixedAttempt(Ok(())));
        let context = awaken_runtime_contract::RuntimeRunContext::new()
            .with_ownership(authority.clone())
            .with_credential_realization(
                awaken_credential_contract::AttemptCredentialRealization::new(
                    bindings.clone(),
                    authority,
                ),
            );
        assert_eq!(
            materializer
                .materialize_claimed_remote(&candidate, &context)
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "remote-token",
            "M1/E1"
        );
        assert_eq!(secrets.reads(), reads_before_materialization + 1, "M1/E1");

        let ownership = Arc::new(MutableAttempt(std::sync::atomic::AtomicBool::new(false)));
        let (published, _) =
            awaken_credential_contract::candidate_credential_material_binding(&candidate).unwrap();
        let delivery = awaken_credential_contract::AttemptCredentialDelivery {
            candidate_fingerprint: bindings[0].candidate_fingerprint.clone(),
            access: awaken_credential_contract::credential_access_without_delivery(published)
                .with_envelope(
                    awaken_agent_contract::credential::CredentialEnvelope::SealedForWorker {
                        envelope_ref:
                            awaken_agent_contract::credential::SealedCredentialEnvelopeRef {
                                id: "refresh".into(),
                                payload_fingerprint: "refresh".into(),
                            },
                        recipient: holder.trust_domain.clone(),
                        expires_at_unix_ms: u64::MAX,
                    },
                ),
            claim_epoch: bindings[0].claim_epoch,
        };
        let realization =
            awaken_credential_contract::AttemptCredentialRealization::new_with_deliveries(
                bindings.clone(),
                Vec::new(),
                Arc::new(FixedAttempt(Ok(()))),
            )
            .unwrap()
            .with_delivery_refresh(Arc::new(LoseDuringRefresh {
                ownership: ownership.clone(),
                delivery,
            }));
        let context = awaken_runtime_contract::RuntimeRunContext::new()
            .with_ownership(ownership)
            .with_credential_realization(realization);
        let reads_before_loss = secrets.reads();
        let error = materializer
            .materialize_claimed_remote(&candidate, &context)
            .await
            .expect_err("M5 ownership must be rechecked after refresh");
        assert!(error.contains("attempt ownership was lost"), "M5: {error}");
        assert_eq!(secrets.reads(), reads_before_loss, "M5/E2");
    }

    struct SshExtensionConsumer {
        corrupt_receipt: bool,
    }

    #[async_trait::async_trait]
    impl awaken_credential_contract::CredentialExtensionConsumer for SshExtensionConsumer {
        fn descriptor(&self) -> awaken_credential_contract::CredentialExtensionDescriptor {
            awaken_credential_contract::CredentialExtensionDescriptor {
                consumer_id: "example.ssh-agent".into(),
                material_types: std::collections::BTreeSet::from([
                    "example.ssh-private-key/v1".into()
                ]),
            }
        }

        async fn consume(
            &self,
            request: awaken_credential_contract::CredentialExtensionRequest<'_>,
        ) -> Result<awaken_credential_contract::CredentialExtensionReceipt, CredentialMaterialError>
        {
            if request
                .material
                .fields
                .get("private_key")
                .map(RedactedString::expose_secret)
                != Some("ssh-secret")
            {
                return Err(CredentialMaterialError::Invalid);
            }
            Ok(awaken_credential_contract::CredentialExtensionReceipt {
                consumer_id: "example.ssh-agent".into(),
                material_type: "example.ssh-private-key/v1".into(),
                target_use_fingerprint: if self.corrupt_receipt {
                    "wrong-target".into()
                } else {
                    request.binding.target_use_fingerprint.clone()
                },
            })
        }
    }

    async fn extension_fixture(
        corrupt_receipt: bool,
    ) -> (
        PinnedCredentialMaterializer,
        CredentialAccess,
        PlaintextHolder,
    ) {
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let material = awaken_credential_vault::StructuredCredentialMaterial {
            type_id: "example.ssh-private-key/v1".into(),
            fields: std::collections::BTreeMap::from([(
                "private_key".into(),
                RedactedString::new("ssh-secret"),
            )]),
        };
        let material = awaken_credential_vault::encode_structured_material(material).unwrap();
        let usage = CredentialUsage::Extension {
            consumer_id: "example.ssh-agent".into(),
            material_type: "example.ssh-private-key/v1".into(),
            public_config: serde_json::json!({"socket": "agent.sock"}),
        };
        let target = awaken_agent_contract::credential::CredentialTarget::new(
            awaken_agent_contract::credential::CredentialPurpose::Extension,
            "extension://example.ssh-agent",
        );
        let mut source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("example.ssh-agent".into()),
                env_key: None,
                secret: Some(material),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        source.provider_id = None;
        source.descriptor = Some(awaken_credential_contract::CredentialDescriptor::new(
            "example.ssh-agent",
            awaken_credential_contract::CredentialMaterialDescriptor::structured(
                "example.ssh-private-key/v1",
                ["private_key"],
            ),
            [awaken_credential_contract::CredentialTargetContract::new(
                target.clone(),
                usage.clone(),
            )],
        ));
        credentials.put(source.clone()).await.unwrap();
        let holder = PlaintextHolder::new(
            PlaintextBoundary::Worker,
            awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        );
        let access = CredentialAccess::new(
            CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version).unwrap(),
            },
            CredentialMaterialSource::ControlPlaneReference,
            usage,
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        )
        .with_target(target);
        let materializer = pinned_vault(credentials, secrets, awaken_time::system_clock())
            .with_extension_consumer(Arc::new(SshExtensionConsumer { corrupt_receipt }))
            .unwrap();
        (materializer, access, holder)
    }

    #[tokio::test]
    async fn extension_registry_drives_discovery_resolution_consumption_and_receipt() {
        // Cause/effect decision rule X1: installed descriptor + matching Extension
        // usage + exact active revision/workspace/holder + matching typed material
        // => advertise the same descriptor, dispatch once without returning
        // plaintext, and accept only an exact target-bound receipt.
        let (materializer, access, holder) = extension_fixture(false).await;
        assert_eq!(
            materializer
                .worker_relay_capabilities()
                .extension_consumers
                .get("example.ssh-agent"),
            Some(&std::collections::BTreeSet::from([
                "example.ssh-private-key/v1".into()
            ]))
        );

        let receipt = materializer
            .consume_extension_for_workspace(
                &access,
                &holder,
                "workspace-a",
                &("repository-a", "git.example"),
            )
            .await
            .unwrap();
        assert_eq!(receipt.consumer_id, "example.ssh-agent");
        assert_eq!(receipt.material_type, "example.ssh-private-key/v1");
        assert!(!receipt.target_use_fingerprint.is_empty());
    }

    #[tokio::test]
    async fn extension_registry_rejects_duplicates_and_forged_receipts() {
        // Decision rules X2/X3: duplicate consumer ids are composition conflicts;
        // a consumer receipt for a different target is a resolver mismatch. In
        // both cases no second dispatch authority is synthesized.
        let duplicate = pinned_vault(
            Arc::new(InMemoryCredentialRepo::new()),
            Arc::new(InMemorySecretStore::new()),
            awaken_time::system_clock(),
        )
        .with_extension_consumer(Arc::new(SshExtensionConsumer {
            corrupt_receipt: false,
        }))
        .unwrap()
        .with_extension_consumer(Arc::new(SshExtensionConsumer {
            corrupt_receipt: false,
        }));
        assert!(matches!(
            duplicate,
            Err(CredentialExtensionRegistryError::DuplicateConsumer(id))
                if id == "example.ssh-agent"
        ));

        let (materializer, access, holder) = extension_fixture(true).await;
        assert_eq!(
            materializer
                .consume_extension_for_workspace(&access, &holder, "workspace-a", &"target")
                .await,
            Err(CredentialMaterialError::ResolverMismatch)
        );
    }
}
