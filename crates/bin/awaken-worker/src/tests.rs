use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;

use super::{
    CredentialMaterializerSupport, InferenceExecutorMaterializer, ResourceManifestSupport,
    StandardManifestConfig, StandardManifestInputs, WorkerNodeBuilder, derive_standard_manifest,
    grace_window,
};

struct SchemeMaterializer;

struct ExternalCredentialResolver;

#[async_trait::async_trait]
impl awaken_runtime_contract::CredentialMaterialResolver for ExternalCredentialResolver {
    fn supported_material_sources(
        &self,
    ) -> std::collections::BTreeSet<awaken_runtime_contract::CredentialMaterialSource> {
        std::collections::BTreeSet::from([
            awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_runtime_contract::CredentialMaterialSource::WorkerReference,
        ])
    }

    fn supports_recipient_bound_envelopes(&self) -> bool {
        true
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

fn deployment() -> awaken_runtime_host::DeploymentConfig {
    awaken_runtime_host::DeploymentConfig::ephemeral()
}

fn credential_support() -> CredentialMaterializerSupport {
    let material_sources = std::collections::BTreeSet::from([
        awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference,
    ]);
    CredentialMaterializerSupport {
        process_secret: awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources: material_sources.clone(),
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment,
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        worker_relay: awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources,
            realization_kinds: [awaken_runtime_contract::CredentialRealizationKind::WorkerRelay]
                .into_iter()
                .collect(),
            ..Default::default()
        },
    }
}

#[test]
fn worker_uses_the_injected_web_search_registry_as_its_only_catalog() {
    // Cause-effect graph / decision table:
    // R1 no injection -> open built-ins; R2 one injected deployment registry ->
    // exactly that catalog (the empty catalog is the minimal observable
    // replacement). This proves Cloud does not append a second provider path
    // beside the Worker's default registry.
    let registry = awaken_runtime_host::WebSearchProviderRegistry::default();
    let worker = WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new(
        "https://control.test",
    ))
    .with_standard_manifest(Default::default())
    .with_web_search_provider_registry(registry)
    .build()
    .unwrap();
    let descriptors = worker.web_search_providers.descriptors();
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
        materializer: Some(&materializer),
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });

    assert!(manifest.capabilities.contains("native-runtime"));
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
fn standard_builder_derives_from_its_installed_materializer() {
    let worker = WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new("http://control"))
        .with_inference_materializer(Arc::new(SchemeMaterializer))
        .with_standard_manifest(Default::default())
        .build()
        .expect("installed standard topology is valid");

    assert!(worker.manifest().capabilities.contains("test-access/v1"));
}

/// Cause-effect graph: an external resolver without the canonical materializer
/// is meaningless and fails build; stores + resolver compose once, while a
/// resolver without an installed consuming adapter cannot advertise realization.
///
/// | Rule | Stores | External resolver | Result |
/// |---|---|---|---|
/// | X1 | F | T | build error |
/// | X2 | T | T | build succeeds; no invented realization evidence |
#[test]
fn external_credential_resolver_composes_with_the_canonical_materializer() {
    let missing =
        WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new("http://control"))
            .with_external_credential_resolver(Arc::new(ExternalCredentialResolver))
            .with_standard_manifest(Default::default())
            .build()
            .err()
            .expect("X1 resolver without a materializer is invalid");
    assert!(missing.to_string().contains("credential materializer"));

    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let worker = WorkerNodeBuilder::new(awaken_runtime_host::WorkerUpstream::new("http://control"))
        .with_credential_materializer(awaken_runtime_host::PinnedCredentialMaterializer::new(
            credentials,
            secrets,
        ))
        .with_external_credential_resolver(Arc::new(ExternalCredentialResolver))
        .with_worker_local_credential_resolver(Arc::new(ExternalCredentialResolver))
        .with_standard_manifest(Default::default())
        .build()
        .expect("X2 exact resolver topology");
    assert!(
        worker
            .manifest()
            .capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
        "X2 exact observation/revalidation capability"
    );
    let evidence =
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &worker.manifest().capabilities,
        )
        .expect("X2 evidence decodes");
    assert!(
        evidence.is_empty(),
        "X2 a material source is not itself an installed realization mechanism"
    );
}

/// Credential capability derivation cause graph:
///
/// inference materializer -> Worker provider adapter; exact credential
/// materializer + installed ACP profile -> Workload process-secret delivery.
/// Missing either ACP cause advertises neither half of that workload tuple.
///
/// | Rule | materializer | credential store | ACP | Result |
/// |---|---|---|---|---|
/// | C1 | credential-aware | - | - | Worker/provider-adapter |
/// | C2 | - | T | T | Workload/process-secret |
/// | C3 | - | T | F | no Workload/process-secret |
#[test]
fn standard_manifest_advertises_only_installed_credential_mechanisms() {
    let mut acp = deployment();
    acp.acp = Some(
        awaken_runtime_host::AcpWorkerProfile::new(["claude".to_string()], None)
            .expect("one ACP profile"),
    );
    let workload = derive_standard_manifest(StandardManifestInputs {
        deployment: &acp,
        materializer: None,
        credential_materializer: Some(credential_support()),
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    let workload_realization =
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &workload.capabilities,
        )
        .expect("ACP credential realization capability decodes");
    assert!(
        workload_realization
            .holders
            .contains(&awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Workload,
                awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
            ))
    );
    assert!(
        workload_realization.realization_kinds.contains(
            &awaken_runtime_contract::CredentialRealizationKind::ProcessSecretEnvironment
        )
    );

    let without_acp = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment(),
        materializer: None,
        credential_materializer: Some(credential_support()),
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &StandardManifestConfig::default(),
    });
    assert!(
        awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
            &without_acp.capabilities,
        )
        .expect("empty credential realization capability decodes")
        .is_empty()
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
            materializer: None,
            credential_materializer: credential_materializer.then(credential_support),
            worker_local_credentials: false,
            remote_credential_realization: None,
            sandbox_override: Some((sandbox.clone(), "external-secure-provider")),
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
        assert_eq!(
            realization
                .realization_kinds
                .contains(&awaken_runtime_contract::CredentialRealizationKind::WorkerRelay),
            expected_relay,
            "credentials={credential_materializer}, substitution={substitution}, no_bypass={no_bypass}"
        );
    }
}

#[test]
/// Cause graph: C1 Resource plane installed -> session capability; C2 exact
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
        materializer: None,
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
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
        materializer: None,
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
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
        materializer: None,
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
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
        materializer: Some(&materializer),
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
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
        materializer: None,
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
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
    let deployment = deployment();
    let config = StandardManifestConfig::new("build:test")
        .with_zone("zone:test")
        .with_extra_capabilities(["operator:test/v1".to_string()])
        .with_max_concurrent(7);
    let manifest = derive_standard_manifest(StandardManifestInputs {
        deployment: &deployment,
        materializer: None,
        credential_materializer: None,
        worker_local_credentials: false,
        remote_credential_realization: None,
        sandbox_override: None,
        resource_support: ResourceManifestSupport::None,
        application_capabilities: Default::default(),
        config: &config,
    });

    assert_eq!(manifest.build_digest, "build:test");
    assert_eq!(manifest.zone.as_deref(), Some("zone:test"));
    assert!(manifest.capabilities.contains("operator:test/v1"));
    assert_eq!(manifest.capacity.max_concurrent, 7);
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
