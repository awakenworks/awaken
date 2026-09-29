// @formal evidence session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
// @formal evidence worker.realization_scope_uses_only_the_kubernetes_namespace_or_persisted_local_workspace
use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;

use super::{
    CredentialMaterializerSupport, InferenceExecutorMaterializer, ResourceManifestSupport,
    StandardManifestConfig, StandardManifestInputs, WorkerApplicationSource, WorkerNodeBuilder,
    configured_container_acp_targets, derive_standard_manifest, grace_window,
    validate_worker_manifest, worker_realization_namespace,
};

#[test]
fn container_realization_namespace_is_installation_wide() {
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
        let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = awaken_session_executor::SandboxTier::K8s;
        deployment.sandbox.k8s_namespace = namespace.into();
        deployment.storage_dir = Some(local_root.into());
        worker_realization_namespace(&deployment).expect("Kubernetes realization namespace")
    };
    let first_root = tempfile::tempdir().expect("first Worker root");
    let replacement_root = tempfile::tempdir().expect("replacement Worker root");
    let worker_a = kubernetes("installation-a", first_root.path());
    let replacement = kubernetes("installation-a", replacement_root.path());
    let other_installation = kubernetes("installation-b", first_root.path());
    let mut markerless_kubernetes = awaken_session_executor::DeploymentConfig::ephemeral();
    markerless_kubernetes.sandbox_tier = awaken_session_executor::SandboxTier::K8s;
    markerless_kubernetes.sandbox.k8s_namespace = "installation-a".into();
    markerless_kubernetes.storage_dir = None;
    let markerless = worker_realization_namespace(&markerless_kubernetes)
        .expect("Kubernetes does not require a local Workspace marker");
    assert_eq!(worker_a, replacement, "R1/E1-E3");
    assert_eq!(worker_a, markerless, "R1/E4");
    assert_ne!(worker_a, other_installation, "R2/E2");

    let local = |root: &std::path::Path| {
        let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = awaken_session_executor::SandboxTier::Docker;
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
impl awaken_sandbox_container::ContainerEnvironmentProvider for CheckpointContainerProvider {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    async fn create_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        _intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment creation",
        ))
    }
    async fn observe_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }
    async fn adopt_environment_for_effect(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment adoption",
        ))
    }
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_sandbox_container::ContainerEffectFence>,
        _terminal_effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        vec!["hosted-csi-v1".into()]
    }

    async fn create_unattached_environment(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        unreachable!("builder tests do not create an environment")
    }

    async fn adopt_unattached_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        unreachable!("builder tests do not adopt an environment")
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
impl awaken_sandbox_container::ContainerEnvironmentProvider for UnusedContainerProvider {
    fn adoption_layout(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<
        awaken_container_contract::ContainerAdoptionLayout,
        awaken_provisioning_contract::SandboxError,
    > {
        awaken_sandbox_container::container_adoption_layout(handle)
    }

    fn physical_incarnation(
        &self,
        handle: &awaken_provisioning_contract::SandboxHandle,
    ) -> Result<String, awaken_provisioning_contract::SandboxError> {
        awaken_sandbox_container::container_physical_incarnation(handle)
    }

    async fn create_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        _intent: awaken_container_contract::EnvironmentRealizationIntent,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment creation",
        ))
    }
    async fn observe_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
    ) -> Result<
        awaken_provisioning_contract::SandboxObservation,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not observe Environments",
        ))
    }
    async fn adopt_environment_for_effect(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        _effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced Environment adoption",
        ))
    }
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
        _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
        _expected_effect_fence: Option<&awaken_sandbox_container::ContainerEffectFence>,
        _terminal_effect_fence: &awaken_sandbox_container::ContainerEffectFence,
        _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
    ) -> Result<
        Option<std::sync::Arc<dyn awaken_sandbox_container::ContainerEnvironment>>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "test double does not implement fenced terminal preparation",
        ))
    }

    async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
        Ok(())
    }

    async fn create_unattached_environment(
        &self,
        _spec: &awaken_provisioning_contract::SandboxSpec,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "unused test provider",
        ))
    }

    async fn adopt_unattached_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        awaken_provisioning_contract::SandboxError,
    > {
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "unused test provider",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialMaterialResolver for ExternalCredentialResolver {
    fn material_capabilities(
        &self,
    ) -> std::collections::BTreeSet<awaken_runtime_contract::CredentialMaterialCapability> {
        let mut profiles = std::collections::BTreeSet::new();
        for holder in [
            awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                "awaken.worker",
            ),
            awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                "awaken.workload.acp",
            ),
        ] {
            for source in [
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
                awaken_runtime_contract::CredentialMaterialSource::WorkerReference,
            ] {
                for envelope in [false, true] {
                    profiles.insert(awaken_runtime_contract::CredentialMaterialCapability {
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
        _request: awaken_runtime_contract::CredentialMaterialRequest<'_>,
    ) -> Result<
        awaken_runtime_contract::ResolvedCredentialMaterial,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        Err(awaken_runtime_contract::CredentialMaterialError::Unavailable)
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialObservationSource for ExternalCredentialResolver {
    async fn credential_observations(
        &self,
    ) -> Result<
        std::collections::BTreeSet<awaken_runtime_contract::CredentialObservation>,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        Ok(std::collections::BTreeSet::from([
            awaken_runtime_contract::CredentialObservation::available(
                awaken_runtime_contract::CredentialRef {
                    id: "cred:worker".into(),
                    revision: 4,
                },
                1,
            ),
        ]))
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::WorkerLocalReferenceRevalidator for ExternalCredentialResolver {
    async fn revalidate_worker_reference(
        &self,
        credential: &awaken_runtime_contract::CredentialRef,
    ) -> Result<
        awaken_runtime_contract::CredentialObservation,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        Ok(awaken_runtime_contract::CredentialObservation::available(
            credential.clone(),
            1,
        ))
    }
}

impl InferenceExecutorMaterializer for SchemeMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &["test-access/v1"]
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources: [
                awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            ]
            .into_iter()
            .collect(),
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
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

fn deployment() -> awaken_session_executor::DeploymentConfig {
    awaken_session_executor::DeploymentConfig::ephemeral()
}

fn admits_native_source(
    evidence: &awaken_runtime_contract::CredentialRealizationCapabilities,
    source: awaken_runtime_contract::CredentialMaterialSource,
) -> bool {
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialRealizationKind, CredentialRef,
        CredentialTarget, CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    let holder = PlaintextHolder::new(
        PlaintextBoundary::Worker,
        awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
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
        awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
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
            awaken_lease_contract::system_clock(),
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
        awaken_lease_contract::system_clock(),
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
        brokered_acp_model_access: None,
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });

    assert!(manifest.capabilities.contains("native-runtime"));
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &manifest.capabilities,
        )
        .expect("credential realization capability decodes");
    assert!(
        realization
            .holders
            .contains(&awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            ))
    );
    assert!(
        realization
            .realization_kinds
            .contains(&awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter)
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
        (awaken_session_executor::SandboxTier::Local, false, true),
        (awaken_session_executor::SandboxTier::Namespace, false, false),
        (awaken_session_executor::SandboxTier::Namespace, true, true),
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
            brokered_acp_model_access: None,
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
    deployment.sandbox_tier = awaken_session_executor::SandboxTier::Namespace;
    deployment.sandbox.allow_local_fallback = true;
    deployment.sandbox.trusted_host_enabled = false;
    let make = || {
        WorkerNodeBuilder::new(
            awaken_worker_transport_security::WorkerUpstream::new("http://control"),
            awaken_lease_contract::system_clock(),
        )
        .with_deployment_config(deployment.clone())
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
        deployment.sandbox_support_for_tier(awaken_session_executor::SandboxTier::Local);
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
        awaken_session_executor::SandboxTier::Namespace
    );

    let stale = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        materializer: None,
        credential_materializer: None,
        brokered_acp_model_access: None,
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
        awaken_lease_contract::system_clock(),
    )
    .with_inference_materializer(Arc::new(SchemeMaterializer))
    .with_standard_manifest(Default::default())
    .build()
    .expect("installed standard topology is valid");

    assert!(worker.manifest().capabilities.contains("test-access/v1"));
}

#[test]
fn worker_builder_rejects_scoped_execution_without_platform_or_host_consent() {
    // W1 typed builder bypasses TOML but must obey the same admission rule.
    // W2 disabled/valid selection never attests a full supervision guarantee.
    for scoped in [false, true] {
        for trusted in [false, true] {
            let mut deployment = deployment();
            deployment.sandbox_tier = awaken_session_executor::SandboxTier::Namespace;
            deployment.sandbox.trusted_host_enabled = trusted;
            deployment.sandbox.trusted_host_scoped_execution = scoped;
            let result = WorkerNodeBuilder::new(
                awaken_worker_transport_security::WorkerUpstream::new("http://control"),
                awaken_lease_contract::system_clock(),
            )
            .with_deployment_config(deployment)
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
        awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest;
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
            awaken_lease_contract::system_clock(),
        )
        .with_session_container_provider("hosted", Arc::new(CheckpointContainerProvider))
        .with_hand_executor_factory(crate::relay_hand_executor_factory())
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
        awaken_lease_contract::system_clock(),
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
    .with_hand_executor_factory(crate::relay_hand_executor_factory())
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
        awaken_lease_contract::system_clock(),
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
        awaken_lease_contract::system_clock(),
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
        awaken_lease_contract::system_clock(),
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
        awaken_lease_contract::system_clock(),
    )
    .with_credential_materializer(
        awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
            Arc::new(ExternalCredentialResolver),
            awaken_lease_contract::system_clock(),
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &worker.manifest().capabilities,
        )
        .expect("X2 evidence decodes");
    for source in [
        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
        awaken_runtime_contract::CredentialMaterialSource::WorkerReference,
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
        awaken_session_executor::AcpWorkerProfile::new(["claude".to_string(), "codex".to_string()])
            .expect("mixed ACP profile"),
    );
    let workload = derive_standard_manifest(StandardManifestInputs {
        deployment: &acp,
        inference_schemes: &Default::default(),
        inference_credentials: &Default::default(),
        credential_materializer: Some(credential_support()),
        brokered_acp_model_access: None,
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    let workload_realization =
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &workload.capabilities,
        )
        .expect("ACP credential realization capability decodes");
    let supports_workload_process_secret =
        |profile: &awaken_runtime_contract::CredentialRealizationCapabilities| {
            profile
                .holders
                .contains(&awaken_runtime_contract::PlaintextHolder::new(
                    awaken_runtime_contract::PlaintextBoundary::Workload,
                    awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
                ))
                && profile.realization_kinds.contains(
                    &awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment,
                )
        };
    let supports_worker_provider_adapter =
        |profile: &awaken_runtime_contract::CredentialRealizationCapabilities| {
            profile
                .holders
                .contains(&awaken_runtime_contract::PlaintextHolder::new(
                    awaken_runtime_contract::PlaintextBoundary::Worker,
                    awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
                ))
                && profile.realization_kinds.contains(
                    &awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
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
        Some(awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment)
    );
    assert_eq!(
        workload_realization
            .acp_backend_realization_kind("acp:codex")
            .expect("file delivery mapping is coherent"),
        Some(awaken_runtime_contract::CredentialRealizationKind::PrivateSecretFile)
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
        brokered_acp_model_access: None,
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    let native =
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without_acp.capabilities,
        )
        .expect("Native credential realization capability decodes");
    assert!(admits_native_source(
        &native,
        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference
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
        brokered_acp_model_access: None,
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
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
        brokered_acp_model_access: None,
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without_credentials.capabilities,
        )
        .expect("R4 credential realization evidence decodes")
        .is_empty(),
        "R4 absent materializers emit no realization evidence"
    );
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
            brokered_acp_model_access: None,
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
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &manifest.capabilities,
            )
            .expect("credential evidence decodes");
        let supports_worker_relay =
            |profile: &awaken_runtime_contract::CredentialRealizationCapabilities| {
                profile
                    .realization_kinds
                    .contains(&awaken_runtime_contract::CredentialRealizationKind::WorkerRelay)
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
        brokered_acp_model_access: None,
        worker_local_credential_resolver_installed: false,
        remote_credential_realization: None,
        sandbox_override: None,
        checkpoint_formats: Default::default(),
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
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
        brokered_acp_model_access: None,
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
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
        brokered_acp_model_access: None,
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
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
        brokered_acp_model_access: None,
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
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &inference.capabilities,
        )
        .expect("W4 evidence decodes");
    assert_eq!(
        evidence.holders,
        std::collections::BTreeSet::from([awaken_runtime_contract::PlaintextHolder::new(
            awaken_runtime_contract::PlaintextBoundary::Worker,
            awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),])
    );
    assert_eq!(
        evidence.material_sources,
        std::collections::BTreeSet::from([
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
        ])
    );
    assert_eq!(
        evidence.realization_kinds,
        std::collections::BTreeSet::from([
            awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
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
        brokered_acp_model_access: None,
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
        brokered_acp_model_access: None,
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
    config.acp = Some(awaken_session_executor::AcpWorkerProfile::new(vec!["gemini".into()]).unwrap());
    assert!(
        configured_container_acp_targets(&config)
            .unwrap()
            .is_empty(),
        "R1"
    );

    config.sandbox_tier = awaken_session_executor::SandboxTier::Docker;
    config.container_image = Some("image@sha256:exact".into());
    config.acp = None;
    assert!(
        configured_container_acp_targets(&config)
            .unwrap()
            .is_empty(),
        "R2"
    );

    config.acp = Some(awaken_session_executor::AcpWorkerProfile::new(vec!["gemini".into()]).unwrap());
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
    let catalog = awaken_run_executor_acp::known_acp_clis();
    config.acp = Some(
        awaken_session_executor::AcpWorkerProfile::new(catalog.iter().map(|cli| cli.id.to_owned()))
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
            awaken_lease_contract::system_clock(),
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
