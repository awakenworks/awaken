use super::*;
use crate::host::ExecutorComponents;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
use awaken_sandbox_container::ContainerHandleCodec as _;
use awaken_sandbox_container::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
use awaken_sandbox_local::LocalProvider;
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization::peers::EnvironmentPeer as _;
use awaken_session_execution_contract::TerminalCleanupRuntime as _;

/// The logical path a staged resource realizes under, recovered from a projected
/// pc mount (`.mnt/<logical>`) so the registry assertions stay resource-oriented.
fn logical_of(m: &pc::MountRequirement) -> &str {
    m.mount_path.strip_prefix(".mnt/").unwrap_or(&m.mount_path)
}

/// Whether a projected Workdir spec denies tool egress.
fn denies(spec: &pc::SandboxSpec) -> bool {
    spec.deny_tool_egress
}

#[test]
fn repository_binding_failure_controls_host_retry_semantics() {
    // Cause/effect table: malformed/denied binding -> caller BadRequest; a
    // concurrently changed authority -> Conflict (409, retried within the
    // budget); authority outage or unknown read -> Unavailable; corrupt
    // authority response -> Internal with its own closed code (terminal). The
    // Host boundary enters through the one realization classifier, so it
    // agrees with the Run boundary's table.
    // R1..R6 cover the complete verifier enum through the sole classifier;
    // kind, literal code/message and Display are observable effects.
    // There is no lock, IO, state or poison precondition in this pure mapping.
    use crate::host::HostErrorKind as K;
    use awaken_resource_contract::RepositoryBindingVerifierError as E;
    let cases = [
        (
            E::InvalidRequest { detail: "x".into() },
            K::BadRequest,
            "invalid_request",
            "invalid Repository binding request: x",
        ),
        (
            E::Denied { detail: "x".into() },
            K::BadRequest,
            "invalid_request",
            "Repository binding was denied: x",
        ),
        (
            E::Conflict { detail: "x".into() },
            K::Conflict,
            "conflict",
            "Repository binding authority changed concurrently: x",
        ),
        (
            E::AuthorityUnavailable { detail: "x".into() },
            K::Unavailable,
            "unavailable",
            "Repository binding authority is unavailable: x",
        ),
        (
            E::Indeterminate { detail: "x".into() },
            K::Unavailable,
            "unavailable",
            "Repository binding decision is indeterminate: x",
        ),
        (
            E::CorruptAuthority { detail: "x".into() },
            K::Internal,
            "repository_binding_authority_corrupt",
            "Repository binding authority returned corrupt data: x",
        ),
    ];
    for (error, expected, code, message) in cases {
        let actual =
            crate::HostError::from(crate::realization_error::RealizationError::from(error));
        assert_eq!(actual.kind, expected);
        assert_eq!(actual.code, code);
        assert_eq!(actual.message, message);
        assert_eq!(actual.to_string(), message);
    }
}

struct NoLlm;
#[async_trait::async_trait]
impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        unreachable!("provisioning bookkeeping never calls the model")
    }
}

async fn dependencies() -> crate::HostDependencies {
    crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await
}

async fn host() -> Arc<ExecutorComponents> {
    (dependencies().await)
        .build()
        .expect("validated Host dependencies")
}

#[path = "tests/artifact_harvest.rs"]
mod artifact_harvest;
#[path = "tests/file_fence.rs"]
mod file_fence;

#[derive(Default)]
struct RecordingPublicationRealizer {
    calls: std::sync::Mutex<usize>,
    mismatched_receipt: bool,
}

#[async_trait::async_trait]
impl pc::RepositoryRealizer for RecordingPublicationRealizer {
    async fn realize_repository(
        &self,
        _plan: &pc::RepositoryRealizationPlan,
        _credential: Option<&pc::RepositoryHttpBasicCredential>,

        _intent: awaken_provisioning_contract::RepositoryRealizationIntent,
    ) -> Result<(), pc::SandboxError> {
        unreachable!("publication helper never realizes a repository")
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        expectation: &pc::RepositoryPublicationExpectation,
        _credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
        *self.calls.lock().unwrap() += 1;
        let mut receipt = pc::RepositoryPublicationReceipt::new(plan, expectation);
        if self.mismatched_receipt {
            receipt.repository_id.push_str("-wrong");
        }
        Ok(receipt)
    }
}

fn terminal_cleanup_test_lease() -> session_resource::SessionRealizationLease {
    session_resource::SessionRealizationLease {
        owner: "terminal-provisioning-test".into(),
        runtime_incarnation: "terminal-provisioning-test/incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_session_realization::test_support::wall_clock_now_ms() + 60_000,
    }
}

fn terminal_cleanup_projection(
    workspace_id: &str,
    environment: awaken_session_state_contract::SessionEnvironmentState,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    frozen_projection_with_model_override(workspace_id, environment, None)
}

fn frozen_projection_with_model_override(
    workspace_id: &str,
    environment: awaken_session_state_contract::SessionEnvironmentState,
    model_override: Option<awaken_session_configuration_contract::SessionModelOverride>,
) -> awaken_session_realization_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "terminal.provisioning.test",
    );
    let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "terminal-test-environment".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "terminal-test-environment-v1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_credential_contract::CredentialRealizationProfile {
                    inference_holder: holder.clone(),
                    mcp_holder: holder.clone(),
                    resource_holder: holder,
                },
            },
            runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
            mcp_authoring: Default::default(),
            agent_id: "terminal-test-agent".into(),
            agent_revision: None,
            model_override,
            model: "terminal-test-model".into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    awaken_session_realization_contract::FrozenSessionProjection {
        workspace_id: workspace_id.into(),
        revision: awaken_session_types::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment_materialization: None,
        environment,
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(session_resource::SessionResourceManifest::new(
            workspace_id,
            session_resource::ResolvedSessionResources::default(),
        )),
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    }
}

/// Install the complete frozen projection before a terminal-artifact fixture
/// creates its resident Environment. Causes: P1 the process-local slot is
/// absent and P2 the physical Sandbox has not been created. Effects: E1 the
/// canonical projection owner installs baseline, Workspace, Environment
/// expectation, and exact revision-zero Resource transition; E2 later Sandbox
/// creation cannot precede those frozen facts. Decision rule P1+P2 => E1+E2.
async fn install_terminal_artifact_projection_before_environment(
    host: &ExecutorComponents,
    session_id: &str,
    workspace_id: &str,
) {
    host.run_assembly
        .install_frozen_session_projection(
            session_id,
            terminal_cleanup_projection(workspace_id, Default::default()),
            None,
            true,
            None,
        )
        .await
        .expect("terminal artifact fixture installs its complete frozen projection first");
}

/// Publish a legacy durable binding through the same projection/adoption owner
/// that terminal assignment installation later replays. Causes: F1 the frozen
/// slot is Unmaterialized; F2 one exact physical Environment exists; F3 the
/// historical aggregate state has binding evidence but no generated V2
/// generation. Effects: E1 project the canonical DurableBinding identity; E2
/// adopt and publish that exact Arc once. Rule F1+F2+F3=>E1+E2. A direct-only
/// test receipt is not interchangeable with aggregate durable truth.
fn install_terminal_artifact_environment(
    host: &ExecutorComponents,
    session_id: &str,
    workspace_id: &str,
    environment: Arc<crate::session_environment::SessionEnvironment>,
) {
    let binding = serde_json::to_string(&environment.lifecycle().handle())
        .expect("terminal artifact Environment binding");
    let state = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    host.run_assembly
        .install_session_environment_owner_projection(session_id, workspace_id, &state)
        .expect("F1/E1 project the canonical legacy durable binding");
    let candidate = host
        .realization_executor
        .environments()
        .begin_session_environment_adoption(
            host.realization_executor.peers(),
            session_id,
            environment,
            None,
        )
        .expect("F2/E2 retain the exact adoption candidate");
    let crate::environment_owner::UnboundSessionEnvironmentOrigin::AdoptionFrom(identity) =
        &candidate.origin
    else {
        panic!("F3 aggregate projection must own the durable source identity");
    };
    host.realization_executor
        .environments()
        .publish_prepared_session_environment(session_id, &candidate, identity.clone())
        .expect("F2/E2 publish the exact durable owner");
}

async fn root_terminal_cleanup_effect(
    host: &ExecutorComponents,
    session_id: &str,
    lease: session_resource::SessionRealizationLease,
) -> (
    awaken_session_state_contract::PersistedSession,
    awaken_session_state_contract::SessionTerminalCleanupEffect,
) {
    let environment = host
        .run_assembly
        .installed_sessions
        .realizations()
        .read_realization(session_id, |r| r.environment().owner.resident())
        .unwrap()
        .flatten()
        .expect("terminal test requires a resident Environment");
    let binding = serde_json::to_string(&environment.lifecycle().handle()).unwrap();
    let workspace_id = host
        .run_assembly
        .installed_sessions
        .realizations()
        .frozen(session_id)
        .unwrap()
        .workspace
        .clone()
        .expect("terminal test requires a root Workspace projection");
    let projection = terminal_cleanup_projection(
        &workspace_id,
        awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding,
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    );
    // Terminal Resident is a successor of the initially unmaterialized root.
    let mut projection = projection;
    projection.revision.0 += 1;
    let mut session = awaken_session_state_contract::PersistedSession::frozen_with_budget(
        session_id,
        projection.baseline.clone(),
        awaken_session_state_contract::SessionResourceState::from_active(
            projection.resources.clone(),
        ),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    session.environment = projection.environment.clone();
    session.realization = Some(lease.clone());
    session.execution = awaken_session_state_contract::SessionExecutionState::Terminated;
    assert!(session.ensure_terminal_cleanup_fence());
    session
        .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
        .unwrap();
    let command = session
        .terminal_cleanup
        .command_for(
            &awaken_session_types::SessionId::from(session_id),
            session_id,
        )
        .unwrap();
    host.run_assembly
        .install_test_terminal_cleanup_projection(
            &awaken_session_realization_contract::SessionTerminalCleanupAssignment {
                environment_creation_not_attempted: false,
                session_id: session_id.into(),
                projection,
                authority: awaken_session_realization::test_support::test_lease_receipt(
                    lease.clone(),
                ),
            },
        )
        .await
        .unwrap();
    let effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(command, lease);
    (session, effect)
}

fn root_terminal_preparation_authorization(
    session: &awaken_session_state_contract::PersistedSession,
    effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    workspace_id: &str,
) -> awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization {
    let inherited_provider_disposal = session
        .authorize_terminal_cleanup_effect(effect)
        .expect("derive the root preparation authorization from the aggregate");
    awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
        effect.clone(),
        workspace_id.into(),
        inherited_provider_disposal,
    )
    .expect("close the exact root terminal preparation authorization")
    .with_source_dispatch(
        awaken_session_realization_contract::SessionTerminalSourceDispatch::Settled {},
    )
}

#[tokio::test]
async fn session_and_housekeeping_filesystem_continuity_are_distinct() {
    /* Continuity cause/effect table.
     * Causes: C1 the spec realizes the canonical Session environment; C2
     * the spec realizes a disposable child/probe environment. Effects: E1
     * request retained writable state for DurableRequest recovery; E2
     * request ephemeral state and therefore no configured continuation PVC.
     * Rules: SC1 C1=>E1; SC2 C2=>E2. The typed field participates in the
     * capacity identity, so the two requests cannot share warm capacity.
     */
    assert_eq!(
        host()
            .await
            .realization_executor
            .sandbox_spec("session")
            .filesystem_continuity,
        pc::FilesystemContinuity::Retained,
        "SC1"
    );
    assert_eq!(
        agent_run_sandbox_spec("probe").filesystem_continuity,
        pc::FilesystemContinuity::Ephemeral,
        "SC2"
    );
    assert_ne!(
        pc::SandboxCapacityShapeId::from_spec(
            &host().await.realization_executor.sandbox_spec("session")
        ),
        pc::SandboxCapacityShapeId::from_spec(&agent_run_sandbox_spec("probe")),
        "SC1/SC2"
    );
}

/// A resource mount realized read-only under `.mnt/<logical>`.
fn resource_mount(logical: &str) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: format!("id-{logical}"),
        source: pc::MountSource::InlineBytes {
            contents: format!("content of {logical}").into_bytes(),
            content_hash: None,
        },
        mount_path: format!(".mnt/{logical}"),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    }
}

#[tokio::test]
async fn projected_sandbox_layout_preserves_one_complete_input_snapshot() {
    // Cause/effect graph for the private projected-layout command: C1 an
    // ordinary Resource mount is present; C2 the frozen baseline contributes a
    // second mount and one env value; C3 SemanticTools suppresses only the
    // baseline MemoryStore mount; C4 the frozen Environment denies network but
    // the selected provider cannot enforce network isolation; C5 a Repository
    // path is present. Effects: E1 the non-Memory mounts are merged exactly
    // once; E2 the baseline env is retained; E3 tool egress remains denied while
    // the provider-visible network falls back to unrestricted; E4 Repository
    // path fidelity raises the effective isolation to Namespace. Constraint:
    // every cause is supplied in one ephemeral projection input and the helper
    // owns no retained Session or Sandbox authority. Decision rule L1:
    // C1+C2+C3+C4+C5 => E1+E2+E3+E4.
    let host = host().await;
    let resource = resource_mount("resource.txt");
    let baseline = resource_mount("baseline.txt");
    let memory = pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "memory".into(),
            materialization_reference: None,
            write_consistency: Default::default(),
        },
        mount_path: ".mnt/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    };
    let baseline_mounts = [baseline.clone(), memory];
    let baseline_env = [pc::EnvVar {
        name: "PROJECT".into(),
        value: pc::EnvValue::Inline {
            value: "awaken".into(),
        },
        visibility: pc::EnvVisibility::Process,
    }];
    let mut environment = frozen_projection_with_model_override(
        "workspace",
        awaken_session_state_contract::SessionEnvironmentState::default(),
        None,
    )
    .baseline
    .environment;
    environment.network = awaken_session_configuration_contract::SessionNetworkPolicy::None;
    let environment = project_environment(&environment);

    let spec = host
        .realization_executor
        .staging()
        .sandbox_spec_for_projected_layout(
            "complete-layout",
            ProjectedSandboxLayout {
                resource_mounts: vec![resource.clone()],
                has_repositories: true,
                baseline_mounts: Some(&baseline_mounts),
                baseline_env: Some(&baseline_env),
                content_delivery: Some(crate::frozen_inputs::ManagedContentDelivery::SemanticTools),
                environment: Some(&environment),
                network_isolation: false,
            },
        )
        .unwrap();

    assert_eq!(spec.mounts, vec![resource, baseline], "L1/E1");
    assert_eq!(spec.env, baseline_env, "L1/E2");
    assert!(spec.deny_tool_egress, "L1/E3");
    assert_eq!(spec.network, pc::NetworkPolicy::Unrestricted, "L1/E3");
    assert_eq!(spec.isolation, pc::IsolationClass::Namespace, "L1/E4");
}

#[tokio::test]
async fn checkpoint_memory_projection_is_typed_strict_and_replay_stable() {
    // Cause/effect graph: C1 retention is Resident/CheckpointAndRelease; C2 a
    // Memory input is RO/RW; C3 projection is effectful fresh materialization or
    // pure cold adoption; C4 a File is a late-attached Resource; C5 the remote
    // Run materialization reference is present/absent; C6 no Worker upstream.
    // Effects: E1 ProviderDefault;
    // E2 WriteThroughRequired; E3 only create-time Memory enters the substrate;
    // E4 fresh/cold immutable fingerprints agree. Decision rules:
    // | Rule | C1 checkpoint | C2 RW | C3 cold | C4 File | C5 Run ref | Effect |
    // | M1   | no            | yes   | any     | any     | any        | E1     |
    // | M2   | yes           | no    | any     | any     | any        | E1     |
    // | M3   | yes           | yes   | no      | yes     | yes        | E2+E3 |
    // | M4   | yes           | yes   | yes     | yes     | no         | E2+E3+E4 |
    // M5 C6 => local validator and no remote materialization reference.
    // Constraint: the Environment policy is the only consistency cause; File
    // bytes remain governed by Resource reconciliation and never re-enter the
    // provider substrate fingerprint during adoption.
    fn memory_mount(access: pc::MountAccess, reference: Option<&str>) -> pc::MountRequirement {
        pc::MountRequirement {
            mount_id: "memory-binding".into(),
            source: pc::MountSource::MemoryStore {
                store_id: "memory-store".into(),
                materialization_reference: reference.map(str::to_owned),
                write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
            },
            mount_path: "/mnt/memory".into(),
            access,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }
    }

    fn consistency(spec: &pc::SandboxSpec) -> pc::MemoryWriteConsistency {
        let pc::MountSource::MemoryStore {
            write_consistency, ..
        } = &spec.mounts[0].source
        else {
            panic!("expected one typed Memory substrate mount")
        };
        *write_consistency
    }

    let host = (dependencies().await.with_session_execution(
        crate::SessionExecutionDependencies::default()
            .with_resource_validator(crate::host::test_resource_validator()),
    ))
    .build()
    .unwrap();
    let mut environment = frozen_projection_with_model_override(
        "workspace",
        awaken_session_state_contract::SessionEnvironmentState::default(),
        None,
    )
    .baseline
    .environment;
    environment.idle_retention = awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy {
        mode: awaken_session_configuration_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease,
        checkpoint_after_secs: 1,
        retention_secs: 60,
        expiry_behavior: Default::default(),
        max_checkpoint_bytes: 1_024,
        max_checkpoint_duration_secs: 30,
        checkpoint_format: "awaken-fs-tar-v1".into(),
    };
    host.run_assembly
        .install_environment_projection("checkpoint-memory", &environment)
        .expect("install exact frozen Environment");

    let file_mount = pc::MountRequirement {
        mount_id: "file-a".into(),
        source: pc::MountSource::InlineBytes {
            contents: b"immutable file".to_vec(),
            content_hash: Some(awaken_resource_contract::content_id(b"immutable file")),
        },
        mount_path: "/mnt/session/uploads/file-a".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    };
    host.realization_executor
        .staging()
        .register_thread_resources(
            "checkpoint-memory",
            StagedResources {
                mounts: vec![
                    file_mount,
                    memory_mount(pc::MountAccess::ReadWrite, Some("run-v1-reference")),
                ],
                ..Default::default()
            },
        );
    let fresh = host
        .realization_executor
        .staging()
        .sandbox_substrate_spec_for_provider(
            "checkpoint-memory",
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
        )
        .unwrap();
    assert_eq!(fresh.mounts.len(), 1, "M3/E3");
    assert_eq!(
        consistency(&fresh),
        pc::MemoryWriteConsistency::WriteThroughRequired,
        "M3/E2"
    );

    let resolved = session_resource::ResolvedSessionResources::try_new(
        vec![
            session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("file-binding"),
                source: session_resource::ResolvedInputSource::File {
                    file_id: awaken_resource_contract::FileId::from("file-a"),
                },
                mount_path: "/file-a".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            },
            session_resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("memory-binding"),
                source: session_resource::ResolvedInputSource::MemoryStore {
                    memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-store"),
                    config: awaken_resource_contract::MemoryStoreConfigVersion {
                        memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                            "memory-store",
                        ),
                        version: awaken_resource_contract::ConfigVersion(1),
                        retention_policy: Default::default(),
                    },
                },
                mount_path: "/memory".into(),
                access: awaken_resource_contract::ResourceAccess::ReadWrite,
                instructions: None,
            },
        ],
        Vec::new(),
    )
    .expect("resolved File+Memory manifest");
    let strict_projection = project_environment(&environment);
    let effectful = host
        .realization_executor
        .staging()
        .stage_resolved_input(
            "workspace",
            &resolved.inputs()[1],
            None,
            None,
            Some(&strict_projection),
        )
        .await
        .expect("effectful Memory staging");
    let pc::MountSource::MemoryStore {
        write_consistency,
        materialization_reference,
        ..
    } = &effectful.mounts[0].source
    else {
        panic!("effectful staging must preserve typed Memory")
    };
    assert_eq!(
        *write_consistency,
        pc::MemoryWriteConsistency::WriteThroughRequired,
        "M3/E2 effectful"
    );
    assert!(
        materialization_reference.is_none(),
        "M3 local no-upstream staging uses its binding validator, not a remote reference"
    );
    let cold = host
        .realization_executor
        .staging()
        .sandbox_spec_for_resolved_resources_and_provider(
            "checkpoint-memory",
            &resolved,
            &host
                .realization_executor
                .environments()
                .providers()
                .session_provider,
        )
        .unwrap();
    assert_eq!(cold.mounts.len(), 1, "M4/E3");
    assert_eq!(
        consistency(&cold),
        pc::MemoryWriteConsistency::WriteThroughRequired,
        "M4/E2"
    );
    assert_eq!(
        pc::SandboxRealizationFingerprint::from_spec(&fresh),
        pc::SandboxRealizationFingerprint::from_spec(&cold),
        "M4/E4"
    );

    let readonly = host
        .realization_executor
        .staging()
        .sandbox_spec_for_projected_layout(
            "readonly-memory",
            ProjectedSandboxLayout {
                resource_mounts: vec![memory_mount(pc::MountAccess::ReadOnly, None)],
                has_repositories: false,
                baseline_mounts: None,
                baseline_env: None,
                content_delivery: None,
                environment: Some(&strict_projection),
                network_isolation: true,
            },
        )
        .unwrap();
    assert_eq!(
        consistency(&readonly),
        pc::MemoryWriteConsistency::ProviderDefault,
        "M2/E1"
    );

    let mut resident_environment = environment;
    resident_environment.idle_retention = Default::default();
    let resident_projection = project_environment(&resident_environment);
    let resident = host
        .realization_executor
        .staging()
        .sandbox_spec_for_projected_layout(
            "resident-memory",
            ProjectedSandboxLayout {
                resource_mounts: vec![memory_mount(pc::MountAccess::ReadWrite, None)],
                has_repositories: false,
                baseline_mounts: None,
                baseline_env: None,
                content_delivery: None,
                environment: Some(&resident_projection),
                network_isolation: true,
            },
        )
        .unwrap();
    assert_eq!(
        consistency(&resident),
        pc::MemoryWriteConsistency::ProviderDefault,
        "M1/E1"
    );
}

fn repository_activation(logical: &str) -> RepositoryActivation {
    RepositoryActivation {
        plan: pc::RepositoryRealizationPlan {
            repository_id: format!("id-{logical}"),
            mount_path: format!("/workspace/{logical}"),
            source_remote_url: "https://example.invalid/x.git".to_string(),
            transport_url: "https://example.invalid/x.git".to_string(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        },
        credential_pin: None,
    }
}

#[tokio::test]
async fn register_replaces_the_threads_staged_set() {
    let host = host().await;
    host.realization_executor
        .staging()
        .register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("a.md"), resource_mount("b.md")],
                prompts: vec!["first".into()],
                resource_prompts: Vec::new(),
                binding_checks: Vec::new(),
                repositories: vec![repository_activation("repo-a")],
            },
        );
    // A second register REPLACES (correct at create time, before any first Run).
    host.realization_executor
        .staging()
        .register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("c.md")],
                prompts: vec!["second".into()],
                resource_prompts: Vec::new(),
                binding_checks: Vec::new(),
                repositories: Vec::new(),
            },
        );

    assert_eq!(
        host.realization_executor.sandbox_spec("t").mounts.len(),
        1,
        "old mounts dropped"
    );
    assert_eq!(
        host.run_assembly.thread_session_prompts("t"),
        vec!["second".to_string()]
    );
    assert!(
        host.realization_executor
            .staging()
            .thread_repository_activations("t")
            .is_empty(),
        "old repository activation dropped"
    );
}

#[tokio::test]
async fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
    // Cause graph: a frozen restriction plus a provider without strict network
    // isolation selects the existing Workdir wrapper, not an unsupported
    // admission requirement.
    // | Rule | restriction | strict provider | network | deny wrapper |
    // | W1 | absent | no | unrestricted | no |
    // | W2 | none | no | unrestricted | yes |
    let host = host().await;
    // No registration and no egress: shared network, no mounts.
    let bare = host.realization_executor.sandbox_spec("t");
    assert!(!denies(&bare));
    assert!(bare.mounts.is_empty());

    host.run_assembly
        .install_environment_projection(
            "t",
            &awaken_session_configuration_contract::EnvironmentSnapshot {
                environment_id: "environment".into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                    "environment-1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_configuration_contract::SessionNetworkPolicy::None,
                credential_realization:
                    awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            },
        )
        .expect("freeze test Environment");
    host.realization_executor
        .staging()
        .register_thread_resources(
            "t",
            StagedResources {
                mounts: vec![resource_mount("notes.md")],
                ..Default::default()
            },
        );
    let spec = host.realization_executor.sandbox_spec("t");
    assert!(denies(&spec), "the thread's deny-egress policy is carried");
    assert_eq!(
        spec.network,
        pc::NetworkPolicy::Unrestricted,
        "Workdir does not claim strict network isolation in admission"
    );
    assert_eq!(spec.mounts.len(), 1);
    assert_eq!(logical_of(&spec.mounts[0]), "notes.md");
}

#[tokio::test]
async fn repository_workspace_rejects_a_provider_with_split_tool_and_process_paths() {
    // Cause/effect graph: C1 Session has/has-not a Repository; C2 provider
    // has/has-not tool transparency plus sandbox-absolute path fidelity.
    // Effects: E1 repository demand is promoted to Namespace requirements;
    // E2 a Workdir provider is rejected before environment creation/clone;
    // E3 a no-repository Workdir Session retains its supported local tier.
    //
    // | Rule | Repository | Provider path contract | Effect |
    // |---|---|---|---|
    // | W1 | yes | split Workdir paths | E1+E2 |
    // | W2 | no | split Workdir paths | E3 |
    // | W3 | yes | transparent/fidelitous | admitted (covered by the real Namespace Hand test) |
    // Constraint/invariant: SandboxSpec plus provider capabilities are the
    // single admission authority; no Bash command rewriting or alias mount
    // is introduced as a second path mapping.
    let host = host().await;
    let local = awaken_session_environment::SandboxEnvironmentProvider::workdir(
        tempfile::tempdir().unwrap().path(),
    )
    .into_session_provider();
    let bare = host.realization_executor.sandbox_spec("bare");
    assert_eq!(bare.isolation, pc::IsolationClass::Workdir, "W2/E3");

    host.realization_executor
        .staging()
        .register_thread_resources(
            "repository-session",
            StagedResources {
                repositories: vec![repository_activation("repo")],
                ..Default::default()
            },
        );
    let repository_spec = host.realization_executor.sandbox_spec("repository-session");
    assert_eq!(
        repository_spec.isolation,
        pc::IsolationClass::Namespace,
        "W1/E1"
    );
    let error = match host
        .realization_executor
        .environments()
        .create_session_environment(host.realization_executor.peers(), &local, &repository_spec)
        .await
    {
        Ok(_) => panic!("W1/E2 Workdir cannot offer one /workspace path"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("backend isolation is weaker than requested"),
        "W1/E2: {}",
        error
    );
}

#[tokio::test]
async fn trusted_host_repository_projection_preserves_the_exact_tier() {
    // Cause/effect rules: T1 explicit TrustedHost + Repository => Workdir;
    // I1 isolated + Repository => Namespace; T2/I2 without Repository => Workdir.
    // The frozen Environment is the authority; presence of a Repository must
    // not silently change a trusted machine identity into an isolated process.
    let host = host().await;
    for trusted in [false, true] {
        for repository in [false, true] {
            let mut environment = frozen_projection_with_model_override(
                "workspace",
                awaken_session_state_contract::SessionEnvironmentState::default(),
                None,
            )
            .baseline
            .environment;
            environment.sandbox.isolation = Some(pc::IsolationClass::Workdir);
            environment.sandbox.host_access = if trusted {
                pc::HostAccessPolicy::TrustedHost
            } else {
                pc::HostAccessPolicy::Isolated
            };
            let environment = project_environment(&environment);
            let spec = host
                .realization_executor
                .staging()
                .sandbox_spec_for_projected_layout(
                    "trust-matrix",
                    ProjectedSandboxLayout {
                        resource_mounts: Vec::new(),
                        has_repositories: repository,
                        baseline_mounts: None,
                        baseline_env: None,
                        content_delivery: None,
                        environment: Some(&environment),
                        network_isolation: false,
                    },
                )
                .unwrap();
            assert_eq!(
                spec.isolation,
                if repository && !trusted {
                    pc::IsolationClass::Namespace
                } else {
                    pc::IsolationClass::Workdir
                },
                "trusted={trusted}, repository={repository}"
            );
        }
    }
}

#[tokio::test]
async fn projected_acp_rejects_a_provider_with_split_tool_and_process_paths() {
    // Cause/effect graph: C1 the immutable Agent publication contains/does not
    // contain an ACP candidate whose process is projected into the Session
    // Sandbox; C2 the provider does/does not preserve one sandbox-absolute path
    // for arbitrary processes. E1 the exact published candidate set is the only
    // opaque-process cause; E2 a split-path Workdir provider is rejected before
    // environment creation; E3 cooperative Native execution keeps Workdir.
    //
    // | Rule | Published execution | Provider path contract | Effect |
    // |---|---|---|---|
    // | A1 | projected ACP | split Workdir paths | rejected before create |
    // | A2 | Native | split Workdir paths | admitted (covered by W2 above) |
    // | A3 | projected ACP | transparent/fidelitous | admitted (Namespace provider suite) |
    // Constraint: Repository absence must not mask the independent opaque-process
    // cause, and realization must consume the same candidate predicate as Worker
    // placement instead of reconstructing it from backend text.
    let host = host().await;
    let publication = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "acp:test",
                ),
            ),
        )
        .build();
    host.realization_executor
        .environments()
        .publication()
        .retain_session_publication("projected-acp", Some(&publication))
        .expect("install exact immutable Agent publication");

    let root = tempfile::tempdir().unwrap();
    let local = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let spec = host.realization_executor.sandbox_spec("projected-acp");
    assert_eq!(
        spec.isolation,
        pc::IsolationClass::Workdir,
        "A1 precondition"
    );
    let error = match host
        .realization_executor
        .environments()
        .create_session_environment(host.realization_executor.peers(), &local, &spec)
        .await
    {
        Ok(_) => panic!("A1/E2 projected ACP cannot run with split paths"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("one sandbox-absolute workspace path"),
        "A1/E2: {}",
        error
    );
}

#[tokio::test]
async fn model_override_only_acp_candidate_rejects_split_process_paths() {
    // Cause/effect graph: C1 the frozen baseline has a complete model-override
    // publication; C2 no Agent publication is retained; C3 ACP appears only as
    // a fallback candidate, not the primary. E1 the complete override route is
    // the effective frozen model set; E2 every candidate contributes its opaque
    // process requirement; E3 Workdir is rejected before environment creation.
    //
    // | Rule | Override publication | Agent publication | ACP location | Effect |
    // |---|---|---|---|---|
    // | M1 | complete | absent | fallback | E1+E2+E3 |
    // | M2 | complete | present | any override candidate | override wins (canonical resolver) |
    // | M3 | absent | present | Agent candidate | Agent set wins (A1 above) |
    // Constraint: the override is durable baseline truth. Realization must not
    // substitute a mutable catalog lookup or collapse an absent Agent snapshot
    // into a Native/non-opaque default.
    let host = host().await;
    let model_override = awaken_session_configuration_contract::SessionModelOverride {
        publication: Some(Box::new(
            awaken_session_configuration_contract::SessionModelPublication {
                primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    awaken_runtime_contract::resolved::ModelBinding::new(
                        "provider", "primary", "native",
                    ),
                ),
                candidates: vec![
                    awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        awaken_runtime_contract::resolved::ModelBinding::new(
                            "provider", "fallback", "acp:test",
                        ),
                    ),
                ],
            },
        )),
        inference: Default::default(),
    };
    host.run_assembly
        .install_frozen_session_projection(
            "model-override-acp",
            frozen_projection_with_model_override(
                "test",
                awaken_session_state_contract::SessionEnvironmentState::default(),
                Some(model_override),
            ),
            None,
            true,
            None,
        )
        .await
        .expect("install exact model-override-only frozen projection");
    assert!(
        host.run_assembly
            .installed_sessions
            .read("model-override-acp", |slot| slot
                .inputs
                .published_snapshot
                .is_none())
            .expect("installed Sessions")
            .unwrap_or(false),
        "M1/C2"
    );

    let root = tempfile::tempdir().unwrap();
    let local = awaken_session_environment::SandboxEnvironmentProvider::workdir(root.path())
        .into_session_provider();
    let spec = host.realization_executor.sandbox_spec("model-override-acp");
    assert_eq!(
        spec.isolation,
        pc::IsolationClass::Workdir,
        "M1 precondition"
    );
    let error = match host
        .realization_executor
        .environments()
        .create_session_environment(host.realization_executor.peers(), &local, &spec)
        .await
    {
        Ok(_) => panic!("M1/E3 override ACP fallback cannot run with split paths"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("one sandbox-absolute workspace path"),
        "M1/E3: {}",
        error
    );
}

#[tokio::test]
async fn realize_thread_repositories_fails_closed_on_an_unsafe_path() {
    // Managed repository Skill cause/effect rule R9. C1 the frozen mount is
    // unsafe (the provider rejects it before Git) or C2 repository
    // realization returns an error. E1 Session activation fails before
    // Skill-root selection, snapshot, or prompt publication. This existing
    // realization boundary is the only clone/fetch owner; Skill discovery
    // never adds another transport path. Decision rows R9a=C1=>E1 and
    // R9b=C2=>E1 exercise both causes through that same boundary.
    struct FailingRepositoryRealizer;

    #[async_trait::async_trait]
    impl pc::RepositoryRealizer for FailingRepositoryRealizer {
        async fn realize_repository(
            &self,
            _plan: &pc::RepositoryRealizationPlan,
            _credential: Option<&pc::RepositoryHttpBasicCredential>,

            _intent: awaken_provisioning_contract::RepositoryRealizationIntent,
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::backend(
                "realize_repository",
                "clone failed",
            ))
        }

        async fn publish_repository(
            &self,
            _plan: &pc::RepositoryRealizationPlan,
            _expectation: &pc::RepositoryPublicationExpectation,
            _credential: Option<&pc::RepositoryHttpBasicCredential>,
        ) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
            unreachable!("R9 tests realization only")
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let env = awaken_session_environment::SandboxEnvironment::workdir(
        LocalProvider::new(tmp.path())
            .create_sandbox(&agent_run_sandbox_spec("s"))
            .await
            .unwrap(),
    )
    .into_session_environment();
    let host = host().await;
    host.realization_executor
        .staging()
        .register_thread_resources(
            "t",
            StagedResources {
                repositories: vec![RepositoryActivation {
                    plan: pc::RepositoryRealizationPlan {
                        repository_id: "repo-escape".into(),
                        mount_path: "../escape".into(),
                        source_remote_url: "https://example.invalid/x.git".into(),
                        transport_url: "https://example.invalid/x.git".into(),
                        initial_branch: None,
                        initial_commit: None,
                        access: pc::MountAccess::ReadWrite,
                    },
                    credential_pin: None,
                }],
                ..Default::default()
            },
        );
    let err = host
        .realization_executor
        .staging()
        .realize_thread_repositories(
            "t",
            host.run_assembly.local_workspace(),
            env.repositories().as_ref(),
        )
        .await;
    assert!(
        err.is_err(),
        "R9a unsafe repo mount must abort Session start"
    );

    host.realization_executor
        .staging()
        .register_thread_resources(
            "clone-failure",
            StagedResources {
                repositories: vec![repository_activation("repo")],
                ..Default::default()
            },
        );
    let error = host
        .realization_executor
        .staging()
        .realize_thread_repositories(
            "clone-failure",
            host.run_assembly.local_workspace(),
            &FailingRepositoryRealizer,
        )
        .await
        .expect_err("R9b clone failure must abort Session start");
    assert!(
        error.to_string().contains("clone failed"),
        "R9b/E1: {error:?}"
    );
}

#[tokio::test]
async fn explicit_repository_publication_uses_one_frozen_activation_and_verifies_receipt() {
    /* Publication-helper cause/effect table.
     * Causes: C1 activation is read/write; C2 expectation has a valid exact
     * coordinate; C3 adapter receipt is canonical. Effects: E1 invoke exactly
     * one Realizer effect; E2 return the verified receipt; E3 reject before
     * any adapter call; E4 reject mismatched evidence. Rules: H1 C1+C2+C3 =>
     * E1+E2; H2 !C1=>E3; H3 !C2=>E3; H4 C1+C2+!C3=>E1+E4. The caller supplies
     * the activation compiled from its frozen ResolvedInput; no thread Resource
     * lookup or publish-all loop exists in this helper.
     */
    let host = host().await;
    let repository = repository_activation("repo");
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        expected_prior_commit: None,
    };
    let realizer = RecordingPublicationRealizer::default();
    let receipt = host
        .realization_executor
        .staging()
        .publish_repository_activation(
            "thread",
            host.run_assembly.local_workspace(),
            &repository,
            &[],
            &realizer,
            &expectation,
            None,
        )
        .await
        .expect("H1");
    receipt.verify(&repository.plan, &expectation).unwrap();
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H1/E1");

    let readonly = RepositoryActivation {
        plan: pc::RepositoryRealizationPlan {
            access: pc::MountAccess::ReadOnly,
            ..repository.plan.clone()
        },
        credential_pin: None,
    };
    let error = host
        .realization_executor
        .staging()
        .publish_repository_activation(
            "thread",
            host.run_assembly.local_workspace(),
            &readonly,
            &[],
            &realizer,
            &expectation,
            None,
        )
        .await
        .expect_err("H2");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.to_string().contains("read-only")
        ),
        "H2/E3"
    );
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H2 no effect");

    let invalid = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "short".into(),
        expected_prior_commit: None,
    };
    host.realization_executor
        .staging()
        .publish_repository_activation(
            "thread",
            host.run_assembly.local_workspace(),
            &repository,
            &[],
            &realizer,
            &invalid,
            None,
        )
        .await
        .expect_err("H3");
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H3 no effect");

    let mismatched = RecordingPublicationRealizer {
        mismatched_receipt: true,
        ..Default::default()
    };
    let error = host
        .realization_executor
        .staging()
        .publish_repository_activation(
            "thread",
            host.run_assembly.local_workspace(),
            &repository,
            &[],
            &mismatched,
            &expectation,
            None,
        )
        .await
        .expect_err("H4");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.to_string().contains("does not match")
        ),
        "H4/E4"
    );
    assert_eq!(*mismatched.calls.lock().unwrap(), 1, "H4/E1");
}

#[tokio::test]
async fn reverse_channels_are_safe_noops_without_a_live_session() {
    let host = host().await;
    // Stage a repo, but never create a session for the thread: reverse channels
    // must early-return (no live env), not panic. Memory write-through is owned
    // by the MemoryMount guard and therefore has no Host-side reverse channel.
    host.realization_executor
        .staging()
        .register_thread_resources(
            "t",
            StagedResources {
                repositories: vec![repository_activation("r")],
                ..Default::default()
            },
        );
    assert!(
        host.harvest_thread_artifacts("t")
            .await
            .unwrap()
            .receipts
            .is_empty()
    );
    assert!(
        host.harvest_thread_artifacts("never-seen")
            .await
            .unwrap()
            .receipts
            .is_empty()
    );
}

#[tokio::test]
async fn agent_authored_skill_remains_run_scoped_until_explicit_publication() {
    // Cause/effect decision table (ADR-0036 D8): C1 an Agent writes a valid
    // Skill under the writable run root; C2 no external PromotionGate has
    // published it. R1 C1+C2 => E1 the live Sandbox scan discovers the
    // Agent-created Skill, E2 the shared catalog remains unchanged, and E3
    // disposing the Sandbox cannot promote it as a cleanup side effect.
    // Constraint: explicit control-plane publication is covered separately
    // by the durable-catalog test below and is the only route to that store.
    let dir = std::env::temp_dir().join(format!("awaken-skillharvest-{}", std::process::id()));
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_skill_store(dir.join("store")))
    .build()
    .expect("validated Host dependencies");

    // A real sandbox env with a skill authored under the workspace `skills/` dir.
    let base = dir.join("sbx");
    let env = awaken_session_environment::SandboxEnvironment::workdir(
        LocalProvider::new(&base)
            .create_sandbox(&agent_run_sandbox_spec("t"))
            .await
            .unwrap(),
    )
    .into_session_environment();
    let skill_dir = base.join("t").join("skills").join("notes");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: authored this run\n---\nremember to hydrate",
    )
    .unwrap();

    assert!(
        host.capabilities
            .tools
            .skills
            .definitions(host.run_assembly.local_workspace())
            .await
            .unwrap()
            .is_empty()
    );
    let authored = env
        .skills()
        .scan_skill_dir(crate::skills::DEFAULT_SKILLS_SUBDIR)
        .unwrap();
    assert!(
        authored.iter().any(|skill| skill.id == "notes"),
        "R1/E1: the authored Skill remains visible inside its originating run"
    );

    env.lifecycle().dispose().await.unwrap();
    assert!(
        host.capabilities
            .tools
            .skills
            .definitions(host.run_assembly.local_workspace())
            .await
            .unwrap()
            .is_empty(),
        "R1/E2-E3: cleanup must not self-promote an Agent-authored Skill"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_durable_skill_is_advertised_by_a_resolvable_catalog_id() {
    // Cause/effect rule V1: C1 a Skill is persisted into the versioned
    // catalog => E1 Managed advertisement contains its stable resource id
    // and E2 the corresponding frozen bytes remain loadable. Constraint:
    // display names and host-static specs cannot satisfy E2, so they cannot
    // enter this Managed id set. The static exclusion row is covered by
    // `managed_session_folds_builtins_into_the_agent_toolset`.
    let dir = std::env::temp_dir().join(format!("awaken-skillid-{}", std::process::id()));
    let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test")
        .await
        .with_skill_store(dir.join("store")))
    .build()
    .expect("validated Host dependencies");
    host.capabilities
        .tools
        .skills
        .persist_authored(
            host.run_assembly.local_workspace(),
            "Greeter",
            "---\nname: Greeter\ndescription: hi\n---\nsay hi",
        )
        .await;

    let cid = "Greeter".to_string();
    assert_eq!(
        host.capabilities
            .tools
            .skills
            .managed_ids_in(host.run_assembly.local_workspace()),
        vec![cid.clone()],
        "Managed advertisement contains only the version-backed catalog id"
    );

    let version = host
        .capabilities
        .tools
        .skills
        .cache_snapshot_in(host.run_assembly.local_workspace())
        .into_iter()
        .find(|version| version.skill_id.as_str() == cid)
        .expect("the advertised resource id must resolve to the Skill version");
    assert!(version.skill_md().unwrap().ends_with(b"say hi"));
    assert!(
        host.capabilities
            .tools
            .skills
            .cache_snapshot_in(host.run_assembly.local_workspace())
            .into_iter()
            .find(|version| version.skill_id.as_str() == "skill_deadbeefdeadbeef")
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
