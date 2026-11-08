// @formal evidence session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal evidence worker.realization_scope_uses_only_the_kubernetes_namespace_or_persisted_local_workspace
use awaken_credential_contract::CredentialAccessAdmission;
use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;

use super::{
    CredentialMaterializerSupport, InferenceExecutorMaterializer, ResourceManifestSupport,
    StandardManifestConfig, StandardManifestInputs, WorkerApplicationSource, WorkerNodeBuilder,
    configured_container_acp_targets, derive_standard_manifest, grace_window,
    validate_worker_manifest, worker_realization_namespace,
};

#[tokio::test]
async fn preparation_drain_requires_an_actual_return_before_shutdown() {
    // Causes: C1 preparation task returned, C2 its budget expired before
    // return, C3 the task was cancelled. Effect E1 only C1 permits ordinary
    // shutdown; E2 C2/C3 require exact-owner revocation, never an invented
    // physical ACK. Rules D1=C1=>E1, D2=C2=>E2, D3=C3=>E2.
    let mut finished = tokio::spawn(async {});
    assert!(
        super::preparation_drained_before(&mut finished, std::time::Duration::from_secs(1)).await,
        "D1/E1"
    );
    let mut pending = tokio::spawn(std::future::pending::<()>());
    assert!(
        !super::preparation_drained_before(&mut pending, std::time::Duration::ZERO).await,
        "D2/E2"
    );
    assert!(
        !pending.is_finished(),
        "D2 timeout cannot cancel or acknowledge the effect"
    );
    pending.abort();
    let mut cancelled = tokio::spawn(std::future::pending::<()>());
    cancelled.abort();
    assert!(
        !super::preparation_drained_before(&mut cancelled, std::time::Duration::from_secs(1)).await,
        "D3/E2"
    );
}

#[test]
fn trusted_launcher_incarnation_selection_is_explicit_and_validated() {
    // Causes: C1 a launcher supplies an id; C2 it has the canonical local
    // process format. Effects: E1 absent keeps the existing random-on-run
    // behavior; E2 valid input freezes that exact id; E3 malformed input fails
    // before Worker registration or Sandbox effects.
    // | Rule | C1 | C2 | Effect |
    // | B1   | N  | -  | E1     |
    // | B2   | Y  | Y  | E2     |
    // | B3   | Y  | N  | E3     |
    let builder = || {
        WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("http://127.0.0.1:1"),
            awaken_time::system_clock(),
        )
    };
    assert!(builder().preselected_incarnation_id.is_none(), "B1/E1");
    let selected = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        builder()
            .with_preselected_incarnation_id(selected)
            .expect("B2/E2")
            .preselected_incarnation_id
            .as_deref(),
        Some(selected)
    );
    assert!(
        builder().with_preselected_incarnation_id(" ").is_err(),
        "B3/E3"
    );
}

#[test]
fn container_realization_namespace_is_installation_wide() {
    // The same public projection now supplies downstream decorated providers;
    // no new decision is introduced. The external consumer's compile check
    // owns export availability; this table retains every identity/refusal row.
    // Cause/effect decision table. Causes: C1 backend is Kubernetes/host-local;
    // C2 physical Kubernetes namespace or persisted local Workspace is
    // equal/different; C3 the replacement has a different Worker/process
    // incarnation. Effects: E1 equal C2 preserves one physical namespace across
    // C3; E2 different C2 cannot discover another installation's substrate; E3
    // Kubernetes never substitutes a per-Pod local Workspace for its existing
    // namespace authority; E4 a missing local storage root cannot block K8s.
    //
    // | Rule | C1 | C2 | Effect |
    // | R1 | Kubernetes | equal namespace | E1+E3+E4 same realization namespace |
    // | R2 | Kubernetes | different namespace | E2 different realization namespace |
    // | R3 | host-local | equal/different Workspace | E1/E2 respectively |
    //
    // C3 is deliberately absent from the projector's input type: registry
    // leases and Sandbox effect fences, not object identity, fence incarnations.
    let kubernetes = |namespace: &str, local_root: &std::path::Path| {
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = awaken_deployment_config::SandboxTier::K8s;
        deployment.sandbox.k8s_namespace = namespace.into();
        deployment.storage_dir = Some(local_root.into());
        worker_realization_namespace(&deployment).expect("Kubernetes realization namespace")
    };
    let first_root = tempfile::tempdir().expect("first Worker root");
    let replacement_root = tempfile::tempdir().expect("replacement Worker root");
    let worker_a = kubernetes("installation-a", first_root.path());
    let replacement = kubernetes("installation-a", replacement_root.path());
    let other_installation = kubernetes("installation-b", first_root.path());
    let mut markerless_kubernetes = awaken_deployment_config::DeploymentConfig::ephemeral();
    markerless_kubernetes.sandbox_tier = awaken_deployment_config::SandboxTier::K8s;
    markerless_kubernetes.sandbox.k8s_namespace = "installation-a".into();
    markerless_kubernetes.storage_dir = None;
    let markerless = worker_realization_namespace(&markerless_kubernetes)
        .expect("Kubernetes does not require a local Workspace marker");
    assert_eq!(worker_a, replacement, "R1/E1-E3");
    assert_eq!(worker_a, markerless, "R1/E4");
    assert_ne!(worker_a, other_installation, "R2/E2");

    let local = |root: &std::path::Path| {
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Docker;
        deployment.storage_dir = Some(root.into());
        worker_realization_namespace(&deployment).expect("local realization namespace")
    };
    let local_a = tempfile::tempdir().expect("local installation A");
    let local_b = tempfile::tempdir().expect("local installation B");
    assert_eq!(local(local_a.path()), local(local_a.path()), "R3/E1");
    assert_ne!(local(local_a.path()), local(local_b.path()), "R3/E2");
}

struct SchemeMaterializer;

struct ExternalCredentialResolver;

pub(crate) struct UnusedContainerProvider;

struct CheckpointContainerProvider;

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxReadiness for CheckpointContainerProvider {
    fn capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
        Default::default()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        vec!["hosted-csi-v1".into()]
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxProvider for CheckpointContainerProvider {
    type Environment = dyn awaken_provisioning_contract::SessionSandbox;

    async fn prewarm_images(
        &self,
        _specs: &[awaken_provisioning_contract::SandboxSpec],
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_provisioning_contract::SandboxAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn validate_closed_observation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
        observation: &awaken_provisioning_contract::SandboxObservation,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::validate_container_closed_observation(handle, observation)
    }

    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a awaken_provisioning_contract::SandboxSpec,
        effect: awaken_provisioning_contract::SandboxEffect<'a>,
        intent: awaken_provisioning_contract::SandboxRealizationIntent,
    ) -> awaken_provisioning_contract::ClaimedEffect<
        'a,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        let claim = awaken_provisioning_contract::EffectClaim::in_process(
            awaken_provisioning_contract::test_support::retain_abandoned_effect(spec.scope.clone()),
        );
        awaken_provisioning_contract::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            match effect {
                awaken_provisioning_contract::SandboxEffect::Unattached => {
                    let _spec = spec;

                    unreachable!("builder tests do not create an environment")
                }
                awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                    let _spec = spec;
                    let _effect_fence = fence;
                    let _authority = authority;
                    let _intent = intent;

                    Err(awaken_provisioning_contract::SandboxError::unsupported(
                        "test double does not implement fenced Environment creation",
                    ))
                }
            }
        })
    }

    async fn observe(
        &self,
        _adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }

    async fn adopt_for_effect(
        &self,
        adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        effect: awaken_provisioning_contract::SandboxEffect<'_>,
    ) -> Result<
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        awaken_provisioning_contract::SandboxError,
    > {
        match effect {
            awaken_provisioning_contract::SandboxEffect::Unattached => {
                let _adoption = adoption;

                unreachable!("builder tests do not adopt an environment")
            }
            awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                let _adoption = adoption;
                let _effect_fence = fence;
                let _authority = authority;

                Err(awaken_provisioning_contract::SandboxError::unsupported(
                    "test double does not implement fenced Environment adoption",
                ))
            }
        }
    }

    async fn prepare_terminal_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn restoration(
        &self,
    ) -> awaken_provisioning_contract::SandboxRestoration<
        '_,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        awaken_provisioning_contract::SandboxRestoration::Targets(self)
    }
}

impl awaken_provisioning_contract::SandboxInstallation for CheckpointContainerProvider {
    fn install_memory_mounter(
        &self,
        _mounter: std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
    }

    fn install_secret_broker(
        &self,
        _broker: std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) {
    }
}

#[async_trait::async_trait]
impl
    awaken_provisioning_contract::RestoreTargetProvider<
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > for CheckpointContainerProvider
{
    async fn acquire_restore(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _request: &awaken_provisioning_contract::SandboxRestoreRequest,
    ) -> Result<
        awaken_provisioning_contract::SandboxRestoreTarget<
            std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        >,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "builder tests do not restore an environment",
        ))
    }

    async fn dispose_restored(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _request: &awaken_provisioning_contract::SandboxRestoreRequest,
        _authorization: &awaken_provisioning_contract::SandboxDisposalAuthorization,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "builder tests do not dispose restored environments",
        ))
    }
}

struct UnusedCheckpointStore;

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxCheckpointStore for UnusedCheckpointStore {
    async fn put(
        &self,
        _metadata: &awaken_provisioning_contract::CheckpointObjectMetadata,
        _bytes: Vec<u8>,
    ) -> Result<
        awaken_provisioning_contract::StoredCheckpointObject,
        awaken_provisioning_contract::SandboxError,
    > {
        unreachable!("builder tests do not write checkpoint bytes")
    }

    async fn get(&self, _id: &str) -> Result<Vec<u8>, awaken_provisioning_contract::SandboxError> {
        unreachable!("builder tests do not read checkpoint bytes")
    }

    async fn delete(&self, _id: &str) -> Result<(), awaken_provisioning_contract::SandboxError> {
        unreachable!("builder tests do not delete checkpoint bytes")
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxReadiness for UnusedContainerProvider {
    fn capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
        Default::default()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_provisioning_contract::SandboxProvider for UnusedContainerProvider {
    type Environment = dyn awaken_provisioning_contract::SessionSandbox;

    async fn prewarm_images(
        &self,
        _specs: &[awaken_provisioning_contract::SandboxSpec],
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_provisioning_contract::SandboxAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn validate_closed_observation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
        observation: &awaken_provisioning_contract::SandboxObservation,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::validate_container_closed_observation(handle, observation)
    }

    fn claim_for_effect<'a>(
        &'a self,
        spec: &'a awaken_provisioning_contract::SandboxSpec,
        effect: awaken_provisioning_contract::SandboxEffect<'a>,
        intent: awaken_provisioning_contract::SandboxRealizationIntent,
    ) -> awaken_provisioning_contract::ClaimedEffect<
        'a,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        let claim = awaken_provisioning_contract::EffectClaim::in_process(
            awaken_provisioning_contract::test_support::retain_abandoned_effect(spec.scope.clone()),
        );
        awaken_provisioning_contract::ClaimedEffect::new(claim, async move {
            intent.admit_under(effect)?;
            match effect {
                awaken_provisioning_contract::SandboxEffect::Unattached => {
                    let _spec = spec;

                    Err(awaken_provisioning_contract::SandboxError::unsupported(
                        "unused test provider",
                    ))
                }
                awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                    let _spec = spec;
                    let _effect_fence = fence;
                    let _authority = authority;
                    let _intent = intent;

                    Err(awaken_provisioning_contract::SandboxError::unsupported(
                        "test double does not implement fenced Environment creation",
                    ))
                }
            }
        })
    }

    async fn observe(
        &self,
        _adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }

    async fn adopt_for_effect(
        &self,
        adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
        effect: awaken_provisioning_contract::SandboxEffect<'_>,
    ) -> Result<
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        awaken_provisioning_contract::SandboxError,
    > {
        match effect {
            awaken_provisioning_contract::SandboxEffect::Unattached => {
                let _adoption = adoption;

                Err(awaken_provisioning_contract::SandboxError::unsupported(
                    "unused test provider",
                ))
            }
            awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                let _adoption = adoption;
                let _effect_fence = fence;
                let _authority = authority;

                Err(awaken_provisioning_contract::SandboxError::unsupported(
                    "test double does not implement fenced Environment adoption",
                ))
            }
        }
    }

    async fn prepare_terminal_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
        _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    fn restoration(
        &self,
    ) -> awaken_provisioning_contract::SandboxRestoration<
        '_,
        std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    > {
        awaken_provisioning_contract::SandboxRestoration::Unsupported
    }
}

impl awaken_provisioning_contract::SandboxInstallation for UnusedContainerProvider {
    fn install_memory_mounter(
        &self,
        _mounter: std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>,
    ) {
    }

    fn install_secret_broker(
        &self,
        _broker: std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>,
    ) {
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialMaterialResolver for ExternalCredentialResolver {
    fn material_capabilities(
        &self,
    ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability> {
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
        _request: awaken_credential_contract::CredentialMaterialRequest<'_>,
    ) -> Result<
        awaken_credential_contract::ResolvedCredentialMaterial,
        awaken_credential_contract::CredentialMaterialError,
    > {
        Err(awaken_credential_contract::CredentialMaterialError::Unavailable)
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialObservationSource for ExternalCredentialResolver {
    async fn credential_observations(
        &self,
    ) -> Result<
        std::collections::BTreeSet<awaken_credential_contract::CredentialObservation>,
        awaken_credential_contract::CredentialMaterialError,
    > {
        Ok(std::collections::BTreeSet::from([
            awaken_credential_contract::CredentialObservation::available(
                awaken_agent_contract::credential::CredentialRef {
                    id: "cred:worker".into(),
                    revision: 4,
                },
                1,
            ),
        ]))
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::WorkerLocalReferenceRevalidator for ExternalCredentialResolver {
    async fn revalidate_worker_reference(
        &self,
        credential: &awaken_agent_contract::credential::CredentialRef,
    ) -> Result<
        awaken_credential_contract::CredentialObservation,
        awaken_credential_contract::CredentialMaterialError,
    > {
        Ok(awaken_credential_contract::CredentialObservation::available(credential.clone(), 1))
    }
}

impl InferenceExecutorMaterializer for SchemeMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &["test-access/v1"]
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        awaken_credential_contract::CredentialRealizationCapabilities {
            holders: [awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources: [
                awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes: false,
            envelope_required: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        }
    }

    fn materialize_pinned(
        &self,
        _candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        None
    }
}

fn deployment() -> awaken_deployment_config::DeploymentConfig {
    awaken_deployment_config::DeploymentConfig::ephemeral()
}

fn admits_native_source(
    evidence: &awaken_credential_contract::CredentialRealizationCapabilities,
    source: awaken_agent_contract::credential::CredentialMaterialSource,
) -> bool {
    use awaken_agent_contract::credential::{
        CredentialAccess, CredentialExecutionPolicy, CredentialRef, CredentialTarget,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_credential_contract::CredentialRealizationKind;
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
    );
    CredentialAccess::new(
        CredentialRef {
            id: "source".into(),
            revision: 1,
        },
        source,
        CredentialUsage::ProviderAdapter,
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
    )
    .with_target(CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
        "provider",
    ))
    .admit(
        &holder,
        CredentialRealizationKind::WorkerProviderAdapter,
        evidence,
        1,
    )
    .is_ok()
}

fn credential_support() -> CredentialMaterializerSupport {
    CredentialMaterializerSupport::from(
        &awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
            Arc::new(ExternalCredentialResolver),
            awaken_time::system_clock(),
        ),
    )
}

#[test]
fn worker_uses_the_injected_web_search_registry_as_its_only_catalog() {
    // Cause-effect graph / decision table:
    // R1 no injection -> open built-ins; R2 one injected deployment registry ->
    // exactly that catalog (the empty catalog is the minimal observable
    // replacement). This proves Cloud does not append a second provider path
    // beside the Worker's default registry.
    let registry = awaken_ext_builtin_tools::WebSearchProviderRegistry::default();
    let worker = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_standard_manifest(Default::default())
    .with_web_search_provider_registry(registry)
    .build()
    .unwrap();
    let WorkerApplicationSource::Static { dependencies, .. } = worker.application else {
        panic!("static application source");
    };
    let descriptors = dependencies.web_providers.descriptors();
    assert!(descriptors.is_empty());
}

#[test]
fn worker_manifest_derives_materialization_capabilities_from_the_adapter() {
    // Cause/effect rule M1: the standard derivation receives only installed
    // inference evidence, so it publishes Native + that adapter and cannot
    // invent the independently installed A2A capability.
    let materializer = SchemeMaterializer;
    let deployment = deployment();
    let manifest = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &materializer
            .supported_access_schemes()
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        inference_credentials: &materializer.credential_realization_capabilities(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });

    assert!(manifest.capabilities.contains("native-runtime"));
    assert!(
        manifest
            .capabilities
            .contains(awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY),
        "M1 compiled Claimed.request decoder is advertised independently of execution backend"
    );
    // J-C1: this standard composition includes the actual raw-result codec,
    // therefore its immutable manifest must advertise exactly that capability.
    // A B-only/old Worker omitting it is covered by Dispatch's hard subset and
    // intrinsic-omission cases; Native presence alone must not imply support.
    assert!(
        manifest
            .capabilities
            .contains(awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY),
    );
    // C1: the standard composition installs the registered Task control, so
    // its manifest advertises the Task control codec.
    assert!(
        manifest
            .capabilities
            .contains(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY),
    );
    assert_eq!(
        manifest.sandbox_tool_recovery,
        deployment
            .sandbox
            .container_hand_residency
            .recovery_capability(),
        "the standard manifest and SessionEnvironment share one typed source"
    );
    assert!(
        !manifest
            .capabilities
            .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY)
    );
    assert!(manifest.capabilities.contains("test-access/v1"));
    let realization =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &manifest.capabilities,
        )
        .expect("credential realization capability decodes");
    assert!(realization.holders.contains(
        &awaken_agent_contract::credential::PlaintextHolder::new(
            awaken_agent_contract::credential::PlaintextBoundary::Worker,
            awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        )
    ));
    assert!(
        realization.realization_kinds.contains(
            &awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter
        )
    );
}

#[test]
fn worker_manifest_reports_trusted_host_only_from_operator_configuration() {
    // Cause/effect graph: C1 primary tier={local,namespace}; C2 explicit
    // trusted-host switch={false,true}. Effects: E1 the primary backend is
    // always reported; E2 trusted-host is reported for the explicitly local
    // tier or explicit switch, never by default on an isolated Worker.
    // Rules: local/F=>E1+E2, namespace/F=>E1 only, namespace/T=>E1+E2.
    for (tier, enabled, expected_trusted) in [
        (awaken_deployment_config::SandboxTier::Local, false, true),
        (
            awaken_deployment_config::SandboxTier::Namespace,
            false,
            false,
        ),
        (awaken_deployment_config::SandboxTier::Namespace, true, true),
    ] {
        let mut deployment = deployment();
        deployment.sandbox_tier = tier;
        deployment.sandbox.trusted_host_enabled = enabled;
        let primary_backend = deployment.sandbox_support().1.to_string();
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            inference_schemes: &Default::default(),
            inference_credentials: &Default::default(),
            credential_materializer: None,
            brokered_acp_credentials: &Default::default(),
            worker_local_credential_resolver_installed: false,
            remote_credential_realization: None,
            sandbox_override: None,
            checkpoint_formats: Default::default(),
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert!(manifest.sandbox_backends.contains(&primary_backend), "E1");
        // E3 the additional profile comes from the installed Workdir provider,
        // not from the primary tier. In particular Namespace's hard path and
        // network guarantees must remain absent on TrustedHost.
        assert_eq!(
            manifest.trusted_host_sandbox,
            expected_trusted.then(awaken_provisioning_contract::SandboxCapabilities::default)
        );
        assert_eq!(
            manifest
                .sandbox_backends
                .contains(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND),
            expected_trusted,
            "tier={tier:?} enabled={enabled}"
        );
    }
}

/// Fallback claim decision table. Causes: C1 a Namespace launcher is missing;
/// C2 the operator explicitly permits Local fallback; C3 the Worker has run its
/// pre-registration provider preparation; C4 the manifest is standard or an
/// explicit stale Namespace claim. Effects: C1+C2+C3 publishes Local's exact
/// capability/backend while retaining the configured Namespace's lack of
/// TrustedHost consent; C1+C2+!C3 refuses build before registration; an
/// explicit Namespace claim is refused. The participant runs in a child with
/// an empty PATH so it cannot race this test process's ambient environment.
#[tokio::test]
async fn namespace_fallback_manifest_uses_the_probed_provider() {
    let binary = std::env::current_exe().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let child = tokio::process::Command::new(binary)
        .args([
            "--ignored",
            "--exact",
            "tests::namespace_fallback_manifest_participant",
        ])
        .env("PATH", empty_path.path())
        .env("AWAKEN_WORKER_FALLBACK_PARTICIPANT", "1")
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(std::time::Duration::from_secs(15), child)
        .await
        .expect("fallback participant stayed bounded")
        .unwrap();
    assert!(
        output.status.success(),
        "fallback participant: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("test tests::namespace_fallback_manifest_participant ... ok"),
        "the isolated participant must actually execute: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[tokio::test]
#[ignore = "isolated PATH participant invoked by namespace_fallback_manifest_uses_the_probed_provider"]
async fn namespace_fallback_manifest_participant() {
    assert_eq!(
        std::env::var("AWAKEN_WORKER_FALLBACK_PARTICIPANT").as_deref(),
        Ok("1")
    );
    let mut deployment = deployment();
    deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Namespace;
    deployment.sandbox.allow_local_fallback = true;
    deployment.sandbox.trusted_host_enabled = false;
    let make = || {
        WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("http://control"),
            awaken_time::system_clock(),
        )
        .with_deployment_config(deployment.clone(), false)
    };

    assert!(
        make()
            .with_standard_manifest(Default::default())
            .build()
            .err()
            .unwrap()
            .to_string()
            .contains("requires preparing"),
        "F2 no probe cannot advertise Namespace"
    );

    let worker = make()
        .with_standard_manifest(Default::default())
        .prepare_session_environment_from_deployment()
        .await
        .unwrap()
        .build()
        .unwrap();
    let manifest = worker.manifest();
    let (local, backend) =
        deployment.sandbox_support_for_tier(awaken_deployment_config::SandboxTier::Local);
    assert_eq!(manifest.sandbox, local, "F1 actual Workdir capabilities");
    assert_eq!(
        manifest.sandbox_backends,
        std::collections::BTreeSet::from([backend.to_string()])
    );
    assert!(
        manifest.trusted_host_sandbox.is_none(),
        "F1 fallback is not host consent"
    );
    assert_eq!(
        worker.deployment.sandbox_tier,
        awaken_deployment_config::SandboxTier::Namespace
    );

    let stale = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        make()
            .with_manifest(stale)
            .prepare_session_environment_from_deployment()
            .await
            .unwrap()
            .build()
            .err()
            .unwrap()
            .to_string()
            .contains("must match the prepared effective"),
        "F3 explicit stale Namespace claim refused"
    );
}

#[test]
fn standard_builder_derives_from_its_installed_materializer() {
    let worker = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("http://control"),
        awaken_time::system_clock(),
    )
    .with_inference_materializer(Arc::new(SchemeMaterializer))
    .with_standard_manifest(Default::default())
    .build()
    .expect("installed standard topology is valid");

    assert!(worker.manifest().capabilities.contains("test-access/v1"));
}

#[tokio::test]
async fn container_preparation_preserves_existing_provider_and_refuses_foreign_create_owner() {
    // Causes: C1=non-container tier; C2=already selected provider;
    // C3=caller supplies a foreign Kubernetes Create transport. Effects:
    // R1 C1 -> no container construction/decorator; R2 C2 -> preserve the
    // exact provider without rebuilding/decorating it; R3 C3 -> refuse before
    // provider I/O. The existing Create-client tests own registration binding
    // and HTTP effects; this table prevents a product from bypassing that owner.
    for selected in [false, true] {
        let mut configuration = deployment();
        configuration.sandbox_tier = if selected {
            awaken_deployment_config::SandboxTier::Docker
        } else {
            awaken_deployment_config::SandboxTier::Local
        };
        let mut builder = WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("http://control"),
            awaken_time::system_clock(),
        )
        .with_deployment_config(configuration, false);
        let provider: Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        > = Arc::new(UnusedContainerProvider);
        if selected {
            builder = builder.with_session_container_provider("selected", provider.clone());
        }
        let builder = builder
            .prepare_session_environment_from_deployment_with_policy(Default::default(), |_| {
                panic!("R1/R2 must not construct or decorate another provider")
            })
            .await
            .unwrap();
        assert_eq!(builder.session_container_provider.is_some(), selected);
        if let Some(installed) = builder.session_container_provider {
            assert!(Arc::ptr_eq(&installed.provider, &provider), "R2");
        }
    }
    #[cfg(feature = "container-k8s")]
    {
        let upstream = awaken_worker_transport_security::WorkerUpstream::new("http://control");
        let policy = awaken_execution_composition::ContainerTierPolicy {
            k8s_create_transport: Some(Arc::new(super::WorkerK8sCreateClient::new(
                upstream.clone(),
            ))),
            ..Default::default()
        };
        let result = WorkerNodeBuilder::new(upstream, awaken_time::system_clock())
            .prepare_session_environment_from_deployment_with_policy(policy, |_| {
                panic!("R3 must refuse before provider effects")
            })
            .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("owns its Kubernetes Create")
        );
    }
}

#[test]
fn worker_builder_rejects_scoped_execution_without_platform_or_host_consent() {
    // W1 typed builder bypasses TOML but must obey the same admission rule.
    // W2 disabled/valid selection never attests a full supervision guarantee.
    for scoped in [false, true] {
        for trusted in [false, true] {
            let mut deployment = deployment();
            deployment.sandbox_tier = awaken_deployment_config::SandboxTier::Namespace;
            deployment.sandbox.trusted_host_enabled = trusted;
            deployment.sandbox.trusted_host_scoped_execution = scoped;
            let result = WorkerNodeBuilder::new(
                awaken_worker_transport_security::WorkerUpstream::new("http://control"),
                awaken_time::system_clock(),
            )
            .with_deployment_config(deployment, false)
            .with_standard_manifest(Default::default())
            .build();
            let allowed = !scoped || (cfg!(target_os = "linux") && trusted);
            assert_eq!(result.is_ok(), allowed);
            if let Ok(worker) = result {
                assert!(!worker.manifest().sandbox.process_tree_supervision);
            }
        }
    }
}

#[test]
fn manifest_build_and_placement_share_current_protocol_admission() {
    use awaken_worker_contract::{
        CURRENT_CONTRACT_VERSION, CURRENT_RUNTIME_PROTOCOL_VERSION, VersionRange, WorkerManifest,
    };
    // Causes C1 explicit runtime range contains the current whole-claim floor;
    // C2 dispatch range contains the envelope version. R1 C1+C2 => build;
    // R2 !C1 or !C2 => reject before registration. Cross both envelope cases
    // with omitted, old stage-scoped, exact current, broad, future and inverted
    // declarations. In particular v4-only is sufficient: requiring v1 is a
    // second, contradictory policy rather than backward compatibility.
    for range in [
        VersionRange::ANY,
        VersionRange { min: 1, max: 3 },
        VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION),
        VersionRange {
            min: 1,
            max: CURRENT_RUNTIME_PROTOCOL_VERSION,
        },
        VersionRange::exact(CURRENT_RUNTIME_PROTOCOL_VERSION + 1),
        VersionRange {
            min: CURRENT_RUNTIME_PROTOCOL_VERSION,
            max: 3,
        },
    ] {
        for envelope in [CURRENT_CONTRACT_VERSION, CURRENT_CONTRACT_VERSION + 1] {
            let manifest = WorkerManifest {
                build_digest: "protocol-fixture".into(),
                dispatch_contract: VersionRange::exact(envelope),
                runtime_protocol: range,
                ..Default::default()
            };
            assert_eq!(
                validate_worker_manifest(&manifest, &deployment()).is_ok(),
                envelope == CURRENT_CONTRACT_VERSION
                    && manifest.supports_current_runtime_protocol(),
                "{range:?}/envelope={envelope}"
            );
        }
    }
}

#[test]
fn explicit_manifest_cannot_overstate_session_environment_recovery() {
    // Cause/effect decision table: C1=the installed default deployment uses
    // AttachedExec/NonRecoverable; C2=an explicit manifest claims
    // DurableRequest. R1 C1+!C2 is admitted by the existing manifest tests; R2
    // C1+C2 is rejected before registration. Explicit JSON therefore cannot
    // bypass the typed topology used by the actual SessionEnvironment.
    let mut manifest = awaken_worker_contract::WorkerManifest {
        build_digest: "test-worker".into(),
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        ..Default::default()
    };
    manifest.sandbox_tool_recovery =
        awaken_tool_contract::tool::ToolRecoveryCapability::DurableRequest;
    let error = validate_worker_manifest(&manifest, &deployment()).expect_err("R2 fail closed");
    assert!(
        error
            .to_string()
            .contains("does not match installed SessionEnvironment"),
        "R2: {error}"
    );
}

#[test]
fn explicit_manifest_cannot_promote_a_kernel_probe_to_installed_supervision() {
    // WI1 primary-only, WI2 additional-host-only, WI3 both supervision claims
    // must fail while the complete Session lifecycle is not installed.
    // WI4 neither claim preserves legacy admission. A manually supplied
    // manifest cannot bypass the same truthful standard-manifest boundary.
    for primary in [false, true] {
        for trusted in [false, true] {
            let mut manifest = awaken_worker_contract::WorkerManifest {
                build_digest: "supervision-fixture".into(),
                runtime_protocol: awaken_worker_contract::VersionRange::exact(
                    awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                ),
                ..Default::default()
            };
            manifest.sandbox.process_tree_supervision = primary;
            manifest.trusted_host_sandbox =
                trusted.then(|| awaken_provisioning_contract::SandboxCapabilities {
                    process_tree_supervision: true,
                    ..Default::default()
                });
            assert_eq!(
                validate_worker_manifest(&manifest, &deployment()).is_ok(),
                !primary && !trusted
            );
        }
    }
    // WI5 a claimed Workdir profile must also match all other installed axes.
    // A caller cannot smuggle network enforcement under the new profile.
    let mut configured = deployment();
    configured.sandbox.trusted_host_enabled = true;
    let mut manifest = awaken_worker_contract::WorkerManifest {
        build_digest: "profile-fixture".into(),
        runtime_protocol: awaken_worker_contract::VersionRange::exact(
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        ),
        trusted_host_sandbox: configured.trusted_host_support(),
        sandbox_backends: [awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND.into()].into(),
        ..Default::default()
    };
    assert!(validate_worker_manifest(&manifest, &configured).is_ok());
    manifest
        .trusted_host_sandbox
        .as_mut()
        .unwrap()
        .network_isolation = true;
    assert!(validate_worker_manifest(&manifest, &configured).is_err());
}

#[test]
fn checkpoint_provider_store_and_manifest_are_one_fail_closed_topology() {
    // Cause/effect design: C1=provider advertises hosted-csi-v1, C2=byte store
    // installed, C3=provider arrives as the canonical deployment components.
    // R1 C1+!C2 => reject before registration; R2 C1+C2 => exact format appears
    // in the existing WorkerManifest authority; R3 C1+C2+C3 => the decoration
    // seam has identical admission semantics and creates no second capability.
    let base = || {
        WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
            awaken_time::system_clock(),
        )
        .with_session_container_provider("hosted", Arc::new(CheckpointContainerProvider))
        .with_hand_executor_factory(awaken_tool_relay::relay_hand_executor_factory())
        .with_standard_manifest(Default::default())
    };
    assert_eq!(
        base()
            .build()
            .err()
            .expect("R1 must fail closed")
            .to_string(),
        "checkpoint-capable Session provider requires a checkpoint store",
        "R1"
    );
    let worker = base()
        .with_environment_checkpoint_store(Arc::new(UnusedCheckpointStore))
        .build()
        .expect("R2");
    assert!(
        worker
            .manifest()
            .checkpoint_formats
            .contains("hosted-csi-v1"),
        "R2"
    );

    let worker = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_session_container_environment_components(
        "hosted",
        awaken_execution_composition::ContainerEnvironmentComponents {
            provider: Arc::new(CheckpointContainerProvider),
            capacity: None,
            extra_mounts: Vec::new(),
            cache_volume_initializer: None,
        },
    )
    .with_hand_executor_factory(awaken_tool_relay::relay_hand_executor_factory())
    .with_environment_checkpoint_store(Arc::new(UnusedCheckpointStore))
    .with_standard_manifest(Default::default())
    .build()
    .expect("R3");
    assert!(
        worker
            .manifest()
            .checkpoint_formats
            .contains("hosted-csi-v1"),
        "R3"
    );
}

#[test]
fn enclosing_sandbox_boundary_is_the_standard_manifest_capability_source() {
    // Cause-effect graph / decision table:
    //
    // | Enclosing boundary | Backend | Effect |
    // | absent | n/a | deployment-derived sandbox evidence |
    // | present | non-empty | exact enclosing evidence, no container provider |
    // | present | empty | build fails closed |
    // | present + Session provider | any | ambiguous startup fails closed |
    //
    // R2 proves a Pod/VM startup can describe its already-enforced outer
    // boundary without constructing a duplicate Session container path. R3
    // prevents an unaddressable boundary from entering placement evidence.
    let capabilities = awaken_provisioning_contract::SandboxCapabilities {
        process_tree_supervision: false,
        isolation: awaken_provisioning_contract::IsolationClass::Container,
        tool_transparent: true,
        path_fidelity: true,
        enforced_readonly: true,
        network_isolation: true,
        enforced_network_allowlist: true,
        secret_egress_substitution: false,
        resource_limits: true,
        custom_rootfs: true,
        package_provisioning: false,
        control_services: Default::default(),
    };
    let worker = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_enclosing_sandbox_boundary("kubernetes-pod", capabilities.clone())
    .with_standard_manifest(Default::default())
    .build()
    .expect("enclosing boundary is valid without a Session container provider");
    assert_eq!(worker.manifest().sandbox, capabilities);
    assert_eq!(
        worker.manifest().sandbox_backends,
        ["kubernetes-pod".to_string()].into_iter().collect()
    );

    let error = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_enclosing_sandbox_boundary(" ", worker.manifest().sandbox.clone())
    .with_standard_manifest(Default::default())
    .build()
    .err()
    .expect("blank enclosing backend must fail closed");
    assert_eq!(
        error.to_string(),
        "enclosing sandbox backend must not be empty"
    );

    let error = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
        awaken_time::system_clock(),
    )
    .with_enclosing_sandbox_boundary("kubernetes-pod", worker.manifest().sandbox.clone())
    .with_session_container_provider("per-session", Arc::new(UnusedContainerProvider))
    .with_standard_manifest(Default::default())
    .build()
    .err()
    .expect("two sandbox capability sources must fail closed");
    assert_eq!(
        error.to_string(),
        "Session container provider and enclosing sandbox boundary are mutually exclusive"
    );
}

/// Cause-effect graph: the canonical materializer owns external credential
/// resolution. Installing its external-only constructor adds no local Vault,
/// while the same pinned adapter remains the installed Native provider
/// realization mechanism; Worker has no second resolver-startup path.
///
/// | Rule | Local Vault | External resolver | Result |
/// |---|---|---|---|
/// | X1 | F | T | build succeeds through `external_only` |
/// | X2 | F | T | exact external source + Worker provider-adapter evidence |
#[test]
fn external_credential_resolver_has_one_canonical_resolution_path() {
    let worker = WorkerNodeBuilder::new(
        awaken_worker_transport_security::WorkerUpstream::new("http://control"),
        awaken_time::system_clock(),
    )
    .with_credential_materializer(
        awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
            Arc::new(ExternalCredentialResolver),
            awaken_time::system_clock(),
        ),
    )
    .with_worker_local_credential_resolver(Arc::new(ExternalCredentialResolver))
    .with_standard_manifest(Default::default())
    .build()
    .expect("X1 exact resolver topology");
    assert!(
        worker
            .manifest()
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        "X1 exact observation/revalidation capability"
    );
    assert!(
        worker
            .manifest()
            .capabilities
            .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        "X2 the installed pinned adapter satisfies provider-source placement"
    );
    let evidence =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &worker.manifest().capabilities,
        )
        .expect("X2 evidence decodes");
    for source in [
        awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
        awaken_agent_contract::credential::CredentialMaterialSource::WorkerReference,
    ] {
        assert!(
            admits_native_source(&evidence, source),
            "X2 exact source, holder and Native mechanism coexist in one installed profile"
        );
    }
}

/// Credential capability derivation cause-effect graph:
///
/// C1 exact credential materializer is installed; C2 ACP is installed; C3 an
/// exact Worker-local observation/revalidation resolver is installed. E1 is
/// Provider credential-source placement, E2 is Worker/provider-adapter
/// realization, E3 is backend-correlated Workload realization, and E4 is the
/// Worker-local observation capability. Constraints: E1 and E2 follow only C1;
/// E3 requires C1 AND C2; E4 follows only C3. This keeps the two independent
/// credential mechanisms from being inferred from one another.
///
/// | Rule | C1 materializer | C2 ACP | C3 local resolver | E1/E2 provider | E3 ACP delivery | E4 Worker-local |
/// |---|---|---|---|---|---|---|
/// | R1 | T | T | F | T | T | F |
/// | R2 | T | F | F | T | F | F |
/// | R3 | F | F | T | F | F | T |
/// | R4 | F | F | F | F | F | F |
#[test]
fn standard_manifest_advertises_only_installed_credential_mechanisms() {
    let mut acp = deployment();
    acp.acp = Some(
        awaken_acp_contract::AcpWorkerProfile::new(
            ["claude".to_string(), "codex".to_string()],
            awaken_acp_catalog::acp_adapter_policy(),
        )
        .expect("mixed ACP profile"),
    );
    let workload = derive_standard_manifest(StandardManifestInputs {
        deployment: &acp,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: Some(credential_support()),
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    let workload_realization =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &workload.capabilities,
        )
        .expect("ACP credential realization capability decodes");
    let supports_workload_process_secret =
        |profile: &awaken_credential_contract::CredentialRealizationCapabilities| {
            profile
                .holders
                .contains(&awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Workload,
                    awaken_agent_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                ))
                && profile.realization_kinds.contains(
                    &awaken_credential_contract::CredentialRealizationKind::ProcessSecretEnvironment,
                )
        };
    let supports_worker_provider_adapter =
        |profile: &awaken_credential_contract::CredentialRealizationCapabilities| {
            profile
                .holders
                .contains(&awaken_agent_contract::credential::PlaintextHolder::new(
                    awaken_agent_contract::credential::PlaintextBoundary::Worker,
                    awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                ))
                && profile.realization_kinds.contains(
                    &awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
                )
        };
    assert!(
        supports_workload_process_secret(&workload_realization)
            || workload_realization
                .alternatives
                .iter()
                .any(supports_workload_process_secret)
    );
    assert!(
        supports_worker_provider_adapter(&workload_realization)
            || workload_realization
                .alternatives
                .iter()
                .any(supports_worker_provider_adapter)
    );
    assert_eq!(
        workload_realization
            .acp_backend_realization_kind("acp:claude")
            .expect("process delivery mapping is coherent"),
        Some(awaken_credential_contract::CredentialRealizationKind::ProcessSecretEnvironment)
    );
    assert_eq!(
        workload_realization
            .acp_backend_realization_kind("acp:codex")
            .expect("file delivery mapping is coherent"),
        Some(awaken_credential_contract::CredentialRealizationKind::PrivateSecretFile)
    );
    assert!(
        workload
            .capabilities
            .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY)
    );
    assert!(
        !workload
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        "R1 a materializer cannot imply the independent Worker-local resolver capability"
    );

    let without_acp = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment(),
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: Some(credential_support()),
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    let native =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without_acp.capabilities,
        )
        .expect("Native credential realization capability decodes");
    assert!(admits_native_source(
        &native,
        awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference
    ));
    assert!(
        without_acp
            .capabilities
            .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY)
    );
    assert!(
        !supports_workload_process_secret(&native)
            && !native
                .alternatives
                .iter()
                .any(supports_workload_process_secret),
        "R2 missing ACP cannot advertise Workload process-secret realization"
    );
    assert!(
        !without_acp
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        "R2 a materializer cannot imply the independent Worker-local resolver capability"
    );

    let local_observation_only = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment(),
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: true,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        local_observation_only
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        "R3 an installed resolver advertises its exact Worker-local capability"
    );
    assert!(
        !local_observation_only
            .capabilities
            .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        "R3 a Worker-local resolver cannot imply a provider credential source"
    );
    assert!(
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &local_observation_only.capabilities,
        )
        .expect("R3 credential realization evidence decodes")
        .is_empty(),
        "R3 observation/revalidation alone is not a material realization mechanism"
    );

    let without_credentials = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment(),
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        !without_credentials
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
            && !without_credentials
                .capabilities
                .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
        "R4 absent mechanisms advertise neither independent capability"
    );
    assert!(
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without_credentials.capabilities,
        )
        .expect("R4 credential realization evidence decodes")
        .is_empty(),
        "R4 absent materializers emit no realization evidence"
    );
    // R5: brokered ACP is installed with ordinary MCP/resource delivery ->
    // advertise only the brokered ACP model profile, preserve Native delivery,
    // and never advertise a local ACP fallback. R6: without an ACP deployment,
    // the same declaration must not create a Gateway ACP profile.
    let gateway_holder = awaken_agent_contract::credential::PlaintextHolder::new(
        awaken_agent_contract::credential::PlaintextBoundary::Platform,
        "test.gateway",
    );
    let brokered = std::collections::BTreeMap::from([(
        "claude".into(),
        awaken_credential_contract::CredentialRealizationCapabilities {
            holders: [gateway_holder.clone()].into_iter().collect(),
            material_sources: [
                awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_credential_contract::CredentialRealizationKind::PlatformProviderAdapter,
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        },
    )]);
    for has_acp in [true, false] {
        let selected_deployment = if has_acp { &acp } else { &deployment() };
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: selected_deployment,
            inference_schemes: &Default::default(),
            inference_credentials: &Default::default(),
            credential_materializer: Some(credential_support()),
            brokered_acp_credentials: &brokered,
            worker_local_credential_resolver_installed: false,
            remote_credential_realization: None,
            sandbox_override: None,
            checkpoint_formats: Default::default(),
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        let evidence = awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(&manifest.capabilities).unwrap();
        let profiles = std::iter::once(&evidence).chain(evidence.alternatives.iter());
        assert_eq!(
            profiles
                .clone()
                .any(|p| p.holders.contains(&gateway_holder)),
            has_acp,
            "R5/R6"
        );
        assert!(
            !profiles.clone().any(supports_workload_process_secret),
            "R5 no local ACP fallback"
        );
        assert!(
            profiles.clone().any(supports_worker_provider_adapter),
            "R5 Native mechanism retained"
        );
    }
}

/// Cause-effect graph for externally installed Session providers:
///
/// C1 exact credential materializer is installed
/// C2 provider proves secret substitution
/// C3 provider proves enforced no-bypass networking
/// E1 manifest publishes the provider's backend/capabilities
/// E2 WorkerRelay evidence exists iff C1 AND C2 AND C3.
///
/// | Rule | C1 | C2 | C3 | E1 | E2 |
/// |---|---|---|---|---|---|
/// | P1 | F | T | T | exact | F |
/// | P2 | T | F | T | exact | F |
/// | P3 | T | T | F | exact | F |
/// | P4 | T | T | T | exact | T |
#[test]
fn standard_manifest_requires_complete_provider_evidence_for_worker_relay() {
    for (credential_materializer, substitution, no_bypass, expected_relay) in [
        (false, true, true, false),
        (true, false, true, false),
        (true, true, false, false),
        (true, true, true, true),
    ] {
        let deployment = deployment();
        let mut sandbox = deployment.sandbox_support().0;
        sandbox.secret_egress_substitution = substitution;
        sandbox.enforced_network_allowlist = no_bypass;
        let manifest = derive_standard_manifest(StandardManifestInputs {
            deployment: &deployment,
            inference_schemes: &Default::default(),
            inference_credentials: &Default::default(),
            credential_materializer: credential_materializer.then(credential_support),
            brokered_acp_credentials: &Default::default(),
            worker_local_credential_resolver_installed: false,
            remote_credential_realization: None,
            sandbox_override: Some((sandbox.clone(), "external-secure-provider")),
            checkpoint_formats: Default::default(),
            resource_support: ResourceManifestSupport::None,
            application_capabilities: Default::default(),
            config: &StandardManifestConfig::default(),
        });
        assert_eq!(manifest.sandbox, sandbox, "provider evidence is exact");
        assert_eq!(
            manifest.sandbox_backends,
            std::collections::BTreeSet::from(["external-secure-provider".to_string()])
        );
        let realization =
            awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &manifest.capabilities,
            )
            .expect("credential evidence decodes");
        let supports_worker_relay =
            |profile: &awaken_credential_contract::CredentialRealizationCapabilities| {
                profile
                    .realization_kinds
                    .contains(&awaken_credential_contract::CredentialRealizationKind::WorkerRelay)
            };
        assert_eq!(
            supports_worker_relay(&realization)
                || realization.alternatives.iter().any(supports_worker_relay),
            expected_relay,
            "credentials={credential_materializer}, substitution={substitution}, no_bypass={no_bypass}"
        );
    }
}

#[test]
/// Cause graph: C1 Resources component installed -> session capability; C2 exact
/// Repository credential backend installed -> Repository capability; C3
/// inference materializer installed -> its exact realization evidence.
/// C1/C2 never synthesize C3: a Repository transport cannot authorize an
/// inference or MCP Worker relay merely because both hold material in Worker.
///
/// | Rule | C1 | C2 | C3 | Session | Repository | Credential evidence |
/// |---|---|---|---|---|---|---|
/// | W1 | F | - | F | F | F | empty |
/// | W2 | T | F | F | T | F | empty |
/// | W3 | T | T | F | T | T | empty |
/// | W4 | F | - | T | F | F | materializer evidence only |
fn worker_manifest_advertises_only_installed_resource_seams() {
    let deployment = deployment();
    let without = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    // Wire rule W5: the shipped terminal driver decodes/enforces the source
    // authorization independently of optional Resource materializers. This
    // advertises the protocol without synthesizing any Resource capability.
    assert!(
        without
            .capabilities
            .contains(awaken_worker_contract::SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY)
    );
    assert!(
        !without
            .capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY)
    );
    assert!(
        !without
            .capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
    assert!(
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without.capabilities,
        )
        .expect("W1 evidence decodes")
        .is_empty()
    );

    let secretless = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::Session,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        secretless
            .capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY)
    );
    assert!(
        !secretless
            .capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
    assert!(
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &secretless.capabilities,
        )
        .expect("W2 evidence decodes")
        .is_empty()
    );

    let credentialed = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::SessionWithRepositoryCredentials,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        credentialed
            .capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
    let evidence =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &credentialed.capabilities,
        )
        .expect("W3 evidence decodes");
    assert!(evidence.is_empty(), "W3");

    let materializer = SchemeMaterializer;
    let inference = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &materializer
            .supported_access_schemes()
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        inference_credentials: &materializer.credential_realization_capabilities(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        !inference
            .capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY)
    );
    assert!(
        !inference
            .capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
    let evidence =
        awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &inference.capabilities,
        )
        .expect("W4 evidence decodes");
    assert_eq!(
        evidence.holders,
        std::collections::BTreeSet::from([
            awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ),
        ])
    );
    assert_eq!(
        evidence.material_sources,
        std::collections::BTreeSet::from([
            awaken_agent_contract::credential::CredentialMaterialSource::ControlPlaneReference,
        ])
    );
    assert_eq!(
        evidence.realization_kinds,
        std::collections::BTreeSet::from([
            awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
        ])
    );
}

#[test]
fn worker_manifest_includes_explicit_application_capabilities() {
    let deployment = deployment();
    let manifest = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: std::collections::BTreeSet::from([
            "application:flow-envelope/v1".to_string(),
            "application:flow-tools/v1".to_string(),
        ]),
        config: &StandardManifestConfig::default(),
    });

    assert!(
        manifest
            .capabilities
            .contains("application:flow-envelope/v1")
    );
    assert!(manifest.capabilities.contains("application:flow-tools/v1"));
}

#[test]
fn standard_manifest_uses_one_typed_metadata_source() {
    // Protocol cause/effect table: C1=the canonical standard topology still
    // executes ordinary runtime protocol v1; C2=it implements the two-stage
    // terminal-cleanup transport and relative lease receipts. R1 C1+C2 =>
    // publish one inclusive 1..current range and pass the canonical protocol
    // predicate. Omitting C1 would strand ordinary Runs; omitting C2 makes it
    // ineligible for terminal cleanup recovery.
    let deployment = deployment();
    let config = StandardManifestConfig::new("build:test")
        .with_zone("zone:test")
        .with_extra_capabilities(["operator:test/v1".to_string()])
        .with_max_concurrent(7);
    let manifest = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: None,
        brokered_acp_credentials: &Default::default(),
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &config,
    });

    assert_eq!(manifest.build_digest, "build:test");
    assert_eq!(manifest.zone.as_deref(), Some("zone:test"));
    assert!(manifest.capabilities.contains("operator:test/v1"));
    assert_eq!(manifest.capacity.max_concurrent, 7);
    assert_eq!(
        manifest.runtime_protocol,
        awaken_worker_contract::VersionRange {
            min: 1,
            max: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION
        },
        "R1 publishes the implemented protocol range"
    );
    assert!(
        manifest.supports_current_runtime_protocol(),
        "R1 is admitted by the canonical capability predicate"
    );
}

#[test]
fn configured_container_acp_uses_live_image_probe_targets_only() {
    // Cause/effect graph: C1=container tier; C2=typed ACP profile; C3=image
    // identity. Effects: E1=create one image-local live probe target; E2=defer
    // to the host-discovery source; E3=fail configuration before registration.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | R1 | no  | yes | n/a | E2 no container target |
    // | R2 | yes | no  | yes | E2 no ACP target |
    // | R3 | yes | yes | yes | E1 exact container argv + image identity |
    // | R4 | yes | yes | no  | E3 error |
    // | R5 | yes | every catalog row | yes | one exact target per row |
    let mut config = deployment();
    config.acp = Some(
        awaken_acp_contract::AcpWorkerProfile::new(
            vec!["gemini".into()],
            awaken_acp_catalog::acp_adapter_policy(),
        )
        .unwrap(),
    );
    assert!(
        configured_container_acp_targets(&config)
            .unwrap()
            .is_empty(),
        "R1"
    );

    config.sandbox_tier = awaken_deployment_config::SandboxTier::Docker;
    config.container_image = Some("image@sha256:exact".into());
    config.acp = None;
    assert!(
        configured_container_acp_targets(&config)
            .unwrap()
            .is_empty(),
        "R2"
    );

    config.acp = Some(
        awaken_acp_contract::AcpWorkerProfile::new(
            vec!["gemini".into()],
            awaken_acp_catalog::acp_adapter_policy(),
        )
        .unwrap(),
    );
    let targets = configured_container_acp_targets(&config).unwrap();
    assert_eq!(targets.len(), 1, "R3");
    assert_eq!(targets[0].cli_id, "gemini", "R3");
    assert_eq!(
        targets[0].adapter_version, "container-image:image@sha256:exact",
        "R3"
    );
    assert_eq!(
        targets[0].argv,
        [
            "/usr/bin/env",
            "GEMINI_API_KEY=awaken-capability-probe",
            "gemini",
            "--acp"
        ],
        "R3"
    );
    assert_eq!(targets[0].auth_method_id, None, "R3");

    config.container_image = None;
    assert!(configured_container_acp_targets(&config).is_err(), "R4");

    config.container_image = Some("image@sha256:exact".into());
    let catalog = awaken_acp_catalog::known_acp_clis();
    config.acp = Some(
        awaken_acp_contract::AcpWorkerProfile::new(
            catalog.iter().map(|cli| cli.id.to_owned()),
            awaken_acp_catalog::acp_adapter_policy(),
        )
        .unwrap(),
    );
    let targets = configured_container_acp_targets(&config).unwrap();
    assert_eq!(targets.len(), catalog.len(), "R5");
    for cli in catalog {
        let target = targets
            .iter()
            .find(|target| target.cli_id == cli.id)
            .expect("R5 catalog row target");
        assert_eq!(target.cli_id, cli.id, "R5");
        assert_eq!(
            target.argv,
            cli.container_probe_argv.unwrap_or(cli.container_argv),
            "R5"
        );
        assert_eq!(
            target.auth_method_id.as_deref(),
            cli.capability_probe_auth_method_id,
            "R5"
        );
    }

    let codex = targets
        .iter()
        .find(|target| target.cli_id == "codex")
        .expect("R5 codex target");
    assert_eq!(codex.argv[0], "/usr/bin/env", "R5 probe wrapper");
    assert_eq!(codex.auth_method_id.as_deref(), Some("api-key"), "R5");
}

#[test]
fn sigint_exits_promptly_sigterm_waits() {
    assert!(
        grace_window(false, None).is_zero(),
        "ctrl-c drains then exits at once"
    );
    assert!(
        grace_window(false, Some(99)).is_zero(),
        "a configured grace never delays a foreground ctrl-c"
    );
    assert_eq!(
        grace_window(true, None).as_secs(),
        20,
        "SIGTERM default grace is 20s"
    );
    assert_eq!(
        grace_window(true, Some(5)).as_secs(),
        5,
        "the grace window is configurable"
    );
}

// Boundary: an explicitly configured zero grace collapses SIGTERM to the
// prompt-exit behavior — the orchestrator asked for no in-flight wait, so a
// graceful stop must not silently substitute the 20s default.
#[test]
fn a_configured_zero_grace_exits_immediately_even_on_sigterm() {
    assert!(
        grace_window(true, Some(0)).is_zero(),
        "grace of 0 means no wait, not the default"
    );
}

/// Session Resource composition cause/effect decision table. C1 the canonical
/// registration-bound remote projection is absent/present. E1 the immutable
/// manifest omits/publishes `session-resources/v1`. The positive rule proves
/// platform composers reuse the same identity-bound upstream as the standalone
/// Worker instead of manufacturing capability text or a second Resource client.
///
/// | Rule | C1 remote projection | E1 manifest capability |
/// |---|---|---|
/// | R1 | absent | omitted |
/// | R2 | present | published |
#[test]
fn resource_capability_follows_the_canonical_registered_remote_projection() {
    let builder = || {
        WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("http://control"),
            awaken_time::system_clock(),
        )
        .with_standard_manifest(Default::default())
    };
    let without = builder().build().expect("R1 no unsupported Resource claim");
    assert!(
        !without
            .manifest()
            .capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
        "R1"
    );
    let with = builder()
        .with_remote_session_resource_support()
        .build()
        .expect("R2 canonical remote Resource projection");
    assert!(
        with.manifest()
            .capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
        "R2"
    );
}

mod restore_capability;
