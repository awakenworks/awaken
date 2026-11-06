use super::{
    HOST_EXECUTOR_CAPABILITY, PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, RunId, remote_worker_placement,
};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_dispatch_contract::CompletionSink;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedModelCandidate};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_session_run_delivery::durable_resume_input;
use std::sync::Arc;

use crate::NoModelConfiguredExecutor;
use awaken_dispatch_execution::session_dispatch::UNCONFIGURED_MODEL_REF;

fn host_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
    awaken_runtime_contract::ExecutableAgentSnapshot::builder("test")
        .model(ModelBinding::new("host", "primary", "native"))
        .build()
        .resolved_spec
}

fn environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env-1".into(),
        revision: awaken_environment_contract::EnvironmentRevision(3),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "fp-env".into(),
        ),
        sandbox: serde_json::from_value(serde_json::json!({
            "isolation": "container",
            "requests": {"cpu_millis": 250, "memory_bytes": 33554432},
            "limits": {"memory_bytes": 67108864}
        }))
        .expect("valid SandboxOverride fixture"),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: awaken_environment_contract::EnvironmentPackages {
            npm: vec!["tsx@4".into()],
            ..Default::default()
        },
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
            hosts: vec!["api.example.test".into()],
        },
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    }
}

fn environment_with_isolation(
    isolation: awaken_provisioning_contract::IsolationClass,
) -> awaken_session_configuration_contract::EnvironmentSnapshot {
    let mut environment = environment();
    let isolation = match isolation {
        awaken_provisioning_contract::IsolationClass::Workdir => "workdir",
        awaken_provisioning_contract::IsolationClass::Namespace => "namespace",
        awaken_provisioning_contract::IsolationClass::Container => "container",
    };
    environment.sandbox = serde_json::from_value(serde_json::json!({
        "isolation": isolation,
    }))
    .expect("valid isolation-only SandboxOverride fixture");
    environment.packages = Default::default();
    environment.network = awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted;
    environment
}

fn projected_acp_models() -> awaken_runtime_contract::resolved::ResolvedSpec {
    let mut models = host_models();
    models.model_binding = ResolvedModelCandidate::try_provider_with_acp(
        ModelBinding::new("provider", "model", "acp:claude"),
        "provider@1",
        "route@1",
        "workspace",
        None,
        awaken_runtime_contract::InferenceEndpoint {
            adapter_kind: "anthropic".into(),
            api_dialect: "anthropic_messages".into(),
            base_url: "https://example.test".into(),
            upstream_model: "model".into(),
            processing_placement: None,
        },
        awaken_runtime_contract::resolved::AcpExecutionProfile {
            supports_tool_aliases: false,
            capability_fingerprint: "sha256:test-capability".into(),
            capability_adapter_version: "test".into(),
            session_configuration: Default::default(),
        },
    )
    .expect("coherent projected ACP candidate");
    models
}

fn repository_manifest() -> crate::session_resource::SessionResourceManifest {
    use awaken_resource_contract::{
        BindingId, ClonePolicy, ConfigVersion, RepositoryConfigVersion, RepositoryId,
        ResourceAccess,
    };
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, SessionResourceManifest,
    };

    SessionResourceManifest::new(
        "workspace-a",
        ResolvedSessionResources::try_new(
            vec![ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: BindingId::new("repo-binding"),
                source: ResolvedInputSource::Repository {
                    repository_id: RepositoryId::from("repo-a"),
                    config: RepositoryConfigVersion {
                        repository_id: "repo-a".into(),
                        version: ConfigVersion::INITIAL,
                        remote_url: "https://example.invalid/repo.git".into(),
                        credential_binding: Some("credential-a".into()),
                        initial_branch: None,
                        initial_commit: None,
                        clone_policy: ClonePolicy::default(),
                    },
                    credential: None,
                },
                mount_path: "/workspace/repo".into(),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            }],
            Vec::new(),
        )
        .expect("valid frozen Repository manifest"),
    )
}

fn resume_command(run_id: &str, correlation_id: &str, answer: &str) -> ResumeCommand {
    ResumeCommand {
        operation_id: None,
        correlation_id: correlation_id.into(),
        run_id: RunId(run_id.into()),
        thread_id: ThreadId("thread-1".into()),
        snapshot_id: ExecutableAgentSnapshotId("snapshot-1".into()),
        catalog_fingerprint: CatalogFingerprint("catalog-1".into()),
        result: ResumeResult::Input(answer.into()),
        context_messages: Vec::new(),
        now_ms: 10,
    }
}

#[test]
fn durable_resume_identity_is_exactly_the_answered_ticket() {
    // Cause/effect graph: C1 exact retry of one (Run, correlation, payload);
    // C2 same ticket with a conflicting payload; C3 delimiter-ambiguous raw
    // strings belonging to different tickets. Effects: E1 exact retry yields
    // one identical PendingInput; E2 conflict keeps the same message id but a
    // different payload so the canonical Inbox rejects it; E3 distinct ticket
    // pairs never alias. Constraint: caller time is not delivery identity.
    //
    // | Rule | ticket pair | payload | Effect |
    // | R1 | same | same | E1 identical input |
    // | R2 | same | different | E2 same id, conflicting input |
    // | R3 | different but delimiter-ambiguous | any | E3 different id |
    let exact = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
    let retry = durable_resume_input(resume_command("run-a", "corr-a", "yes"));
    assert_eq!(exact, retry, "R1");

    let conflict = durable_resume_input(resume_command("run-a", "corr-a", "no"));
    assert_eq!(exact.message_id, conflict.message_id, "R2 identity");
    assert_ne!(exact, conflict, "R2 payload conflict");

    let left = durable_resume_input(resume_command("a", "bc", "yes"));
    let right = durable_resume_input(resume_command("ab", "c", "yes"));
    assert_ne!(left.message_id, right.message_id, "R3");
}

#[test]
fn environment_and_backend_compile_one_worker_sandbox_requirement_vector() {
    use awaken_provisioning_contract::IsolationClass;
    use awaken_runtime_contract::resolved::{BackendModelSelection, ResolvedModelCandidate};

    // Cause/effect graph:
    // C1=Native; C2=projected ACP opaque process; C3=trusted BackendOwned ACP;
    // C4=A2A-only; C5=frozen Environment isolation/network/requests/limits/packages;
    // C6=prepared image. Effects: E1=one PlacementRequirements.sandbox vector;
    // E2=opaque ACP adds transparent Namespace semantics; E3=A2A adds no local
    // Sandbox demand; E4=image replaces package provisioning with rootfs demand.
    // Constraint: candidates are conjunctive for admission; any local candidate
    // keeps the local Environment requirement.
    //
    // Decision table:
    // R1 C1+C5 -> exact Environment enforcement, including the Environment's
    // Namespace-or-stronger path contract.
    // R2 C2+C5 -> R1 plus transparent/path-fidelity.
    // R3 C3+C5 -> trusted Workdir semantics; no artificial Namespace demand.
    // R4 C4+C5 -> default (no local Environment) vector.
    // R5 C1+C5+C6 -> custom-rootfs=true, package-provisioning=false.
    // R6 C4 plus any Native fallback -> R1 (the candidate set is not A2A-only).
    let frozen = environment();

    let native = remote_worker_placement(&host_models(), Some(&frozen), None, true);
    assert_eq!(native.sandbox.isolation, IsolationClass::Container, "R1");
    assert!(native.sandbox.network_isolation, "R1 network");
    assert!(native.sandbox.enforced_network_allowlist, "R1 allowlist");
    assert!(native.sandbox.resource_limits, "R1 limits");
    assert!(native.sandbox.package_provisioning, "R1 packages");
    // R7 strict supervision is independent of opaque-process path rules.
    // Native/ACP x isolated/trusted x required/omitted all preserve the
    // frozen requirement; TrustedHost still waives no process guarantee.
    for models in [host_models(), projected_acp_models()] {
        for trusted in [false, true] {
            for supervised in [false, true] {
                let mut requested = environment();
                requested.sandbox = awaken_provisioning_contract::SandboxOverride {
                    process_tree_supervision: supervised,
                    isolation: Some(IsolationClass::Workdir),
                    host_access: if trusted {
                        awaken_provisioning_contract::HostAccessPolicy::TrustedHost
                    } else {
                        awaken_provisioning_contract::HostAccessPolicy::Isolated
                    },
                    ..Default::default()
                };
                requested.network =
                    awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted;
                requested.packages = Default::default();
                let placement = remote_worker_placement(&models, Some(&requested), None, true);
                assert_eq!(placement.sandbox.process_tree_supervision, supervised);
            }
        }
    }
    assert_eq!(native.resources.cpu_millis, Some(250), "R1 cpu request");
    assert_eq!(
        native.resources.memory_bytes,
        Some(33_554_432),
        "R1 memory request"
    );
    assert!(
        native.sandbox.tool_transparent && native.sandbox.path_fidelity,
        "R1 Container paths"
    );

    let projected_acp = projected_acp_models();
    let acp = remote_worker_placement(&projected_acp, Some(&frozen), None, true);
    assert!(
        acp.sandbox.tool_transparent && acp.sandbox.path_fidelity,
        "R2"
    );

    let mut trusted = host_models();
    trusted.model_binding = ResolvedModelCandidate::try_backend_owned(
        ModelBinding::new("local", "", "acp:codex"),
        awaken_agent_contract::credential::CredentialRef {
            id: "local".into(),
            revision: 1,
        },
        BackendModelSelection::Default,
        "test",
        "sha256:test-capability",
        Default::default(),
    )
    .expect("coherent trusted ACP candidate");
    let mut workdir = frozen.clone();
    workdir.sandbox = Default::default();
    workdir.sandbox.host_access = awaken_provisioning_contract::HostAccessPolicy::TrustedHost;
    workdir.network = awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted;
    workdir.packages = Default::default();
    let trusted_resources = repository_manifest();
    let trusted = remote_worker_placement(&trusted, Some(&workdir), Some(&trusted_resources), true);
    assert_eq!(trusted.sandbox.isolation, IsolationClass::Workdir, "R3");
    assert!(!trusted.sandbox.tool_transparent, "R3");
    assert!(!trusted.sandbox.enforced_readonly, "R3 advisory access");
    assert_eq!(
        trusted.sandbox_backend.as_deref(),
        Some(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND),
        "R3 exact Worker backend"
    );

    let mut remote = host_models();
    remote.model_binding = ResolvedModelCandidate::try_remote(
        ModelBinding::new("agent", "", "a2a:https://agent.test"),
        "workspace",
        None,
        "security-fp",
    )
    .expect("coherent remote candidate");
    let remote_placement = remote_worker_placement(&remote, Some(&frozen), None, true);
    assert_eq!(remote_placement.sandbox, Default::default(), "R4");
    assert_eq!(
        remote_placement.resources,
        Default::default(),
        "R4 resources"
    );

    remote.model_candidates.push(host_models().model_binding);
    let mixed = remote_worker_placement(&remote, Some(&frozen), None, true);
    assert_eq!(mixed.sandbox.isolation, IsolationClass::Container, "R6");

    let mut prepared = frozen;
    prepared.prepared_image = Some("image@sha256:abc".into());
    let image = remote_worker_placement(&host_models(), Some(&prepared), None, true);
    assert!(image.sandbox.custom_rootfs, "R5 rootfs");
    assert!(!image.sandbox.package_provisioning, "R5 packages");
}

#[test]
fn session_path_causes_compile_into_worker_admission_once() {
    use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};

    // Cause/effect graph: C1 Environment asks for Workdir/Namespace; C2 the
    // frozen SessionResourceManifest has/has-not a typed Repository input;
    // C3 execution is cooperative Native/projected ACP; C4 the Worker
    // advertises path_fidelity=false/true while every unrelated capability
    // remains satisfied. E1 placement emits one SandboxRequirements vector;
    // E2 the exact Worker predicate accepts/rejects it. Repository presence
    // comes only from the frozen typed manifest, never from mount-path text.
    //
    // | Rule | Environment | Repository | Execution | Fidelity required |
    // |---|---|---|---|---|
    // | P1 | Workdir | no  | Native        | no  |
    // | P2 | Workdir | no  | projected ACP | yes |
    // | P3 | Workdir | yes | Native        | yes |
    // | P4 | Workdir | yes | projected ACP | yes |
    // | P5 | Namespace | no  | Native        | yes |
    // | P6 | Namespace | no  | projected ACP | yes |
    // | P7 | Namespace | yes | Native        | yes |
    // | P8 | Namespace | yes | projected ACP | yes |
    // For every P-row, C4=true accepts; C4=false accepts only P1 and rejects
    // P2-P8. This is the same `SandboxCapabilities::satisfies_requirements`
    // predicate consumed by Worker `can_claim`; no second admission table.
    for isolation in [IsolationClass::Workdir, IsolationClass::Namespace] {
        for has_repository in [false, true] {
            for projected_acp in [false, true] {
                let environment = environment_with_isolation(isolation);
                let models = if projected_acp {
                    projected_acp_models()
                } else {
                    host_models()
                };
                let repository = has_repository.then(repository_manifest);
                let placement =
                    remote_worker_placement(&models, Some(&environment), repository.as_ref(), true);
                let fidelity_required =
                    isolation >= IsolationClass::Namespace || has_repository || projected_acp;
                let expected_isolation = if fidelity_required {
                    isolation.max(IsolationClass::Namespace)
                } else {
                    isolation
                };
                assert_eq!(
                    placement.sandbox.isolation, expected_isolation,
                    "P isolation: {isolation:?}/{has_repository}/{projected_acp}"
                );
                assert_eq!(
                    placement.sandbox.tool_transparent, fidelity_required,
                    "P transparency: {isolation:?}/{has_repository}/{projected_acp}"
                );
                assert_eq!(
                    placement.sandbox.path_fidelity, fidelity_required,
                    "P fidelity: {isolation:?}/{has_repository}/{projected_acp}"
                );
                assert_eq!(
                    placement.sandbox.control_services.contains(
                        &awaken_sandbox_control::SandboxControlServiceKind::RepositoryGitCredential
                    ),
                    has_repository,
                    "Repository control service: {isolation:?}/{has_repository}/{projected_acp}"
                );

                for path_fidelity in [false, true] {
                    let worker = SandboxCapabilities {
                        process_tree_supervision: false,
                        isolation: IsolationClass::Namespace,
                        tool_transparent: true,
                        path_fidelity,
                        enforced_readonly: true,
                        network_isolation: true,
                        enforced_network_allowlist: true,
                        secret_egress_substitution: true,
                        resource_limits: true,
                        custom_rootfs: true,
                        package_provisioning: true,
                        control_services: placement.sandbox.control_services.clone(),
                    };
                    assert_eq!(
                        worker.satisfies_requirements(&placement.sandbox),
                        path_fidelity || !fidelity_required,
                        "Worker claim: {isolation:?}/{has_repository}/{projected_acp}/{path_fidelity}"
                    );
                }
            }
        }
    }
}

#[test]
fn hand_recovery_policy_compiles_into_the_existing_worker_requirement() {
    use awaken_tool_contract::tool::{ToolRecoveryMode, ToolRecoveryPolicy};

    // Cause/effect decision table: C1=a selected canonical Hand descriptor
    // freezes DurableRequest; C2=a non-Hand coordination descriptor has the
    // same policy. R1 !C1+C2 => no Sandbox demand, because coordination
    // recovery is owned by its host executor; R2 C1+C2 => exactly one
    // DurableRequest demand.
    // This derives placement from the immutable snapshot without a second
    // Agent field or a concrete tool-id list in the placement owner.
    let mut models = host_models();
    let builtins = awaken_ext_builtin_tools::builtin_tools();
    let hand = builtins
        .iter()
        .find(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Hand)
        .expect("Hand catalog is non-empty")
        .descriptor()
        .clone();
    let coordination = builtins
        .iter()
        .find(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Coordination)
        .expect("Coordination catalog is non-empty")
        .descriptor()
        .clone()
        .with_recovery(ToolRecoveryPolicy::durable_request());
    models.tool_descriptors = vec![hand.clone(), coordination.clone()];
    let coordination_only = remote_worker_placement(&models, None, None, true);
    assert!(
        coordination_only.required_sandbox_tool_recovery.is_empty(),
        "R1"
    );

    models.tool_descriptors = vec![
        hand.with_recovery(ToolRecoveryPolicy::durable_request()),
        coordination,
    ];
    let resident_hand = remote_worker_placement(&models, None, None, true);
    assert_eq!(
        resident_hand.required_sandbox_tool_recovery,
        [ToolRecoveryMode::DurableRequest].into_iter().collect(),
        "R2"
    );
}

#[tokio::test]
async fn exported_dispatch_completion_sink_is_the_host_owned_projection() {
    // Cause graph: one ExecutorComponents owns one raw completion relay; an embedding requests the Worker-settle projection;
    // cloning must preserve that exact sink rather than constructing a peer
    // notification path.
    // Decision table:
    // | host mode | exported sink | effect |
    // | local pool | exact host completion relay | local settle and export converge |
    // | coordinator-only | exact host completion relay | remote settle signals its foreground observer |
    // Source-equivalence rule: the removed alias getter and clone-only fixture
    // wrappers add no decisions; the canonical getter keeps the exact selected
    // allocation, and direct Arc clones retain the same test lifetimes.
    // Mode does not alter sink identity, so pointer identity covers both rules
    // without manufacturing two deployment configurations in this unit test.
    let host = (crate::SessionExecutorBuilder::ephemeral(
        Arc::new(NoModelConfiguredExecutor),
        UNCONFIGURED_MODEL_REF,
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let expected = host.completion.clone() as Arc<dyn CompletionSink>;
    let exported = host.worker_completion_sink();

    assert!(Arc::ptr_eq(&expected, &exported));
}

#[tokio::test]
async fn unconfigured_model_is_rejected_before_durable_enqueue() {
    use awaken_agent_contract::agent::{run::Id as AgentRunId, thread::Id as ThreadId};
    use awaken_runtime_contract::RunActivation;

    // Cause/effect rule: C0 the ordinary Dispatch runtime composition is
    // present and C1 its frozen model is explicitly unconfigured. E1 context
    // creation succeeds through C0; E2 dispatch rejects C1 before enqueue.
    let host = (crate::SessionExecutorBuilder::ephemeral(
        Arc::new(NoModelConfiguredExecutor),
        UNCONFIGURED_MODEL_REF,
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    let _managed = host.clone();
    let ctx = host
        .run_assembly
        .ctx_for("unconfigured-model", None)
        .await
        .expect("Session context");
    let activation = RunActivation::new(
        AgentRunId("run-unconfigured".into()),
        ThreadId("unconfigured-model".into()),
        ctx.config.clone(),
        Vec::new(),
    );

    let error = host
        .run_assembly
        .prepare_dispatch(activation)
        .fresh()
        .expect_err("an unconfigured model must never enter durable dispatch");
    assert!(error.to_string().contains("No model is configured"));
    assert!(error.to_string().contains("Author > Quickstart"));
}

#[test]
fn coordinator_admission_pins_protocol_and_every_materialization_capability() {
    let mut models = host_models();
    models.model_candidates.push(
        ResolvedModelCandidate::try_provider(
            ModelBinding::new("provider", "fallback", "native"),
            "provider@1",
            "route@1",
            "workspace",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: "fallback".into(),
                processing_placement: None,
            },
        )
        .expect("coherent fallback provider candidate"),
    );
    let placement = remote_worker_placement(&models, None, None, true);
    assert_eq!(placement.contract_version, 1);
    assert_eq!(placement.dispatch_contract_version, 1);
    assert_eq!(
        placement.runtime_protocol_version,
        awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION
    );
    assert!(placement.required_capabilities.contains("native-runtime"));
    assert!(
        placement
            .required_capabilities
            .contains(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY)
    );
    assert!(
        placement
            .required_capabilities
            .contains(HOST_EXECUTOR_CAPABILITY)
    );
}

#[test]
fn coordinator_admission_requires_exact_acp_and_generic_a2a_capabilities() {
    let mut models = host_models();
    let (provider, model, _) = models.model_binding.identity();
    let binding = ModelBinding::parse(provider, model, "acp:claude").unwrap();
    models.model_binding = ResolvedModelCandidate::host(binding);
    let remote = ResolvedModelCandidate::try_remote(
        ModelBinding::new("agent", "", "a2a:https://agent.example"),
        "workspace",
        None,
        "sha256:test-security",
    )
    .expect("coherent remote candidate");
    models.model_candidates.push(remote);

    let placement = remote_worker_placement(&models, None, None, true);
    assert!(
        placement.required_capabilities.contains("acp:claude"),
        "ACP CLI identity is part of claim compatibility"
    );
    assert!(
        placement
            .required_capabilities
            .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY),
        "the generic A2A transport is part of claim compatibility"
    );
    assert!(
        !placement.required_capabilities.contains("native-runtime"),
        "native is not advertised as a substitute for exact external backends"
    );
}

#[test]
fn worker_local_candidates_pin_every_exact_credential_for_claim_admission() {
    use awaken_agent_contract::credential::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage,
    };
    use awaken_runtime_contract::InferenceEndpoint;

    let worker_candidate = |model: &str, credential: &str, revision: u64| {
        ResolvedModelCandidate::try_provider(
            ModelBinding::new("provider", model, "genai"),
            "provider@1",
            "route@1",
            "workspace-a",
            Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: credential.into(),
                        revision,
                    },
                    CredentialMaterialSource::WorkerReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::self_hosted_provider(),
                )
                .with_target(
                    awaken_agent_contract::credential::CredentialTarget::new(
                        awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
                        "provider",
                    ),
                ),
            ),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://example.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent Worker-local provider candidate")
    };
    let mut models = host_models();
    models.model_binding = worker_candidate("primary", "cred:primary", 2);
    models
        .model_candidates
        .push(worker_candidate("fallback", "cred:fallback", 5));

    let placement = remote_worker_placement(&models, None, None, false);
    assert_eq!(
        placement.location,
        awaken_worker_contract::ExecutionLocation::RemoteRequired
    );
    assert!(
        placement
            .required_capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
    );
    assert_eq!(
        placement.required_credentials,
        std::collections::BTreeSet::from([
            awaken_agent_contract::credential::CredentialRef {
                id: "cred:fallback".into(),
                revision: 5,
            },
            awaken_agent_contract::credential::CredentialRef {
                id: "cred:primary".into(),
                revision: 2,
            },
        ])
    );
}

#[test]
fn backend_owned_candidate_uses_the_same_exact_worker_placement_fence() {
    // Cause graph: BackendOwned candidate -> WorkerLocal capability + exact
    // credential revision -> RemoteRequired placement -> existing heartbeat
    // selection and pre-launch revalidation. There is no materialization cause.
    //
    // Decision table: BackendOwned always requires its one exact observation;
    // HostExecutor requires neither this capability nor credential revision.
    let mut models = host_models();
    models.model_binding = ResolvedModelCandidate::try_backend_owned(
        ModelBinding::new("cred:local", "", "acp:codex"),
        awaken_agent_contract::credential::CredentialRef {
            id: "cred:local".into(),
            revision: 7,
        },
        awaken_runtime_contract::resolved::BackendModelSelection::Default,
        "test",
        "sha256:test-capability",
        Default::default(),
    )
    .expect("coherent backend-owned candidate");

    let placement = remote_worker_placement(&models, None, None, false);
    assert_eq!(
        placement.location,
        awaken_worker_contract::ExecutionLocation::RemoteRequired
    );
    assert!(
        placement
            .required_capabilities
            .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY)
    );
    assert_eq!(
        placement.required_credentials,
        std::collections::BTreeSet::from([awaken_agent_contract::credential::CredentialRef {
            id: "cred:local".into(),
            revision: 7,
        },])
    );
    assert_eq!(
        placement.required_acp_capabilities,
        std::collections::BTreeSet::from([
            awaken_worker_contract::WorkerAcpCapabilityRequirement {
                backend_ref: "acp:codex".into(),
                fingerprint: "sha256:test-capability".into(),
            },
        ])
    );
}

#[test]
fn resource_placement_requires_resource_and_repository_credential_capabilities() {
    // Cause/effect graph: C1=resource manifest exists; C2=repository has a
    // credential; C3=input is read-only. Effects: E1=resource realization
    // capability; E2=repository credential capability; E3=enforced read-only
    // Sandbox capability. One row with C1+C2+C3 covers all conjunctive effects;
    // the following empty-manifest test owns the !C2/!C3 revocation row.
    let resources = repository_manifest();
    let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
    assert!(
        placement
            .required_capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY)
    );
    assert!(
        placement
            .required_capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
    assert!(placement.sandbox.enforced_readonly);
}

#[test]
fn explicit_empty_manifest_still_requires_the_revocation_capability() {
    let resources = crate::session_resource::SessionResourceManifest::new(
        "workspace-a",
        crate::session_resource::ResolvedSessionResources::default(),
    );
    let placement = remote_worker_placement(&host_models(), None, Some(&resources), true);
    assert!(
        placement
            .required_capabilities
            .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
        "an empty successor manifest may need to remove prior sandbox material"
    );
    assert!(
        !placement
            .required_capabilities
            .contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY)
    );
}
