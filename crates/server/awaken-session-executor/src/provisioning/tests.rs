use super::*;
use crate::host::SharedHost;
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse};
use awaken_sandbox_container::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
use awaken_sandbox_local::LocalProvider;

/// The logical path a staged resource realizes under, recovered from a projected
/// pc mount (`.mnt/<logical>`) so the registry assertions stay resource-oriented.
fn logical_of(m: &pc::MountRequirement) -> &str {
    m.mount_path.strip_prefix(".mnt/").unwrap_or(&m.mount_path)
}

/// Whether a projected Workdir spec denies tool egress.
fn denies(spec: &pc::SandboxSpec) -> bool {
    spec.deny_tool_egress
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

fn host() -> SharedHost {
    SharedHost::new(Arc::new(NoLlm), "test")
}

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

fn terminal_cleanup_test_lease() -> awaken_session_contract::SessionRealizationLease {
    awaken_session_contract::SessionRealizationLease {
        owner: "terminal-provisioning-test".into(),
        runtime_incarnation: "terminal-provisioning-test/incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: crate::terminal_repository_publication::runtime_unix_now_ms() + 60_000,
    }
}

fn terminal_cleanup_projection(
    workspace_id: &str,
    environment: awaken_session_contract::SessionEnvironmentState,
) -> awaken_session_contract::FrozenSessionProjection {
    frozen_projection_with_model_override(workspace_id, environment, None)
}

fn frozen_projection_with_model_override(
    workspace_id: &str,
    environment: awaken_session_contract::SessionEnvironmentState,
    model_override: Option<awaken_session_contract::SessionModelOverride>,
) -> awaken_session_contract::FrozenSessionProjection {
    let holder = awaken_runtime_contract::PlaintextHolder::new(
        awaken_runtime_contract::PlaintextBoundary::Worker,
        "terminal.provisioning.test",
    );
    let baseline = awaken_session_contract::SessionBaseline::compile(
        awaken_session_contract::SessionBaselineInputs {
            environment: awaken_session_contract::EnvironmentSnapshot {
                environment_id: "terminal-test-environment".into(),
                revision: awaken_session_contract::EnvironmentRevision(1),
                self_hosted: false,
                config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                    "terminal-test-environment-v1".into(),
                ),
                sandbox: Default::default(),
                sandbox_provisioning: Default::default(),
                idle_retention: Default::default(),
                packages: Default::default(),
                prepared_image: None,
                network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
                credential_realization: awaken_runtime_contract::CredentialRealizationProfile {
                    inference_holder: holder.clone(),
                    mcp_holder: holder.clone(),
                    resource_holder: holder,
                },
            },
            runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
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
    awaken_session_contract::FrozenSessionProjection {
        workspace_id: workspace_id.into(),
        revision: awaken_session_contract::SessionRevision(1),
        baseline,
        agent_publication: None,
        environment,
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: Some(awaken_session_contract::SessionResourceManifest::new(
            workspace_id,
            awaken_session_contract::ResolvedSessionResources::default(),
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
    host: &SharedHost,
    session_id: &str,
    workspace_id: &str,
) {
    host.install_frozen_session_projection(
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
    host: &SharedHost,
    session_id: &str,
    workspace_id: &str,
    environment: Arc<crate::session_environment::SessionEnvironment>,
) {
    let binding = serde_json::to_string(&environment.handle())
        .expect("terminal artifact Environment binding");
    let state = awaken_session_contract::SessionEnvironmentState::Resident {
        binding,
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    host.install_session_environment_owner_projection(session_id, workspace_id, &state)
        .expect("F1/E1 project the canonical legacy durable binding");
    let candidate = host
        .begin_session_environment_adoption(session_id, environment)
        .expect("F2/E2 retain the exact adoption candidate");
    let crate::session_slot::UnboundSessionEnvironmentOrigin::DurableAdoption(identity) =
        &candidate.origin
    else {
        panic!("F3 aggregate projection must own the durable adoption identity");
    };
    host.publish_prepared_session_environment(session_id, &candidate, identity.clone())
        .expect("F2/E2 publish the exact durable owner");
}

async fn root_terminal_cleanup_effect(
    host: &SharedHost,
    session_id: &str,
    lease: awaken_session_contract::SessionRealizationLease,
) -> (
    awaken_session_contract::PersistedSession,
    awaken_session_contract::SessionTerminalCleanupEffect,
) {
    let environment = host
        .session_slots
        .read(session_id, |slot| slot.environment_owner.resident())
        .flatten()
        .expect("terminal test requires a resident Environment");
    let binding = serde_json::to_string(&environment.handle()).unwrap();
    let workspace_id = host
        .session_slots
        .read(session_id, |slot| slot.workspace.clone())
        .flatten()
        .expect("terminal test requires a root Workspace projection");
    let projection = terminal_cleanup_projection(
        &workspace_id,
        awaken_session_contract::SessionEnvironmentState::Resident {
            binding,
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        },
    );
    let mut session = awaken_session_contract::PersistedSession::frozen_with_budget(
        session_id,
        projection.baseline.clone(),
        awaken_session_contract::SessionResourceState::from_active(projection.resources.clone()),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    session.environment = projection.environment.clone();
    session.realization = Some(lease.clone());
    assert!(session.ensure_terminal_cleanup_fence());
    session
        .freeze_terminal_cleanup_targets(std::iter::empty(), 0, 0)
        .unwrap();
    let command = session
        .terminal_cleanup
        .command_for(session_id, session_id)
        .unwrap();
    host.install_terminal_cleanup_projection(
        &awaken_session_contract::SessionTerminalCleanupAssignment {
            session_id: session_id.into(),
            projection,
            lease: lease.clone(),
        },
    )
    .await
    .unwrap();
    let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(command, lease);
    (session, effect)
}

fn root_terminal_preparation_authorization(
    session: &awaken_session_contract::PersistedSession,
    effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    workspace_id: &str,
) -> awaken_session_contract::SessionTerminalCleanupPreparationAuthorization {
    let inherited_provider_disposal = session
        .authorize_terminal_cleanup_effect(effect)
        .expect("derive the root preparation authorization from the aggregate");
    awaken_session_contract::SessionTerminalCleanupPreparationAuthorization::try_new(
        effect.clone(),
        workspace_id.into(),
        inherited_provider_disposal,
    )
    .expect("close the exact root terminal preparation authorization")
}

#[test]
fn session_and_housekeeping_filesystem_continuity_are_distinct() {
    /* Continuity cause/effect table.
     * Causes: C1 the spec realizes the canonical Session environment; C2
     * the spec realizes a disposable child/probe environment. Effects: E1
     * request retained writable state for DurableRequest recovery; E2
     * request ephemeral state and therefore no configured continuation PVC.
     * Rules: SC1 C1=>E1; SC2 C2=>E2. The typed field participates in the
     * capacity identity, so the two requests cannot share warm capacity.
     */
    assert_eq!(
        host().sandbox_spec("session").filesystem_continuity,
        pc::FilesystemContinuity::Retained,
        "SC1"
    );
    assert_eq!(
        agent_run_sandbox_spec("probe").filesystem_continuity,
        pc::FilesystemContinuity::Ephemeral,
        "SC2"
    );
    assert_ne!(
        pc::SandboxCapacityShapeId::from_spec(&host().sandbox_spec("session")),
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

#[test]
fn register_replaces_the_threads_staged_set() {
    let host = host();
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("a.md"), resource_mount("b.md")],
            prompts: vec!["first".into()],
            memory_prompts: Vec::new(),
            binding_checks: Vec::new(),
            repositories: vec![repository_activation("repo-a")],
        },
    );
    // A second register REPLACES (correct at create time, before any first Run).
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("c.md")],
            prompts: vec!["second".into()],
            memory_prompts: Vec::new(),
            binding_checks: Vec::new(),
            repositories: Vec::new(),
        },
    );

    assert_eq!(host.sandbox_spec("t").mounts.len(), 1, "old mounts dropped");
    assert_eq!(host.thread_session_prompts("t"), vec!["second".to_string()]);
    assert!(
        host.thread_repository_activations("t").is_empty(),
        "old repository activation dropped"
    );
}

#[test]
fn sandbox_spec_carries_deny_egress_and_the_staged_mounts() {
    // Cause graph: a frozen restriction plus a provider without strict network
    // isolation selects the existing Workdir wrapper, not an unsupported
    // admission requirement.
    // | Rule | restriction | strict provider | network | deny wrapper |
    // | W1 | absent | no | unrestricted | no |
    // | W2 | none | no | unrestricted | yes |
    let host = host();
    // No registration and no egress: shared network, no mounts.
    let bare = host.sandbox_spec("t");
    assert!(!denies(&bare));
    assert!(bare.mounts.is_empty());

    host.install_environment_projection(
        "t",
        &awaken_session_contract::EnvironmentSnapshot {
            environment_id: "environment".into(),
            revision: awaken_session_contract::EnvironmentRevision(1),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "environment-1".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::None,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        },
    )
    .expect("freeze test Environment");
    host.register_thread_resources(
        "t",
        StagedResources {
            mounts: vec![resource_mount("notes.md")],
            ..Default::default()
        },
    );
    let spec = host.sandbox_spec("t");
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
    let host = host();
    let local = crate::session_environment::SessionEnvironmentProvider::workdir(
        tempfile::tempdir().unwrap().path(),
    );
    let bare = host.sandbox_spec("bare");
    assert_eq!(bare.isolation, pc::IsolationClass::Workdir, "W2/E3");

    host.register_thread_resources(
        "repository-session",
        StagedResources {
            repositories: vec![repository_activation("repo")],
            ..Default::default()
        },
    );
    let repository_spec = host.sandbox_spec("repository-session");
    assert_eq!(
        repository_spec.isolation,
        pc::IsolationClass::Namespace,
        "W1/E1"
    );
    let error = match host
        .create_session_environment(&local, &repository_spec)
        .await
    {
        Ok(_) => panic!("W1/E2 Workdir cannot offer one /workspace path"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("backend isolation is weaker than requested"),
        "W1/E2: {}",
        error.message
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
        ) -> Result<(), pc::SandboxError> {
            Err(pc::SandboxError::new("clone failed"))
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
    let env = crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(tmp.path())
            .create_sandbox(&agent_run_sandbox_spec("s"))
            .await
            .unwrap(),
    );
    let host = host();
    host.register_thread_resources(
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
    let err = host.realize_thread_repositories("t", &env).await;
    assert!(
        err.is_err(),
        "R9a unsafe repo mount must abort Session start"
    );

    host.register_thread_resources(
        "clone-failure",
        StagedResources {
            repositories: vec![repository_activation("repo")],
            ..Default::default()
        },
    );
    let error = host
        .realize_thread_repositories("clone-failure", &FailingRepositoryRealizer)
        .await
        .expect_err("R9b clone failure must abort Session start");
    assert!(error.message.contains("clone failed"), "R9b/E1: {error:?}");
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
    let host = host();
    let repository = repository_activation("repo");
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "0123456789abcdef0123456789abcdef01234567".into(),
        expected_prior_commit: None,
    };
    let realizer = RecordingPublicationRealizer::default();
    let receipt = host
        .publish_repository_activation("thread", &repository, &[], &realizer, &expectation, None)
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
        .publish_repository_activation("thread", &readonly, &[], &realizer, &expectation, None)
        .await
        .expect_err("H2");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.message.contains("read-only")
        ),
        "H2/E3"
    );
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H2 no effect");

    let invalid = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: "short".into(),
        expected_prior_commit: None,
    };
    host.publish_repository_activation("thread", &repository, &[], &realizer, &invalid, None)
        .await
        .expect_err("H3");
    assert_eq!(*realizer.calls.lock().unwrap(), 1, "H3 no effect");

    let mismatched = RecordingPublicationRealizer {
        mismatched_receipt: true,
        ..Default::default()
    };
    let error = host
        .publish_repository_activation("thread", &repository, &[], &mismatched, &expectation, None)
        .await
        .expect_err("H4");
    assert!(
        matches!(
            error,
            RepositoryPublicationActivationError::Failed(error)
                if error.message.contains("does not match")
        ),
        "H4/E4"
    );
    assert_eq!(*mismatched.calls.lock().unwrap(), 1, "H4/E1");
}

#[tokio::test]
async fn reverse_channels_are_safe_noops_without_a_live_session() {
    let host = host();
    // Stage a repo, but never create a session for the thread: reverse channels
    // must early-return (no live env), not panic. Memory write-through is owned
    // by the MemoryMount guard and therefore has no Host-side reverse channel.
    host.register_thread_resources(
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

struct BatchArtifactContainerProvider {
    files: Vec<awaken_sandbox_container::EnvironmentFile>,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}

struct BatchArtifactContainer {
    handle: pc::SandboxHandle,
    files: Vec<awaken_sandbox_container::EnvironmentFile>,
    scans: Arc<std::sync::atomic::AtomicUsize>,
}

fn batch_artifact_container_handle(spec: &pc::SandboxSpec) -> pc::SandboxHandle {
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    pc::SandboxHandle::container_v2(
        &spec.scope,
        pc::ContainerSandboxHandleV2 {
            previous: pc::ContainerSandboxHandleV1 {
                container_id: format!("container-{}", spec.scope),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: Vec::new(),
                runtime_handle: None,
                sandbox_control_incarnation: None,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint: fingerprint.clone(),
            realization_fingerprint: fingerprint,
            owned_paths: Vec::new(),
        },
    )
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironmentProvider for BatchArtifactContainerProvider {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Container,
            tool_transparent: true,
            path_fidelity: true,
            enforced_readonly: true,
            network_isolation: true,
            enforced_network_allowlist: true,
            secret_egress_substitution: true,
            resource_limits: true,
            custom_rootfs: true,
            package_provisioning: true,
            control_services: Default::default(),
        }
    }

    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(BatchArtifactContainer {
            handle: batch_artifact_container_handle(spec),
            files: self.files.clone(),
            scans: self.scans.clone(),
        }))
    }

    async fn adopt_environment(
        &self,
        _adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "batch Artifact fixture never adopts an Environment",
        ))
    }
}

#[async_trait::async_trait]
impl awaken_sandbox_container::ContainerEnvironment for BatchArtifactContainer {
    async fn spawn_agent_process(
        &self,
        _command: pc::Command,
    ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "batch Artifact fixture never spawns an Agent",
        ))
    }

    async fn read_files(
        &self,
        root: &str,
    ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
        assert_eq!(root, "/outputs", "B1/C1 canonical output root");
        self.scans.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.files.clone())
    }
}

#[async_trait::async_trait]
impl SandboxControlServicePublisher for BatchArtifactContainer {
    async fn publish_sandbox_control_service(
        &self,
        _kind: SandboxControlServiceKind,
        _service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        // Batch Artifact tests advertise no control service; keep the fixture's
        // unavailable path exact and never synthesize a successful lease.
        Err(SandboxControlPublishError)
    }
}

#[async_trait::async_trait]
impl pc::Sandbox for BatchArtifactContainer {
    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
    }

    async fn spawn(
        &self,
        _command: pc::Command,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "batch Artifact fixture never spawns a process",
        ))
    }

    async fn attach(
        &self,
        _requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "batch Artifact fixture never attaches a mount",
        ))
    }

    async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "Host batch capture must not use the Container Sandbox list port",
        ))
    }

    async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "Host batch capture must not reread one Container Artifact",
        ))
    }

    fn realized(&self) -> &[pc::RealizedMount] {
        &[]
    }

    async fn process(
        &self,
        _process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "batch Artifact fixture has no process",
        ))
    }

    async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        Ok(pc::SandboxStatus::Ready)
    }

    async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn dispose(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

async fn install_batch_artifact_environment(host: &SharedHost, thread: &str, workspace: &str) {
    host.register_thread_workspace(thread, workspace);
    let spec = host.sandbox_spec(thread);
    let environment = Arc::new(
        host.create_session_environment(&host.session_provider, &spec)
            .await
            .expect("create batch Artifact Container Environment"),
    );
    host.install_test_resident_session_environment(thread, environment);
}

#[tokio::test]
async fn terminal_release_harvests_outputs_idempotently_before_sandbox_disposal() {
    // Constraint/Invariant: the authoritative inputs and ownership boundaries
    // documented here remain the only decision source; no parallel path is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    use awaken_session_contract::SessionRuntime;
    // Cause/effect decision table:
    // R1 output under the canonical layout root + live Sandbox => harvest
    // creates one scoped File with the exact root-relative logical path.
    // R2 identical retry => same File id, no duplicate manifest row/reference.
    // R3 terminal preparation => File is durable and Sandbox remains; R4 the
    // aggregate records that exact preparation plus its Repository participant,
    // then and only then projects physical disposal => Sandbox gone while File
    // metadata/bytes remain.
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test").with_store_dir(storage.path()));
    host.register_thread_workspace("session-artifacts", "workspace-a");
    install_terminal_artifact_projection_before_environment(
        host.as_ref(),
        "session-artifacts",
        "workspace-a",
    )
    .await;
    let lease = terminal_cleanup_test_lease();
    let spec = host.sandbox_spec("session-artifacts");
    let create_fence = lease
        .sandbox_effect_fence("session-artifacts-create")
        .unwrap();
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        host.provider
            .create_sandbox_for_effect(&spec, &create_fence, None)
            .await
            .unwrap(),
    ));
    install_terminal_artifact_environment(
        host.as_ref(),
        "session-artifacts",
        "workspace-a",
        environment.clone(),
    );
    let output = storage
        .path()
        .join("sandboxes")
        .join("session-artifacts")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"durable report").unwrap();

    let first = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    let retry = host
        .harvest_thread_artifacts("session-artifacts")
        .await
        .unwrap();
    assert_eq!(first.receipts.len(), 1);
    assert_eq!(retry.receipts[0].record.id, first.receipts[0].record.id);
    assert!(first.receipts[0].record.id.starts_with("file_"));
    assert!(first.receipts[0].record.downloadable);
    assert_eq!(
        first.receipts[0].record.logical_path.as_deref(),
        Some("report.txt"),
        "R1 canonical output-root projection"
    );

    let (mut session, effect) =
        root_terminal_cleanup_effect(host.as_ref(), "session-artifacts", lease).await;
    let managed = crate::ManagedHost::new(host.clone());
    let workspace_id = host
        .session_slots
        .read("session-artifacts", |slot| slot.workspace.clone())
        .flatten()
        .expect("R4 frozen aggregate Repository inputs");
    let authorization = root_terminal_preparation_authorization(&session, &effect, &workspace_id);
    let preparation = managed
        .prepare_terminal_cleanup_for_effect(effect.clone(), authorization)
        .await
        .unwrap();
    assert!(
        host.session_slots
            .read("session-artifacts", |slot| {
                slot.environment_owner
                    .terminal_bound_environment()
                    .is_some()
            })
            .unwrap_or(false),
        "R3 exact owner remains Retiring until aggregate-authorized Disposal"
    );
    let repository_preparation = awaken_session_contract::SessionCleanupRepositoryPreparation::new(
        "session-artifacts",
        &workspace_id,
        &session.resources,
    )
    .unwrap();
    session
        .record_terminal_cleanup_preparation(
            &workspace_id,
            &effect.lease,
            preparation,
            Some(repository_preparation),
        )
        .unwrap();
    let disposal_command = match session
        .terminal_cleanup_work_action()
        .unwrap()
        .expect("R4 durable preparation projects disposal")
    {
        awaken_session_contract::SessionTerminalCleanupAction::Dispose { command } => command,
        action => panic!("R4 expected aggregate disposal action, got {action:?}"),
    };
    let disposal = awaken_session_contract::SessionTerminalCleanupDisposalEffect::new(
        disposal_command,
        effect.lease,
    );
    managed
        .dispose_terminal_cleanup_for_effect(disposal)
        .await
        .unwrap();
    assert!(
        host.session_environment("session-artifacts")
            .await
            .is_none()
    );
    let records = host
        .file_application()
        .expect("test startup installs File application")
        .list("workspace-a", Some("session-artifacts"))
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "terminal retry remains idempotent");
    assert_eq!(
        host.file_application()
            .expect("test startup installs File application")
            .bytes("workspace-a", &records[0].id)
            .await
            .unwrap()
            .unwrap()
            .1,
        b"durable report"
    );
}

#[derive(Default)]
struct RecordingArtifactRecovery {
    publish_calls: std::sync::atomic::AtomicUsize,
    recover_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ArtifactPublisher<awaken_dispatch::ArtifactPublicationFence>
    for RecordingArtifactRecovery
{
    async fn publish(
        &self,
        _publication: awaken_resource_contract::ArtifactPublication<
            awaken_dispatch::ArtifactPublicationFence,
        >,
    ) -> Result<
        awaken_resource_contract::ArtifactPublicationReceipt,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.publish_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(awaken_resource_contract::ArtifactPublicationError::new(
            "unexpected publication",
        ))
    }

    async fn recover(
        &self,
        _recovery: awaken_resource_contract::ArtifactRecovery<
            awaken_dispatch::ArtifactPublicationFence,
        >,
    ) -> Result<
        Vec<awaken_resource_contract::ArtifactPublicationReceipt>,
        awaken_resource_contract::ArtifactPublicationError,
    > {
        self.recover_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn artifact_harvest_has_one_file_authority_and_no_magic_bundle_gate() {
    // Cause/effect decision table: C1 an output path happens to be named
    // `skill-export/change.patch`; C2 no manifest or checksum sibling exists;
    // C3 the same immutable bytes are harvested again. Effects: E1 the File
    // aggregate publishes the exact bytes without interpreting the directory;
    // E2 terminal harvest is not blocked by a second bundle-completion rule;
    // E3 replay returns the same File identity. R1 C1+C2=>E1+E2;
    // R2 C1+C2+C3=>E1+E2+E3.
    let storage = tempfile::tempdir().unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));
    host.register_thread_workspace("plain-files", "workspace-a");
    let spec = agent_run_sandbox_spec("plain-files");
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        LocalProvider::new(storage.path())
            .create_sandbox(&spec)
            .await
            .unwrap(),
    ));
    host.install_test_resident_session_environment("plain-files", environment);
    let output = storage
        .path()
        .join("plain-files")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("skill-export")
        .join("change.patch");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"diff --git a/a b/a\n").unwrap();

    let first = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R1/E1+E2");
    let replay = host
        .harvest_thread_artifacts("plain-files")
        .await
        .expect("R2/E1+E2+E3");
    assert_eq!(first.receipts.len(), 1, "R1/E1");
    assert_eq!(replay.receipts.len(), 1, "R2/E3");
    assert_eq!(
        replay.receipts[0].record.id, first.receipts[0].record.id,
        "R2/E3"
    );
}

struct FailingFileCatalog;

use awaken_resource_contract::CreateFileRecordOutcome;

#[async_trait::async_trait]
impl FileCatalog for FailingFileCatalog {
    async fn create_file(
        &self,
        _record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        Err(FileCatalogError::Storage("injected catalog failure".into()))
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Ok(Vec::new())
    }

    async fn list_files_including_deleted(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        Ok(Vec::new())
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        Ok(None)
    }

    async fn active_size_bytes(&self, _workspace_id: &str) -> Result<u64, FileCatalogError> {
        Ok(0)
    }
}

#[tokio::test]
async fn terminal_harvest_failure_preserves_the_sandbox_for_retry() {
    use awaken_session_contract::SessionRuntime;

    // Test design. Causes: R4 has terminal output present while its durable
    // catalog write fails. Effects: terminal preparation fails and preserves
    // both the exact hidden Retiring Environment and output for retry.
    // Constraint/Invariant: sandbox disposal follows successful durable
    // harvest, never precedes it. Decision rule: execute R4 and require failure
    // with zero disposal.
    let storage = tempfile::tempdir().unwrap();
    let mut raw_host = SharedHost::new(Arc::new(NoLlm), "test").with_store_dir(storage.path());
    let catalog = Arc::new(FailingFileCatalog);
    raw_host.file_catalog = catalog.clone();
    let application = Arc::new(awaken_resource_application::FileApplication::new(
        raw_host.file_store(),
        catalog,
        raw_host
            .resource_reclamation()
            .expect("test lifecycle repository"),
    ));
    raw_host = raw_host.with_file_application(
        application.clone(),
        Arc::new(
            awaken_resource_application::ApplicationFileContentSource::new(application.clone()),
        ),
        Arc::new(awaken_resource_application::ApplicationArtifactPublisher::new(application)),
    );
    let host = Arc::new(raw_host);
    host.register_thread_workspace("session-harvest-failure", "test");
    install_terminal_artifact_projection_before_environment(
        host.as_ref(),
        "session-harvest-failure",
        "test",
    )
    .await;
    let lease = terminal_cleanup_test_lease();
    let spec = host.sandbox_spec("session-harvest-failure");
    let create_fence = lease
        .sandbox_effect_fence("session-harvest-failure-create")
        .unwrap();
    let environment = Arc::new(crate::session_environment::SessionEnvironment::workdir(
        host.provider
            .create_sandbox_for_effect(&spec, &create_fence, None)
            .await
            .unwrap(),
    ));
    install_terminal_artifact_environment(
        host.as_ref(),
        "session-harvest-failure",
        "test",
        environment,
    );
    let output = storage
        .path()
        .join("sandboxes")
        .join("session-harvest-failure")
        .join(spec.outputs_path.trim_start_matches('/'))
        .join("report.txt");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    std::fs::write(&output, b"retry me").unwrap();

    let (cleanup, effect) =
        root_terminal_cleanup_effect(host.as_ref(), "session-harvest-failure", lease).await;
    let workspace_id = host
        .registered_thread_workspace("session-harvest-failure")
        .expect("harvest failure retains the frozen Workspace");
    let authorization = root_terminal_preparation_authorization(&cleanup, &effect, &workspace_id);
    let error = crate::ManagedHost::new(host.clone())
        .prepare_terminal_cleanup_for_effect(effect, authorization)
        .await
        .unwrap_err();
    assert!(error.message.contains("injected catalog failure"));
    assert!(
        host.session_slots
            .read("session-harvest-failure", |slot| {
                slot.environment_owner
                    .terminal_bound_environment()
                    .is_some()
            })
            .unwrap_or(false),
        "R4 exact terminal owner remains hidden and retryable"
    );
    assert_eq!(std::fs::read(output).unwrap(), b"retry me");
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
    let host = SharedHost::new(Arc::new(NoLlm), "test").with_skill_store(dir.join("store"));
    host.skills
        .persist_authored(
            host.local_workspace(),
            "Greeter",
            "---\nname: Greeter\ndescription: hi\n---\nsay hi",
        )
        .await;

    let cid = "Greeter".to_string();
    assert_eq!(
        host.skills.managed_ids_in(host.local_workspace()),
        vec![cid.clone()],
        "Managed advertisement contains only the version-backed catalog id"
    );

    let version = host
        .skills
        .cache_snapshot_in(host.local_workspace())
        .into_iter()
        .find(|version| version.skill_id.as_str() == cid)
        .expect("the advertised resource id must resolve to the Skill version");
    assert!(version.skill_md().unwrap().ends_with(b"say hi"));
    assert!(
        host.skills
            .cache_snapshot_in(host.local_workspace())
            .into_iter()
            .find(|version| version.skill_id.as_str() == "skill_deadbeefdeadbeef")
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
