// @formal evidence agent_profile.effective_publication_field_replacement_is_exact_and_noop_preserving
// @formal evidence agent_profile.requested_revision_never_follows_mutable_current
// @formal evidence session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal evidence session_creation.atomic_complete_root_precedes_every_projection
// @formal evidence session_creation.exact_replay_conflict_and_tombstone_are_closed
// @formal evidence session_creation.network_intersection_is_canonical_and_non_widening
// @formal evidence session_creation.selected_environment_is_frozen_in_baseline
// @formal evidence session_creation.work_projection_is_disposable_repairable_and_non_authoritative
// @formal evidence session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root
use super::*;
use awaken_session_types::SessionId;

#[derive(Default)]
pub(super) struct RecordingCreateRuntime {
    pub(super) baseline_installs: AtomicUsize,
    pub(super) preparations: AtomicUsize,
    pub(super) projection_failure: AtomicUsize,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingCreateRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        unreachable!("create replay test never runs")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        unreachable!("create replay test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        unreachable!("create replay test never custom-resumes")
    }

    fn model(&self) -> String {
        "create-replay-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingCreateRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingCreateRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingCreateRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    async fn install_session_projection(
        &self,
        _thread: &str,
        _projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        self.baseline_installs.fetch_add(1, Ordering::SeqCst);
        if matches!(
            mode,
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch
                | awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                    prepare_session: true,
                    ..
                }
        ) {
            self.preparations.fetch_add(1, Ordering::SeqCst);
        }
        match self.projection_failure.swap(0, Ordering::SeqCst) {
            1 => {
                return Err(RunError::unavailable(
                    "injected retryable projection failure",
                ));
            }
            2 => {
                return Err(RunError::classified(
                    "session_realization_invariant_violated",
                    "injected terminal projection failure",
                ));
            }
            3 => return Err(RunError::internal("injected unknown projection failure")),
            _ => {}
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for RecordingCreateRuntime {
    fn validate_session_sandbox_layout(
        &self,
        _thread: &str,
        _layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        // Test-runtime decision rule: C1 creation tests intentionally model an
        // available provider; C2 layouts may contain Repository bindings. E1 an
        // explicit capable fake admits both, while the production trait default
        // remains fail-closed for C2 when no layout authority is installed.
        Ok(())
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_execution_contract::__macro_support::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::CheckpointReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_execution_contract::__macro_support::SourceReleasePreparationEffect,
        generation: &awaken_session_execution_contract::__macro_support::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceReleasePreparedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_execution_contract::__macro_support::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceDisposedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_execution_contract::__macro_support::SandboxRestoreRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::RestoreReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_execution_contract::__macro_support::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_execution_contract::__macro_support::SessionResourceTransition,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .apply_session_inputs(thread, transition)
            .await
    }

    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_execution_contract::__macro_support::SessionRepositoryPublicationCommand,
        lease: &awaken_session_execution_contract::__macro_support::SessionRealizationLease,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SessionRepositoryPublicationEffect,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_execution_contract::__macro_support::SessionTerminalCleanupDisposalEffect,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SessionCleanupDisposalReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_terminal_cleanup_for_effect(effect)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for RecordingCreateRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_execution_contract::OutcomeDrive, RunError> {
        unreachable!("create replay test never defines an outcome")
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_execution_contract::OutcomeDrive>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

fn profiled_mount(id: &str) -> awaken_provisioning_contract::MountRequirement {
    awaken_provisioning_contract::MountRequirement {
        mount_id: id.into(),
        source: awaken_provisioning_contract::MountSource::Inline {
            contents: id.into(),
        },
        mount_path: format!("/workspace/{id}"),
        access: awaken_provisioning_contract::MountAccess::ReadOnly,
        lifetime: awaken_provisioning_contract::MountLifetime::PerRun,
        required: true,
    }
}

fn profiled_env(name: &str) -> awaken_provisioning_contract::EnvVar {
    awaken_provisioning_contract::EnvVar {
        name: name.into(),
        value: awaken_provisioning_contract::EnvValue::Inline { value: "1".into() },
        visibility: awaken_provisioning_contract::EnvVisibility::Process,
    }
}

struct AdmissionEnvironment;

struct ProfiledAgent {
    unavailable: bool,
}

struct ResourceProfiledAgent;

struct SubstitutingProfileSource;

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for SubstitutingProfileSource {
    fn session_profile_in(
        &self,
        _workspace_id: &str,
        _agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        None
    }

    fn session_profile_at_revision_in(
        &self,
        _workspace_id: &str,
        _agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        Some(
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                source_revision: source_revision.saturating_add(1),
                model: Some("substituted-current-model".into()),
                ..Default::default()
            },
        )
    }
}

struct FixedModelPublication {
    publication: awaken_session_configuration_contract::SessionModelPublication,
}

struct RecordingModelPublication {
    publication: awaken_session_configuration_contract::SessionModelPublication,
    requests: Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::SessionModelPublicationResolver
    for FixedModelPublication
{
    async fn resolve_session_model(
        &self,
        _workspace_id: &str,
        _model_reference: &str,
    ) -> Result<
        awaken_session_configuration_contract::SessionModelPublication,
        awaken_session_configuration_contract::SessionModelResolutionError,
    > {
        Ok(self.publication.clone())
    }
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::SessionModelPublicationResolver
    for RecordingModelPublication
{
    async fn resolve_session_model(
        &self,
        workspace_id: &str,
        model_reference: &str,
    ) -> Result<
        awaken_session_configuration_contract::SessionModelPublication,
        awaken_session_configuration_contract::SessionModelResolutionError,
    > {
        self.requests
            .lock()
            .expect("model requests")
            .push((workspace_id.to_string(), model_reference.to_string()));
        Ok(self.publication.clone())
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for ProfiledAgent {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        (workspace_id == "workspace" && agent_id == "profiled").then(|| {
            awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                model: Some("published-model".into()),
                execution_model_ref: Some("execution-model".into()),
                mcp_servers: vec![
                    awaken_executable_agent_contract::ExecutableAgentMcpServer {
                        name: "agent-only".into(),
                        target: awaken_agent_contract::McpTarget::parse_http(
                            "https://agent-only.example.test/mcp",
                        )
                        .unwrap(),
                        prompts_as_skills: false,
                        credential_source_id: None,
                        credential_revision: None,
                    },
                    awaken_executable_agent_contract::ExecutableAgentMcpServer {
                        name: "shared".into(),
                        target: awaken_agent_contract::McpTarget::parse_http(
                            "https://agent-shared.example.test/mcp",
                        )
                        .unwrap(),
                        prompts_as_skills: false,
                        credential_source_id: None,
                        credential_revision: None,
                    },
                ],
                ..Default::default()
            }
        })
    }

    fn session_profile_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        (workspace_id == "workspace" && agent_id == "profiled" && matches!(source_revision, 7 | 9))
            .then(
                || awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                    source_revision,
                    model: Some("historical-model".into()),
                    execution_model_ref: Some("historical-execution-model".into()),
                    ..Default::default()
                },
            )
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        (workspace_id == "workspace" && agent_id == "profiled" && source_revision == 7).then(|| {
            let mut snapshot =
                awaken_runtime_contract::ExecutableAgentSnapshot::builder("profiled")
                    .model(awaken_runtime_contract::resolved::ModelBinding::new(
                        "test", "model", "native",
                    ))
                    .fingerprint("profiled-revision-7")
                    .build();
            snapshot.metadata.source.revision = 7;
            snapshot
        })
    }

    fn agent_unavailable_in(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.unavailable && workspace_id == "workspace" && agent_id == "profiled"
    }
}

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for ResourceProfiledAgent {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        let mut profile =
            awaken_executable_agent_contract::ExecutableAgentProfileSource::session_profile_in(
                &ProfiledAgent { unavailable: false },
                workspace_id,
                agent_id,
            )?;
        profile.resources = vec![awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from("agent-resource"),
            target: awaken_resource_contract::InputResourceId::File(
                awaken_resource_contract::FileId::from("agent-file"),
            ),
            mount_path: "/workspace/agent-input".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }];
        Some(profile)
    }
}

fn direct_file_attachment(
    binding_id: &str,
    file_id: &str,
    mount_path: &str,
) -> crate::session_resource::SessionInputAttachment {
    crate::session_resource::SessionInputAttachment {
        binding: awaken_resource_contract::InputBinding {
            binding_id: awaken_resource_contract::BindingId::from(binding_id),
            target: awaken_resource_contract::InputResourceId::File(
                awaken_resource_contract::FileId::from(file_id),
            ),
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        },
        replaces: None,
    }
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::SessionEnvironmentProjectionSource
    for AdmissionEnvironment
{
    type Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot;

    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        Ok(None)
    }

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        let mut snapshot = persisted("environment-template", false, "idle")
            .frozen_baseline()
            .expect("fixture baseline")
            .environment
            .clone();
        snapshot.environment_id = if environment_id == "substitute-me" {
            "another-environment".into()
        } else {
            environment_id.to_owned()
        };
        if environment_id == "blank-network" {
            snapshot.network =
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec![" ".into(), "\t".into()],
                };
        }
        Ok(Some(ResolvedSessionEnvironment { snapshot }))
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        _revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerAssignment for AdmissionEnvironment {
    async fn enqueue_session(
        &self,
        _environment_id: &str,
        _session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        unreachable!("local admission does not dispatch WorkQueue work")
    }

    async fn wake_session(
        &self,
        _environment_id: &str,
        _session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        unreachable!("local admission does not wake WorkQueue work")
    }

    async fn retire_session(
        &self,
        _environment_id: &str,
        _session_id: &awaken_session_types::SessionId,
    ) -> Result<(), awaken_session_types::RunError> {
        unreachable!("local admission does not retire WorkQueue work")
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeases for AdmissionEnvironment {
    async fn acquire_session(
        &self,
        _environment_id: &str,
        _session_id: &awaken_session_types::SessionId,
        _worker_owner: &str,
        _now_ms: u64,
    ) -> Result<Option<awaken_session_types::SessionWorkLease>, awaken_session_types::RunError>
    {
        Ok(None)
    }

    async fn release_session(
        &self,
        _lease: &awaken_session_types::SessionWorkLease,
    ) -> Result<bool, awaken_session_types::RunError> {
        Ok(false)
    }
    async fn release_owner(
        &self,
        _worker_owner: &str,
    ) -> Result<usize, awaken_session_types::RunError> {
        Ok(0)
    }
}

pub(super) fn creation_command(session_id: &str) -> CreateSessionCommand {
    let environment = persisted("environment-template", true, "idle")
        .frozen_baseline()
        .expect("fixture baseline")
        .environment
        .clone();
    CreateSessionCommand {
        initial_agent_configuration: None,
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        intent: awaken_session_configuration_contract::SessionCreationIntent {
            control: awaken_session_configuration_contract::ControlSessionCreationInputs {
                mutation_policy: crate::session_resource::SessionMutationPolicy::Managed,
                application_origin: None,
                environment,
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                system_prompt:
                    awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
                model: "model".into(),
                execution_model_ref: "model".into(),
                runtime: None,
                mcp_authoring: Default::default(),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: Default::default(),
                initial_mcp: Vec::new(),
            },
        },
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        budget: Default::default(),
        repository_configurations: Vec::new(),
        idempotency: None,
        initial_events: None,
    }
}

#[tokio::test]
async fn worker_projection_failure_is_rebuilt_from_the_durable_session_root() {
    // Cause/effect decision table:
    // C1 Worker placement + durable Preparing root; C2 initial Dispatch
    // projection fails after root commit; C3 recovery scans the same root.
    // C4 the first recovery projection also fails; C5 the frozen root carries a
    // pending Skill-only Resource generation. E1 create reports the effect
    // failure without deleting truth; E2 recovery reinstalls exactly the same
    // Dispatch projection; E3 no local realization lease/attempt is minted; E4
    // a recovery failure reports the exact string Session identity and settles
    // nothing; E5 the next scan may idempotently reinstall while the Worker has
    // not yet acknowledged realization. Rule R1=C1+C2=>E1; R2=C1+C3+C4=>E4;
    // R3=C1+C3+!C4+C5=>E2+E3; R4=R3 repeated=>E5. Terminal, local and
    // already-resident rows retain their adjacent recovery behavior.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("worker projection recovery repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    runtime.projection_failure.store(1, Ordering::SeqCst);
    let application = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repository.clone(), awaken_time::system_clock()),
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::RegisteredWorker,
        ..Default::default()
    })
    .build()
    .expect("valid fixture Session application");
    let mut command = creation_command("worker-projection-recovery");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    command.intent.control.environment.self_hosted = false;
    command.intent.control.resources = skill_resources("pending-skill");

    assert!(application.create_session(command).await.is_err(), "R1/E1");
    let session_id = SessionId::from("worker-projection-recovery");
    let stranded = repository
        .get(&session_id)
        .await
        .expect("R1 durable root remains");
    assert!(stranded.needs_initial_realization_recovery(), "R1/C1");
    assert!(stranded.resources.pending.is_some(), "R1/C5");
    assert!(stranded.realization.is_none(), "R1/E3");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R1/E1");

    runtime.projection_failure.store(1, Ordering::SeqCst);
    let failed_recovery = application.reconcile_session_realizations().await;
    assert!(failed_recovery.settled.is_empty(), "R2/E4");
    assert_eq!(failed_recovery.failures.len(), 1, "R2/E4");
    assert_eq!(
        failed_recovery.failures[0].session_id, "worker-projection-recovery",
        "R2/E4"
    );

    let first = application.reconcile_session_realizations().await;
    assert!(first.failures.is_empty(), "R3/E2: {:#?}", first.failures);
    assert_eq!(first.settled.len(), 1, "R3/E2");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 3, "R3/E2");
    assert!(
        repository
            .get(&session_id)
            .await
            .unwrap()
            .realization
            .is_none(),
        "R3/E3"
    );

    let replay = application.reconcile_session_realizations().await;
    assert!(replay.failures.is_empty(), "R4/E5");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 4, "R4/E5");
}

#[tokio::test]
async fn worker_checkpoint_retention_is_rejected_before_durable_creation() {
    // Creation-topology decision table: C1 frozen placement is Local/Worker;
    // C2 retention is Resident/CheckpointAndRelease. Only Worker+checkpoint
    // requires a remote continuation transport that does not exist. Effect E1
    // rejects before root insert or Runtime projection; the other three rules
    // retain their existing create paths. Constraint K1: the registered-Worker
    // authorize/persist channel is closed over Create/Adopt/Rebuild/Resource
    // reservation, while claimed resume ends at realization projection/MCP and
    // rejects continuation phases; neither is a checkpoint/restore transport.
    //
    // | Rule | placement | retention | Effect |
    // | T1 | Worker | CheckpointAndRelease | E1 reject, zero root/effect |
    // | T2 | Worker | Resident | existing external creation |
    // | T3 | Local | CheckpointAndRelease | existing local continuation |
    // | T4 | Local | Resident | existing local creation |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("Session repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let mut command = creation_command("worker-checkpoint-retention");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    command.intent.control.environment.idle_retention =
        super::continuation::checkpoint_release_retention();

    let error = app
        .create_session(command)
        .await
        .expect_err("T1/E1 unsupported external continuation must fail before insert");
    let SessionCreationError::Rejected(error) = error else {
        panic!("T1/E1 expected deterministic creation rejection")
    };
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "T1/E1",
    );
    assert!(matches!(
        repository
            .get(&SessionId::from("worker-checkpoint-retention"))
            .await,
        Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
    ));
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "T1/E1");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "T1/E1");
}

fn owned_repository_input(session_id: &str) -> SessionRepositoryResourceInput {
    SessionRepositoryResourceInput {
        id: format!("managed:{session_id}:repository:0"),
        workspace_id: "workspace".into(),
        name: "Session Repository".into(),
        description: "creation compensation fixture".into(),
        remote_url: "https://github.com/awaken/compensation.git".into(),
        credential_material: None,
        credential: None,
        mount_path: "/workspace/repository".into(),
        initial_branch: Some("main".into()),
        initial_commit: None,
    }
}

#[tokio::test]
async fn root_rejection_compensates_only_applied_repository_participants() {
    // Creation compensation consolidation causes/effects: original create
    // compile/layout/root-compile rejection, absent exact create replay, or a
    // deterministic replay rejection invokes the one existing participant-abort
    // driver BEFORE returning the original first error. Request input/Skill/pin,
    // Profiled configuration/input/Skill/pin/drift, and input lowering are the
    // same Session-subject compensation (14 callers, original warning literals).
    // Constraint: replay-query Unavailable/Internal is uncertain adoption and
    // NEVER enters compensation; Applied/Replayed ownership is still decided
    // by the existing driver, and cleanup failure cannot replace the first error.
    // Decision rules CC1 known rejection+Applied => retain first/retire;
    // CC2 known rejection+Replayed or adopted Root => retain first/preserve;
    // CC3 uncertain adoption => retain first/no compensation (original receipt
    // and realization recovery tests own that failure partition).
    // Cause/effect graph: C1 Repository participant is Applied/Replayed; C2 a
    // foreign durable root occupies the Session id without referencing that
    // Repository; C3 exact create replay proves idempotency mismatch. Effects:
    // E1 the caller still receives the first root Conflict; E2 Applied is
    // retired; E3 Replayed remains Active because an earlier command owns it.
    //
    // | Rule | Participant | Root receipt | Root reference | Result | Repository |
    // | C1 | Applied | different | absent | Conflict | Deleted |
    // | C2 | Replayed | different | absent | Conflict | Active |
    for (rule, replayed, expected_state) in [
        (
            "C1",
            false,
            awaken_resource_contract::ResourceState::Deleted,
        ),
        ("C2", true, awaken_resource_contract::ResourceState::Active),
    ] {
        let session_id = format!("creation-participant-{rule}");
        let repository = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("participant repository"),
        );
        let catalog = ephemeral_resource_registry().await;
        let mut application = application_with_runtime(
            Arc::new(RecordingCreateRuntime::default()),
            repository.clone(),
            Arc::new(AdmissionEnvironment),
        );
        application.set_resource_registry(catalog.clone());

        let first = application
            .configure_session_repository(owned_repository_input(&session_id))
            .await
            .expect("first participant");
        let configured = if replayed {
            application
                .configure_session_repository(owned_repository_input(&session_id))
                .await
                .expect("exact participant replay")
        } else {
            first
        };
        assert_eq!(
            configured.registry_provenance,
            if replayed {
                SessionParticipantProvenance::Replayed
            } else {
                SessionParticipantProvenance::Applied
            },
            "{rule} precondition"
        );
        create(repository.as_ref(), persisted(&session_id, false, "idle")).await;

        let mut command = creation_command(&session_id);
        command.repository_configurations = vec![configured.clone()];
        assert!(
            matches!(
                application.create_session(command).await,
                Err(SessionCreationError::Conflict)
            ),
            "{rule}/E1"
        );
        assert_eq!(
            catalog
                .find_repository("workspace", configured.repository_id.as_str())
                .await
                .unwrap()
                .unwrap()
                .state,
            expected_state,
            "{rule}/E2-E3"
        );
    }
}

#[tokio::test]
async fn model_override_decision_reuses_only_an_equal_id_and_resolves_every_mismatch() {
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let mut app = application(repository, Arc::new(RecordingEnvironmentSource::default()));
    let publication = awaken_session_configuration_contract::SessionModelPublication {
        primary: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            awaken_runtime_contract::resolved::ModelBinding::new(
                "override-account",
                "override-upstream",
                "genai",
            ),
        ),
        candidates: Vec::new(),
    };
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    app.set_model_publication_resolver(Arc::new(RecordingModelPublication {
        publication: publication.clone(),
        requests: requests.clone(),
    }));

    let reused = app
        .resolve_session_model_override(
            "workspace",
            "published-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("equal model id reuses the publication");
    assert!(reused.publication.is_none());
    assert!(requests.lock().expect("model requests").is_empty());

    let resolved = app
        .resolve_session_model_override(
            "workspace",
            "requested-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect("mismatched model id resolves a complete publication");
    assert_eq!(resolved.publication.as_deref(), Some(&publication));
    assert_eq!(
        requests.lock().expect("model requests").as_slice(),
        [("workspace".to_string(), "requested-model".to_string())]
    );

    let duplicate = publication.primary.clone();
    app.set_model_publication_resolver(Arc::new(FixedModelPublication {
        publication: awaken_session_configuration_contract::SessionModelPublication {
            primary: duplicate.clone(),
            candidates: vec![duplicate],
        },
    }));
    let error = app
        .resolve_session_model_override(
            "workspace",
            "malformed-model",
            "published-model",
            Default::default(),
        )
        .await
        .expect_err("malformed resolver output must fail before persistence");
    assert_eq!(error.kind, awaken_session_types::RunErrorKind::BadRequest);
    assert!(error.message.contains("duplicate model candidate"));
}

#[tokio::test]
async fn creation_driver_owns_finalize_realize_and_activation_order() {
    // Cause/effect graph: C1 complete creation inputs are present; C2 the frozen
    // placement is local; C3 durable insertion succeeds; C4 realization is
    // acknowledged. Effects: E1 finalization precedes realization, E2 the
    // Session becomes Idle, E3 one repository owner exists, and E4 one durable
    // initial-idle fact is emitted. Decision rule R1 C1+C2+C3+C4 => E1-E4.
    // FMECA: emitting E4 at intent insertion
    // creates a false-ready fact if realization later fails, severity 9,
    // occurrence 4, detection 7; the acknowledgement boundary removes that
    // failure mode. This proves protocols cannot reorder the shared sequence.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let active = app
        .create_session(creation_command("active"))
        .await
        .expect("R1");
    assert!(active.frozen_baseline().is_some(), "R1/E1");
    assert_eq!(
        active.execution,
        awaken_session_state_contract::SessionExecutionState::Idle,
        "R1/E1"
    );

    assert_eq!(
        repository.owner(&SessionId::from("active")).await.unwrap(),
        "workspace",
        "R1/E3"
    );
    let pending = repository.pending_lifecycle().await.unwrap();
    assert_eq!(pending.len(), 1, "R1/E4");
    assert_eq!(pending[0].object_id, "active", "R1/E4");
    assert_eq!(pending[0].event_type, "session.status_idled", "R1/E4");
}

#[tokio::test]
async fn create_response_contains_a_fully_durable_initial_batch() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 create carries a nonempty, fully compiled batch;
    // C2 root insertion succeeds; C3 no reconciliation step has run yet.
    // Effects: E1 the original durable row contains every Event in order with
    // stable ids and cursor zero; E2 the same row owns the batch activity epoch;
    // E3 create returns Running; E4 no follow-up authoring write is required.
    //
    // | Rule | Batch | Insert | Reconciled | Effect |
    // | I1 | nonempty | succeeds | no | E1+E2+E3+E4 |
    // | I2 | exact replay | existing equal root | no | same durable payload |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let app = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let mut command = creation_command("initial-root");
    command.initial_events = Some(
        awaken_session_event_contract::SessionInitialEventPlan::compile(
            &awaken_session_types::SessionId::from("initial-root"),
            "initial:initial-root",
            vec![
                awaken_session_event_contract::SessionEventInput::UserMessage {
                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(
                        "hello",
                    )],
                },
            ],
        )
        .expect("I1 valid plan"),
    );

    let created = app.create_session(command).await.expect("I1 create");
    assert_eq!(
        created.execution,
        awaken_session_state_contract::SessionExecutionState::Running,
        "I1/E3"
    );
    let durable = repository
        .get(&SessionId::from("initial-root"))
        .await
        .expect("I1/E1");
    assert_eq!(durable, created, "I1/E1+E4 response follows durable root");
    let batch = durable.event_batches.first().expect("I1/E1 batch");
    assert!(
        batch.events.iter().all(|entry| !entry.processed),
        "I1/C3/E1"
    );
    assert_eq!(batch.events.len(), 1, "I1/E1");
    assert!(
        durable
            .active_activity_epochs
            .contains(&batch.wake_activity_epoch.expect("I1 create wake")),
        "I1/E2"
    );
    let awaken_session_event_contract::SessionEventCommand::UserMessage {
        operation_id,
        run_id,
        ..
    } = &batch.events[0].event
    else {
        panic!("I1/E1 User Event")
    };
    assert_eq!(
        run_id,
        &awaken_session_event_contract::session_event_user_run_id(
            &awaken_session_types::SessionId::from("initial-root"),
            operation_id
        ),
        "I1/E1 stable Run id"
    );
}

#[tokio::test]
async fn self_hosted_work_dispatch_is_durable_but_not_a_fabricated_readiness_ack() {
    // Environment WorkQueue FMECA/cause-effect graph. C1 the frozen Environment
    // is self-hosted; C2 Runtime placement is a registered Worker; C3 the
    // idempotent WorkQueue projection succeeds; C4 no external worker has yet
    // acknowledged physical realization. Effects: E1 create returns the
    // durable Preparing projection without waiting for an acknowledgement that
    // this protocol does not carry; E2 exactly one stable Session work item is
    // queued; E3 no false Idle lifecycle fact is committed. Treating C3 as C4
    // previously caused every split-process create to time out (S9/O10/D2).
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | W1   | T  | T  | T  | F  | E1+E2+E3 |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let app = application_with_configuration(
        repository.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut command = creation_command("external-create");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;

    let created = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        app.create_session(command),
    )
    .await
    .expect("W1/E1 create does not invent a physical-ack barrier")
    .expect("W1/E1 durable create succeeds");
    assert_eq!(
        created.execution,
        awaken_session_state_contract::SessionExecutionState::Preparing,
        "W1/E1+E3"
    );
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["external-create".to_string()],
        "W1/E2"
    );
    assert!(
        repository.pending_lifecycle().await.unwrap().is_empty(),
        "W1/E3"
    );
}

#[tokio::test]
async fn accepted_create_is_recovered_from_the_same_preparing_root() {
    // Causes: C1 no initial batch / two ordered initial events; C2 the first
    // cold recovery scan succeeds / fails before returning authoritative rows.
    // Every row accepts a local root, drops all first-process repository and
    // Application handles, then opens a fresh file-SQLite connection and a new
    // Application. The existing time-assurance SIGKILL test owns crash-at-commit;
    // this table owns acceptance followed by cold supervisor-only recovery.
    // Effects: E1 acceptance returns the complete Preparing root before Runtime
    // effects; E2 full serialized root, owner and exact create receipt survive
    // reopen; E3 failed scan leaves root/outbox/runtime unchanged; E4 retry
    // recovers that root to Idle without a batch or Running with its same batch;
    // E5 repeated scan/replayed create do not repeat preparation or author events.
    // Rules: Q1=each C1=>E1+E2; Q2=C2 failure=>E3 then E4; Q3=success=>E4;
    // Q4=all recovered rows=>E5. SQLite and application provider-port calls
    // execute here; provider internals are simulated. This is not universal
    // Rust/SQL refinement or proof of arbitrary crash schedules.
    for initial_batch in [false, true] {
        for failed_scan in [false, true] {
            let directory = tempfile::tempdir().expect("accepted root directory");
            let path = directory.path().join("accepted.sqlite");
            let path = path.to_str().expect("test database path");
            let id = format!("accepted-create-{initial_batch}-{failed_scan}");
            let record = awaken_session_state_contract::IdempotencyRecord {
                key: format!("accept:{id}"),
                payload_hash: "complete-create-request".into(),
            };
            let make_command = || {
                let mut command = creation_command(&id);
                command.title = Some("cold acceptance".into());
                command.metadata.insert("first".into(), "retained".into());
                command
                    .metadata
                    .insert("second".into(), "independent".into());
                command.idempotency = Some(record.clone());
                if initial_batch {
                    command.initial_events = Some(
                        awaken_session_event_contract::SessionInitialEventPlan::compile(
                            &awaken_session_types::SessionId::from(id.as_str()),
                            format!("initial:{id}"),
                            ["first", "second"].into_iter().map(|text| {
                                awaken_session_event_contract::SessionEventInput::UserMessage {
                                    content: vec![awaken_agent_contract::agent::content::ContentBlock::text(text)],
                                }
                            }).collect(),
                        ).expect("complete initial plan"),
                    );
                }
                command
            };
            let accepted = {
                let repository: Arc<dyn awaken_session_repository_contract::SessionStore> =
                    Arc::new(
                        awaken_session_store::SqliteManagedSessionRepository::open(path)
                            .await
                            .expect("open original authority"),
                    );
                let runtime = Arc::new(RecordingCreateRuntime::default());
                let application = SessionApplicationBuilder::fixture(
                    runtime.clone(),
                    Arc::new(NoopMcpRealizer),
                    crate::SessionPersistence::new(repository.clone(), awaken_time::system_clock()),
                    Arc::new(RecordingEnvironmentSource::default()),
                    std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
                )
                .build()
                .expect("valid fixture Session application");
                let accepted = application
                    .accept_session(make_command())
                    .await
                    .expect("Q1 accept");
                assert_eq!(
                    accepted.execution,
                    awaken_session_state_contract::SessionExecutionState::Preparing,
                    "Q1/E1"
                );
                assert_eq!(
                    accepted.event_batches.len(),
                    usize::from(initial_batch),
                    "Q1 complete initial batch"
                );
                if let Some(plan) = make_command().initial_events {
                    let batch = &accepted.event_batches[0];
                    assert_eq!(
                        batch.batch_id, plan.batch.batch_id,
                        "Q1 stable batch identity"
                    );
                    assert_eq!(
                        batch.events, plan.batch.events,
                        "Q1 two original ordered events"
                    );
                    let wake = batch.wake_activity_epoch.expect("Q1 create wake receipt");
                    assert!(
                        accepted.active_activity_epochs.contains(&wake),
                        "Q1 wake is active"
                    );
                    assert_eq!(
                        accepted.active_activity_epochs.len(),
                        1,
                        "Q1 only initial wake"
                    );
                } else {
                    assert!(
                        accepted.active_activity_epochs.is_empty(),
                        "Q1 no fabricated wake"
                    );
                }
                assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "Q1/E1");
                assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "Q1/E1");
                assert_eq!(
                    serde_json::to_value(repository.get(&SessionId::from(&id)).await.unwrap())
                        .unwrap(),
                    serde_json::to_value(&accepted).unwrap(),
                    "Q1/E2 complete committed response"
                );
                assert!(
                    repository.pending_lifecycle().await.unwrap().is_empty(),
                    "Q1 no fabricated readiness"
                );
                accepted
            };
            let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(path)
                    .await
                    .expect("reopen fresh authority connection"),
            );
            let frozen = serde_json::to_value(&accepted).unwrap();
            assert_eq!(
                serde_json::to_value(repository.get(&SessionId::from(&id)).await.unwrap()).unwrap(),
                frozen,
                "Q1/E2 reopened complete root"
            );
            assert_eq!(
                repository.owner(&SessionId::from(&id)).await.unwrap(),
                "workspace",
                "Q1/E2 owner"
            );
            assert_eq!(
                repository
                    .replay_create("workspace", &SessionId::from(&id), &record)
                    .await
                    .unwrap(),
                Some(accepted.clone()),
                "Q1/E2 retained exact receipt"
            );
            let outbox =
                serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap();
            let faulting = Arc::new(FaultingSessionRepository::new(repository.clone()));
            let runtime = Arc::new(RecordingCreateRuntime::default());
            let application = SessionApplicationBuilder::fixture(
                runtime.clone(),
                Arc::new(NoopMcpRealizer),
                crate::SessionPersistence::new(faulting.clone(), awaken_time::system_clock()),
                Arc::new(RecordingEnvironmentSource::default()),
                std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
            )
            .build()
            .expect("valid fixture Session application");
            if failed_scan {
                faulting.fail_recovery_scan_once();
                let failed = application.reconcile_session_realizations().await;
                assert_eq!(failed.failures.len(), 1, "Q2/E3 exact failed scan");
                assert_eq!(
                    serde_json::to_value(repository.get(&SessionId::from(&id)).await.unwrap())
                        .unwrap(),
                    frozen,
                    "Q2/E3 full root stutter"
                );
                assert_eq!(
                    serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap(),
                    outbox,
                    "Q2/E3 outbox stutter"
                );
                assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 0, "Q2/E3");
                assert_eq!(runtime.preparations.load(Ordering::SeqCst), 0, "Q2/E3");
            }
            let recovery = application.reconcile_session_realizations().await;
            assert!(
                recovery.failures.is_empty(),
                "Q3/E4: {:?}",
                recovery.failures
            );
            let ready = repository.get(&SessionId::from(&id)).await.unwrap();
            assert_eq!(
                ready.execution,
                if initial_batch {
                    awaken_session_state_contract::SessionExecutionState::Running
                } else {
                    awaken_session_state_contract::SessionExecutionState::Idle
                },
                "Q3/E4"
            );
            assert_eq!(
                ready.frozen_baseline(),
                accepted.frozen_baseline(),
                "Q3 original baseline"
            );
            assert_eq!(ready.title, accepted.title, "Q3 title");
            assert_eq!(ready.metadata, accepted.metadata, "Q3 metadata");
            assert_eq!(
                ready.event_batches, accepted.event_batches,
                "Q3 full initial event identity/order"
            );
            assert_eq!(
                ready.active_activity_epochs, accepted.active_activity_epochs,
                "Q3 original wake receipts"
            );
            assert_eq!(
                ready.activity_epoch, accepted.activity_epoch,
                "Q3 wake high-water"
            );
            assert_eq!(
                runtime.baseline_installs.load(Ordering::SeqCst),
                2,
                "Q3 stage/final projection"
            );
            assert_eq!(
                runtime.preparations.load(Ordering::SeqCst),
                1,
                "Q3 one preparation"
            );
            let ready_json = serde_json::to_value(&ready).unwrap();
            let ready_outbox =
                serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap();
            let repeated = application.reconcile_session_realizations().await;
            assert!(repeated.failures.is_empty(), "Q4 repeated scan");
            let replay = application
                .accept_session(make_command())
                .await
                .expect("Q4 exact replay");
            assert_eq!(
                serde_json::to_value(replay).unwrap(),
                ready_json,
                "Q4 complete replay receipt"
            );
            assert_eq!(
                serde_json::to_value(repository.get(&SessionId::from(id)).await.unwrap()).unwrap(),
                ready_json,
                "Q4 complete root frame"
            );
            assert_eq!(
                serde_json::to_value(repository.pending_lifecycle().await.unwrap()).unwrap(),
                ready_outbox,
                "Q4 outbox frame"
            );
            assert_eq!(
                runtime.baseline_installs.load(Ordering::SeqCst),
                2,
                "Q4 no extra projection"
            );
            assert_eq!(
                runtime.preparations.load(Ordering::SeqCst),
                1,
                "Q4 no extra preparation"
            );
        }
    }
}

#[tokio::test]
async fn exact_create_replay_returns_durable_root_without_repeating_external_effects() {
    // Application create-replay cause/effect table. C1 receipt absent creates a
    // Worker-owned Session; C2 the same receipt is retried after a different
    // lowering; C3 the durable root already advanced through activation. Effects:
    // E1 first create installs/prepares once, performs one activation CAS, and
    // dispatches one Work item; E2 replay returns the exact current durable root;
    // E3 Runtime, activation revision, WorkQueue, and outbox counts do not change;
    // C4 a concurrent winner has durably entered ActivationFailed before the
    // replay result is returned; E4 the loser receives typed terminal conflict
    // and still performs no external effect; C5 an AcceptDurableRoot caller
    // replays that same failed operation; E5 it receives the failed root without
    // changing the historical await-realization contract or repeating effects.
    //
    // | Rule | Receipt | Lowering | Durable root | Effect |
    // |---|---|---|---|---|
    // | R1 | absent | first | absent | E1 |
    // | R2 | exact | changed | activated | E2+E3 |
    // | R3 | exact | same | ActivationFailed | E3+E4 |
    // | R4 | exact accept | same | ActivationFailed | E3+E5 |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("create replay repository"),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(repository.clone(), awaken_time::system_clock()),
        environments.clone(),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::RegisteredWorker,
        ..Default::default()
    })
    .build()
    .expect("valid fixture Session application");
    let idempotency = awaken_session_state_contract::IdempotencyRecord {
        key: "create-replay-effects".into(),
        payload_hash: "stable-product-request".into(),
    };
    let mut first = creation_command("create-replay-effects");
    first.intent.control.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    first.idempotency = Some(idempotency.clone());
    let created = application.create_session(first).await.expect("R1/E1");
    let durable_before = repository
        .get(&created.session_id)
        .await
        .expect("R1 durable");
    let revision_before = durable_before.revision;
    let outbox_before = repository.pending_lifecycle().await.expect("R1 outbox");
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R1/E1");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R1/E1"
    );

    let mut replay = creation_command("create-replay-effects");
    replay.intent.control.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    replay.title = Some("changed lowering must not replace durable truth".into());
    replay.idempotency = Some(idempotency);
    let replayed = application.create_session(replay).await.expect("R2/E2");
    assert_eq!(replayed, durable_before, "R2/E2");
    assert_eq!(
        repository.get(&replayed.session_id).await.unwrap().revision,
        revision_before,
        "R2/E3 no second activation CAS"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R2/E3");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R2/E3"
    );
    assert_eq!(
        repository.pending_lifecycle().await.unwrap(),
        outbox_before,
        "R2/E3"
    );

    let mut failed = repository.get(&replayed.session_id).await.unwrap();
    let expected_revision = failed.revision;
    failed.execution = awaken_session_state_contract::SessionExecutionState::ActivationFailed;
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(failed);
    let payload_hash = payload.stable_hash();
    assert!(matches!(
        repository
            .commit(
                "workspace",
                awaken_session_state_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_state_contract::IdempotencyRecord {
                        key: "test:create-replay-activation-failed".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .unwrap(),
        awaken_session_state_contract::SessionMutationResult::Applied { .. }
    ));
    let mut terminal_replay = creation_command("create-replay-effects");
    terminal_replay.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    terminal_replay.idempotency = Some(awaken_session_state_contract::IdempotencyRecord {
        key: "create-replay-effects".into(),
        payload_hash: "stable-product-request".into(),
    });
    assert!(
        matches!(
            application.create_session(terminal_replay).await,
            Err(SessionCreationError::Tombstoned)
        ),
        "R3/E4"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R3/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R3/E3");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R3/E3"
    );

    let mut accepted_terminal_replay = creation_command("create-replay-effects");
    accepted_terminal_replay.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;
    accepted_terminal_replay.idempotency = Some(awaken_session_state_contract::IdempotencyRecord {
        key: "create-replay-effects".into(),
        payload_hash: "stable-product-request".into(),
    });
    let accepted_failed = application
        .accept_session(accepted_terminal_replay)
        .await
        .expect("R4/E5 accepted failed operation root");
    assert_eq!(
        accepted_failed.execution,
        awaken_session_state_contract::SessionExecutionState::ActivationFailed,
        "R4/E5"
    );
    assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 1, "R4/E3");
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R4/E3");
    assert_eq!(
        environments.dispatch_calls.load(Ordering::SeqCst),
        1,
        "R4/E3"
    );
}

#[tokio::test]
async fn committed_create_conflicts_recover_only_through_the_exact_receipt() {
    // Post-create conflict cause/effect graph. C1 the caller omits/provides a
    // stable create receipt; C2 the realization acknowledgement or final
    // activation root commits but its response is reported as Conflict; C3 a
    // stable receipt is replayed with the same/different payload hash. Effects:
    // E1 create returns the exact current durable root; E2 Runtime realization
    // is not repeated; E3 exact replay returns that root without another root
    // mutation; E4 conflicting payload is rejected without effects. Constraint:
    // recovery reads the repository's create receipt and never retries the
    // public POST, installs a process lock, or treats Conflict as success when
    // that exact receipt is absent.
    //
    // | Rule | Receipt | Lost acknowledgement | Replay hash | Effect |
    // |---|---|---|---|---|
    // | R1 | internal | realization ACK | - | E1+E2 |
    // | R2 | internal | final activation | - | E1+E2 |
    // | R3 | stable | realization ACK | exact | E1+E2+E3 |
    // | R4 | stable | realization ACK | different | E4 |
    // | R5 | internal | begin unavailable | - | E1, lifecycle owns retry |
    for (
        rule,
        session_id,
        fault_operation,
        commit_before_failure,
        stable_receipt,
        expected_installs,
    ) in [
        (
            "R1",
            "create-ack-conflict-internal",
            "acknowledge-session-realization",
            true,
            false,
            2,
        ),
        (
            "R2",
            "create-activation-conflict-internal",
            "session:activate:",
            true,
            false,
            2,
        ),
        (
            "R3",
            "create-ack-conflict-stable",
            "acknowledge-session-realization",
            true,
            true,
            2,
        ),
        (
            "R5",
            "create-begin-unavailable-internal",
            "begin-session-realization",
            false,
            false,
            0,
        ),
    ] {
        let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("post-create conflict repository"),
        );
        let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
        if commit_before_failure {
            faulting.commit_then_conflict_once(fault_operation);
        } else {
            faulting.fail_once(fault_operation);
        }
        let runtime = Arc::new(RecordingCreateRuntime::default());
        let application = SessionApplicationBuilder::fixture(
            runtime.clone(),
            Arc::new(NoopMcpRealizer),
            crate::SessionPersistence::new(faulting, awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .build()
        .expect("valid fixture Session application");
        let stable = stable_receipt.then(|| awaken_session_state_contract::IdempotencyRecord {
            key: "public-create-key".into(),
            payload_hash: "public-create-payload".into(),
        });
        let mut command = creation_command(session_id);
        command.idempotency = stable.clone();

        let created = application.create_session(command).await.expect(rule);
        let durable_after_create = durable
            .get(&SessionId::from(session_id))
            .await
            .expect("E1 durable root");
        assert_eq!(created, durable_after_create, "{rule}/E1");
        assert_eq!(
            created.execution.admits_activity(),
            expected_installs > 0,
            "{rule}/E1"
        );
        assert_eq!(
            runtime.baseline_installs.load(Ordering::SeqCst),
            expected_installs,
            "{rule}/E2"
        );
        assert_eq!(
            runtime.preparations.load(Ordering::SeqCst),
            usize::from(expected_installs > 0),
            "{rule}/E2"
        );

        if let Some(stable) = stable {
            let revision = created.revision;
            let mut replay = creation_command(session_id);
            replay.idempotency = Some(stable.clone());
            assert_eq!(
                application.create_session(replay).await.expect("R3/E3"),
                created,
                "R3/E3"
            );
            assert_eq!(
                durable
                    .get(&SessionId::from(session_id))
                    .await
                    .unwrap()
                    .revision,
                revision,
                "R3/E3"
            );
            assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 2, "R3/E3");
            assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R3/E3");

            let mut mismatch = creation_command(session_id);
            mismatch.idempotency = Some(awaken_session_state_contract::IdempotencyRecord {
                key: stable.key,
                payload_hash: "different-public-create-payload".into(),
            });
            assert!(
                matches!(
                    application.create_session(mismatch).await,
                    Err(SessionCreationError::IdempotencyMismatch)
                ),
                "R4/E4"
            );
            assert_eq!(runtime.baseline_installs.load(Ordering::SeqCst), 2, "R4/E4");
            assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "R4/E4");
        }
    }
}

#[tokio::test]
async fn create_failure_cleanup_is_authorized_only_by_a_terminal_durable_root() {
    // Cleanup-admission cause/effect graph. C1 projection failure is retryable
    // unavailable/nonretryable classified internal/unclassified internal (a
    // bounded retry); C2 the canonical failure command leaves
    // the root nonterminal/commits ActivationFailed. Effects: E1 both creates
    // retain the original effect error; E2 a nonterminal root keeps cleanup
    // unfenced for lifecycle retry; E3 only the terminal root enters and fences
    // the existing cleanup protocol (later phases may remain recoverable).
    //
    // | Rule | Effect | Durable root | Cleanup |
    // |---|---|---|---|
    // | F1 | unavailable | nonterminal | E1+E2 |
    // | F2 | classified internal | ActivationFailed | E1+E3 |
    // | F3 | unclassified internal | nonterminal | E1+E2 |
    for (rule, failure, terminal) in [("F1", 1, false), ("F2", 2, true), ("F3", 3, false)] {
        let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .await
                .expect("cleanup admission repository"),
        );
        let runtime = Arc::new(RecordingCreateRuntime::default());
        runtime.projection_failure.store(failure, Ordering::SeqCst);
        let application = SessionApplicationBuilder::fixture(
            runtime,
            Arc::new(NoopMcpRealizer),
            crate::SessionPersistence::new(repository.clone(), awaken_time::system_clock()),
            Arc::new(RecordingEnvironmentSource::default()),
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
        .build()
        .expect("valid fixture Session application");
        let session_id = format!("create-cleanup-{rule}");

        assert!(
            matches!(
                application
                    .create_session(creation_command(&session_id))
                    .await,
                Err(SessionCreationError::Rejected(_))
            ),
            "{rule}/E1"
        );
        let durable = repository
            .get(&SessionId::from(session_id))
            .await
            .expect("durable root");
        assert_eq!(durable.execution.is_terminal(), terminal, "{rule}");
        assert_eq!(
            durable.terminal_cleanup.is_fenced(),
            terminal,
            "{rule}/E2-E3"
        );
    }
}

#[tokio::test]
async fn creation_persists_only_a_complete_frozen_root_before_later_cas_failure() {
    /* Cause/effect graph: C1 all creation inputs compile; C2 the first root
     * insert commits; C3 the later activation CAS is unavailable. Effects: E1
     * create reports retryable failure; E2 durable truth is already Frozen with
     * its Resource/MCP state; E3 no Preparing authoring intent can be stranded.
     * Constraint: physical realization and Work projection remain later,
     * replayable effects.
     *
     * | Rule | Compile | Insert | Later CAS | Result |
     * |---|---|---|---|---|
     * | A1 | pass | pass | fail | E1 + E2 + E3 |
     */
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let faulting = Arc::new(FaultingSessionRepository::new(durable.clone()));
    faulting.fail_once("activate");
    let app = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let mut command = creation_command("atomic-create");
    command.intent.control.runtime_placement =
        awaken_session_types::SessionRuntimePlacement::Worker;

    assert!(app.create_session(command).await.is_err(), "A1/E1");
    let committed = durable
        .get(&SessionId::from("atomic-create"))
        .await
        .expect("A1/E2");
    assert!(committed.frozen_baseline().is_some(), "A1/E2+E3");
    assert!(committed.resources.pending.is_some(), "A1/E2");
}

#[tokio::test]
async fn session_application_admits_new_and_existing_protocol_threads() {
    // Cause/effect graph: C1 durable thread absent/present; C2 workspace matches
    // the durable owner; C3 Agent is available. Effects: E1 absent creates one
    // frozen Session; E2 present reuses and realizes that same aggregate; E3 an
    // owner mismatch fails before Runtime execution; C4 the canonical full
    // workspace feature graph expands the profiled creation future. Effects:
    // E1 absent creates one frozen Session; E2 present reuses and realizes that
    // same aggregate; E3 owner mismatch fails before Runtime execution; E4 the
    // profiled creation boundary completes on the default Tokio worker stack.
    // Decision table: A1 absent+C3+C4 => E1+E4; A2 present+C2 => E2; A3
    // present+!C2 => E3. The canonical full Rust gate owns C4; this test is the
    // concrete application gate shared by AI SDK, AG-UI, and A2A.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("test repository"),
    );
    let app = application(repository.clone(), Arc::new(AdmissionEnvironment));

    app.admit_run_session("workspace", "thread", "assistant")
        .await
        .expect("A1");
    let created = repository
        .get(&SessionId::from("thread"))
        .await
        .expect("A1/E1");
    assert!(created.frozen_baseline().is_some(), "A1/E1");
    assert_eq!(created.agent_id(), Some("assistant"), "A1/E1");

    app.admit_run_session("workspace", "thread", "ignored")
        .await
        .expect("A2/E2");
    let error = app
        .admit_run_session("other-workspace", "thread", "ignored")
        .await
        .expect_err("A3/E3");
    assert_eq!(
        error.kind,
        awaken_session_types::RunErrorKind::BadRequest,
        "A3/E3"
    );
}

fn profiled_session_command(session_id: &str, model: Option<&str>) -> CreateProfiledSessionCommand {
    CreateProfiledSessionCommand {
        owner_scope: "workspace".into(),
        session_id: session_id.into(),
        mutation_policy: crate::session_resource::SessionMutationPolicy::Managed,
        application_origin: None,
        agent_id: "profiled".into(),
        source_revision: None,
        environment_id: None,
        model: model.map(str::to_owned),
        mounts: vec![profiled_mount("workspace")],
        env: vec![profiled_env("PROJECT")],
        prompts: vec!["project context".into()],
        resource_inputs: Vec::new(),
        mcp_candidates: vec![
            McpAttachmentCandidate {
                name: "session-only".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://session-only.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            },
            McpAttachmentCandidate {
                name: "shared".into(),
                target: McpAttachmentCandidateTarget::HttpUrl(
                    "https://session-shared.example.test/mcp".into(),
                ),
                prompts_as_skills: false,
                published_credential: None,
                origin: crate::session_mcp::McpAttachmentOrigin::Session,
            },
        ],
        repositories: Vec::new(),
        network_restriction: Some(
            awaken_session_configuration_contract::SessionNetworkPolicy::None,
        ),
        title: None,
        metadata: Default::default(),
        tools: None,
        idempotency: None,
    }
}

async fn assert_profiled_realization_and_environment_rules(
    repository: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mut explicit_environment = profiled_session_command("profiled-realized", None);
    explicit_environment.environment_id = Some("project-environment".into());
    // Origin cause/effect rules O1/O2: a valid application carrier is supplied
    // before create -> the same complete durable baseline retains it on
    // readback; it is never appended to model-visible context. This assertion
    // shares the existing real persistence matrix rather than a shadow store.
    let origin = awaken_session_configuration_contract::ApplicationOriginBinding::new(
        "application.interaction:v1".into(),
        "a".repeat(64),
        "opaque-origin-proof.not-model-context".into(),
    )
    .unwrap();
    explicit_environment.application_origin = Some(origin.clone());
    let realized = available
        .create_profiled_session(explicit_environment)
        .await
        .expect("P1");
    assert_eq!(realized.model(), Some("published-model"), "P1/E1");
    assert_eq!(
        realized.execution,
        awaken_session_state_contract::SessionExecutionState::Idle,
        "P1/E1"
    );
    let baseline = realized.frozen_baseline().expect("P1 frozen baseline");
    assert_eq!(baseline.application_origin.as_ref(), Some(&origin), "O1");
    assert_eq!(
        repository
            .get(&realized.session_id)
            .await
            .unwrap()
            .frozen_baseline()
            .unwrap()
            .application_origin
            .as_ref(),
        Some(&origin),
        "O1 persisted readback"
    );
    assert!(
        !baseline
            .prompts
            .iter()
            .any(|prompt| prompt.contains(origin.evidence())),
        "O2 no model context"
    );
    assert_eq!(baseline.mounts.len(), 1, "P1/E1");
    assert_eq!(baseline.env.len(), 1, "P1/E1");
    assert_eq!(
        baseline.prompts,
        [
            "project context".to_owned(),
            awaken_session_configuration_contract::SessionContextSeed::from_metadata(
                awaken_session_configuration_contract::SessionTrigger::Interactive {},
                &Default::default(),
                None,
            )
            .unwrap()
            .encode()
        ],
        "P1/E1 product prompt + one frozen context seed"
    );
    assert_eq!(
        baseline.environment.environment_id, "project-environment",
        "P1/E1"
    );
    assert_eq!(
        baseline.environment.network,
        awaken_session_configuration_contract::SessionNetworkPolicy::None,
        "P1/E1"
    );
    // O3/O4: metadata can be patched or cleared, including application-looking
    // principal/proof labels, but those presentation writes cannot replace the
    // original baseline carrier. Observe the real root CAS and its readback;
    // hosting attribution must consume that root, never these mutable labels.
    for (index, metadata) in [
        SessionMetadataUpdate::Patch(std::collections::BTreeMap::from([
            (
                "flow.interaction_principal".into(),
                Some("account:another".into()),
            ),
            (
                "application_origin".into(),
                Some("forged-metadata-proof".into()),
            ),
        ])),
        SessionMetadataUpdate::Clear,
    ]
    .into_iter()
    .enumerate()
    {
        let before = repository.get(&realized.session_id).await.unwrap();
        let updated = available
            .update_session(
                &realized.session_id,
                SessionUpdateCommand {
                    title: None,
                    metadata: Some(metadata),
                    budget: None,
                    tools: None,
                    mcp_update: None,
                    idempotency_key: Some(format!("origin-metadata-{index}")),
                    request_fingerprint: format!("origin-metadata-{index}"),
                    if_match: Some(before.revision),
                },
            )
            .await
            .expect("O3/O4 metadata mutation");
        let after = repository.get(&realized.session_id).await.unwrap();
        assert_eq!(
            after.revision, updated.command_revision,
            "O3/O4 committed receipt"
        );
        assert_eq!(
            after.frozen_baseline(),
            before.frozen_baseline(),
            "O3/O4 immutable origin and fingerprint"
        );
        assert_eq!(
            after.frozen_baseline().unwrap().application_origin.as_ref(),
            Some(&origin),
            "O3/O4 not metadata-derived"
        );
    }
    // N7: blank-only Environment and request restrictions previously froze an
    // Allowlist([""]) instead of canonical None. SQLite must persist
    // the exact deny fact; replay/projection must not reconstruct another form.
    let mut blank = profiled_session_command("profiled-blank-network", None);
    blank.environment_id = Some("blank-network".into());
    blank.network_restriction = Some(
        awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
            hosts: vec!["\n".into(), " ".into()],
        },
    );
    let denied = available
        .create_profiled_session(blank)
        .await
        .expect("N7 created");
    let reloaded = repository
        .get(&denied.session_id)
        .await
        .expect("N7 durable Session");
    assert_eq!(
        reloaded
            .frozen_baseline()
            .expect("N7 frozen")
            .environment
            .network,
        awaken_session_configuration_contract::SessionNetworkPolicy::None,
        "N7 must remain canonical after persistence"
    );
    let mcp = realized
        .mcp
        .attachments
        .iter()
        .map(|attachment| {
            (
                attachment.name.as_str(),
                attachment.target.http_url().expect("HTTP MCP target"),
                attachment.origin,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mcp,
        [
            (
                "agent-only",
                "https://agent-only.example.test/mcp",
                crate::session_mcp::McpAttachmentOrigin::Agent,
            ),
            (
                "session-only",
                "https://session-only.example.test/mcp",
                crate::session_mcp::McpAttachmentOrigin::Session,
            ),
            (
                "shared",
                "https://session-shared.example.test/mcp",
                crate::session_mcp::McpAttachmentOrigin::Session,
            ),
        ],
        "P1/E3"
    );

    let mut substituted_environment =
        profiled_session_command("profiled-environment-substitution", None);
    substituted_environment.environment_id = Some("substitute-me".into());
    assert!(
        available
            .create_profiled_session(substituted_environment)
            .await
            .is_err(),
        "an Environment resolver cannot substitute another identity"
    );
    assert!(
        matches!(
            repository
                .get(&SessionId::from("profiled-environment-substitution"))
                .await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "Environment substitution fails before Session persistence"
    );
}

async fn assert_profiled_publication_revision_rules(
    repository: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mut historical_command = profiled_session_command("profiled-historical", None);
    historical_command.source_revision = Some(7);
    let historical = available
        .create_profiled_session(historical_command)
        .await
        .expect("exact historical publication");
    assert_eq!(historical.model(), Some("historical-model"));
    assert_eq!(
        historical
            .frozen_baseline()
            .expect("historical baseline")
            .agent_revision,
        Some(7),
        "the requested revision, not current or zero, is frozen"
    );
    // Exact-publication projection decision table:
    // | baseline revision | catalog snapshot | placement | effect |
    // | exact             | exact            | any       | project snapshot |
    // | exact             | missing          | Worker    | fail before effect |
    // The first assertion proves the sole profile source supplies the complete
    // immutable publication; P2 below covers the missing-snapshot Worker rule.
    let historical_projection = available
        .frozen_session_projection("workspace".into(), &historical, true)
        .await
        .expect("exact historical projection");
    // P1 exact historical publication + inherited model/prompt + explicit empty
    // Skill pins over an already-empty selection => preserve the complete
    // catalog publication, including its consistency token and metadata.
    // O1 below independently changes the model and requires the derived
    // fingerprint; P2 rejects a missing Worker publication before effects.
    let publication = historical_projection.agent_publication.as_ref().unwrap();
    let source_publication =
        awaken_executable_agent_contract::ExecutableAgentProfileSource::executable_snapshot_at_revision_in(
            &ProfiledAgent { unavailable: false },
            "workspace",
            "profiled",
            7,
        )
        .expect("P1 exact catalog publication");
    assert_eq!(publication.root_agent_id.0, "profiled");
    assert_eq!(publication.metadata.source.revision, 7);
    assert!(
        publication
            .resolved_spec
            .plugin_config
            .agent
            .skills
            .is_empty()
    );
    assert_eq!(publication, &source_publication, "P1 complete no-op");

    let mut missing_command = profiled_session_command("profiled-missing", None);
    missing_command.source_revision = Some(8);
    assert!(
        available
            .create_profiled_session(missing_command)
            .await
            .is_err(),
        "an unproven exact publication fails closed"
    );

    let mut substituting = application(repository.clone(), Arc::new(AdmissionEnvironment));
    substituting.set_config_source(Arc::new(SubstitutingProfileSource));
    let mut substituted_command = profiled_session_command("profiled-substituted-revision", None);
    substituted_command.source_revision = Some(7);
    assert!(
        substituting
            .create_profiled_session(substituted_command)
            .await
            .is_err(),
        "a profile source cannot substitute another revision"
    );
    assert!(
        matches!(
            repository
                .get(&SessionId::from("profiled-substituted-revision"))
                .await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "revision substitution must fail before Session persistence"
    );

    let mut worker = application_with_configuration(
        repository.clone(),
        Arc::new(AdmissionEnvironment),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    worker.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    let mut profile_without_snapshot =
        profiled_session_command("profiled-worker-missing-snapshot", None);
    profile_without_snapshot.source_revision = Some(9);
    assert!(
        worker
            .create_profiled_session(profile_without_snapshot)
            .await
            .is_err(),
        "P2: a Worker effect cannot start without the complete immutable publication"
    );
}

async fn assert_profiled_override_projection(
    repository: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    // Override-projection cause/effect table:
    // | Rule | Agent snapshot | Override publication | Effect |
    // | O1 | exact revision | complete | replace the whole candidate roster and identity |
    // | O2 | exact revision | none/equal | preserve the exact Agent snapshot (above) |
    // O1 also proves the original Agent route cannot leak into the derived
    // executable snapshot and all three fingerprint fields move together.
    let primary = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "override-account",
            "override-upstream",
            "acp:claude",
        ),
    );
    let fallback = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
        awaken_runtime_contract::resolved::ModelBinding::new(
            "override-fallback",
            "override-upstream",
            "genai",
        ),
    );
    let mut override_application = application(repository.clone(), Arc::new(AdmissionEnvironment));
    override_application.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    override_application.set_model_publication_resolver(Arc::new(FixedModelPublication {
        publication: awaken_session_configuration_contract::SessionModelPublication {
            primary: primary.clone(),
            candidates: vec![fallback.clone()],
        },
    }));
    let mut override_command =
        profiled_session_command("profiled-override", Some("override-public-id"));
    override_command.source_revision = Some(7);
    let overridden = override_application
        .create_profiled_session(override_command)
        .await
        .expect("O1 admitted override");
    let override_projection = override_application
        .frozen_session_projection("workspace".into(), &overridden, true)
        .await
        .expect("O1 frozen projection");
    let snapshot = override_projection
        .agent_publication
        .expect("O1 executable Agent snapshot");
    assert_eq!(snapshot.resolved_spec.model_binding, primary, "O1");
    assert_eq!(snapshot.resolved_spec.model_candidates, [fallback], "O1");
    assert_eq!(
        snapshot.fingerprint, snapshot.resolved_spec.catalog_fingerprint,
        "O1"
    );
    assert_eq!(
        snapshot.fingerprint.0, snapshot.metadata.fingerprint.0,
        "O1"
    );
    assert_ne!(snapshot.fingerprint.0, "profiled-revision-7", "O1");
    assert!(
        snapshot
            .resolved_spec
            .execution_candidates(None)
            .into_iter()
            .all(|candidate| candidate.binding().identity().0.starts_with("override-")),
        "O1 original Agent route must be absent"
    );
}

async fn assert_profiled_rejection_rules(
    repository: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let mut available = application(repository.clone(), Arc::new(AdmissionEnvironment));
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));

    let mismatched = available
        .create_profiled_session(profiled_session_command(
            "profiled-mismatch",
            Some("unpublished-model"),
        ))
        .await;
    assert!(mismatched.is_err(), "P2/E2");
    assert!(
        matches!(
            repository.get(&SessionId::from("profiled-mismatch")).await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "P2/E2"
    );

    let mut unavailable = application(repository.clone(), Arc::new(AdmissionEnvironment));
    unavailable.set_config_source(Arc::new(ProfiledAgent { unavailable: true }));
    assert!(
        unavailable
            .create_profiled_session(profiled_session_command("profiled-unavailable", None))
            .await
            .is_err(),
        "P3/E2"
    );
    assert!(
        matches!(
            repository
                .get(&SessionId::from("profiled-unavailable"))
                .await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "P3/E2"
    );

    let mut conflicting = profiled_session_command("profiled-mcp-conflict", None);
    conflicting.mcp_candidates.push(McpAttachmentCandidate {
        name: "session-only".into(),
        target: McpAttachmentCandidateTarget::HttpUrl(
            "https://duplicate-session.example.test/mcp".into(),
        ),
        prompts_as_skills: false,
        published_credential: None,
        origin: crate::session_mcp::McpAttachmentOrigin::Session,
    });
    assert!(
        available
            .create_profiled_session(conflicting)
            .await
            .is_err(),
        "P4/E2"
    );
    assert!(
        matches!(
            repository
                .get(&SessionId::from("profiled-mcp-conflict"))
                .await,
            Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
        ),
        "P4/E2"
    );
}

async fn assert_profiled_direct_resource_rules() {
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("direct resource repository"),
    );
    let capture = Arc::new(FaultingSessionRepository::new(durable.clone()));
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let mut available = application_with_runtime(
        runtime.clone(),
        capture.clone(),
        Arc::new(AdmissionEnvironment),
    );
    available.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    let mut valid = profiled_session_command("profiled-direct-file", None);
    valid.resource_inputs.push(direct_file_attachment(
        "direct-file",
        "file-direct",
        "/workspace/direct-file",
    ));
    available
        .create_profiled_session(valid)
        .await
        .expect("D2 valid direct File");
    let roots = capture.applied_create_roots();
    assert_eq!(roots.len(), 1, "D2 one original root");
    assert_eq!(
        roots[0].revision,
        awaken_session_types::SessionRevision(1),
        "D2 original root revision"
    );
    assert!(
        roots[0].resources.desired().inputs().iter().any(|input| {
            input.binding_id.as_str() == "direct-file"
                && matches!(
                    &input.source,
                    crate::session_resource::ResolvedInputSource::File { file_id }
                        if file_id.as_str() == "file-direct"
                )
        }),
        "D2 direct File desired intent freezes in original root before realization"
    );

    let agent_repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("Agent collision repository"),
    );
    let agent_runtime = Arc::new(RecordingCreateRuntime::default());
    let mut agent_collision = application_with_runtime(
        agent_runtime.clone(),
        agent_repo.clone(),
        Arc::new(AdmissionEnvironment),
    );
    agent_collision.set_config_source(Arc::new(ResourceProfiledAgent));
    let mut collides_agent = profiled_session_command("profiled-direct-agent-collision", None);
    collides_agent.resource_inputs.push(direct_file_attachment(
        "direct-collides-agent",
        "file-collides-agent",
        "/workspace/agent-input",
    ));
    assert!(
        agent_collision
            .create_profiled_session(collides_agent)
            .await
            .is_err(),
        "D3 Agent collision"
    );
    assert_eq!(
        agent_repo
            .get(&SessionId::from("profiled-direct-agent-collision"))
            .await,
        Err(awaken_session_state_contract::SessionRepositoryError::NotFound),
        "D3 no root"
    );
    assert_eq!(
        agent_runtime.baseline_installs.load(Ordering::SeqCst),
        0,
        "D3"
    );
    assert_eq!(agent_runtime.preparations.load(Ordering::SeqCst), 0, "D3");

    // Final-path cause/effect rules: D4 gives a File and Repository the same
    // authored spelling, but the Runtime projects the File below
    // `/mnt/session/uploads`, so creation succeeds and the canonical
    // Stage-to-Publish sequence installs two complete phase projections while
    // preparing physical state once. D5 gives two Repository trees a
    // parent/child overlap; because both paths are already final, admission
    // rejects the whole set before Registry or Runtime effects.
    let repository_repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("Repository path Session store"),
    );
    let repository_runtime = Arc::new(RecordingCreateRuntime::default());
    let registry = ephemeral_resource_registry().await;
    let mut repository_application = application_with_runtime(
        repository_runtime.clone(),
        repository_repo.clone(),
        Arc::new(AdmissionEnvironment),
    );
    repository_application.set_config_source(Arc::new(ProfiledAgent { unavailable: false }));
    repository_application.set_resource_registry(registry.clone());
    let disjoint_session = "profiled-direct-projected-disjoint";
    let disjoint_repository_id = format!("profiled:{disjoint_session}:repository:0");
    let mut projected_disjoint = profiled_session_command(disjoint_session, None);
    projected_disjoint
        .resource_inputs
        .push(direct_file_attachment(
            "direct-file-shared-spelling",
            "file-shared-spelling",
            "/workspace/repository",
        ));
    projected_disjoint.repositories = vec![crate::ProfiledSessionRepositoryInput {
        binding_id: awaken_resource_contract::BindingId::new(
            "profiled-disjoint-repository-binding",
        ),
        repository: SessionRepositoryResourceInput {
            id: disjoint_repository_id.clone(),
            workspace_id: "workspace".into(),
            name: "Projected-disjoint Repository".into(),
            description: "File spelling is projected to another final tree".into(),
            remote_url: "https://example.test/disjoint.git".into(),
            credential_material: None,
            credential: None,
            mount_path: "/workspace/repository".into(),
            initial_branch: Some("main".into()),
            initial_commit: None,
        },
    }];
    repository_application
        .create_profiled_session(projected_disjoint)
        .await
        .expect("D4 projected paths are disjoint");
    assert!(
        registry
            .find_repository("workspace", &disjoint_repository_id)
            .await
            .expect("D4 inventory")
            .is_some(),
        "D4 Repository configuration is admitted"
    );
    assert_eq!(
        repository_runtime.baseline_installs.load(Ordering::SeqCst),
        2,
        "D4 Stage and Publish install two complete phase projections"
    );
    assert_eq!(
        repository_runtime.preparations.load(Ordering::SeqCst),
        1,
        "D4 one physical preparation"
    );

    let overlapping_session = "profiled-direct-repository-overlap";
    let repository_input =
        |index: usize, binding: &str, mount_path: &str| crate::ProfiledSessionRepositoryInput {
            binding_id: awaken_resource_contract::BindingId::new(binding),
            repository: SessionRepositoryResourceInput {
                id: format!("profiled:{overlapping_session}:repository:{index}"),
                workspace_id: "workspace".into(),
                name: format!("Overlapping Repository {index}"),
                description: "must fail before registration".into(),
                remote_url: format!("https://example.test/overlap-{index}.git"),
                credential_material: None,
                credential: None,
                mount_path: mount_path.into(),
                initial_branch: Some("main".into()),
                initial_commit: None,
            },
        };
    let mut overlaps = profiled_session_command(overlapping_session, None);
    overlaps.repositories = vec![
        repository_input(0, "overlap-parent", "/workspace/parent"),
        repository_input(1, "overlap-child", "/workspace/parent/child"),
    ];
    assert!(
        repository_application
            .create_profiled_session(overlaps)
            .await
            .is_err(),
        "D5 Repository trees overlap"
    );
    for index in 0..2 {
        assert_eq!(
            registry
                .find_repository(
                    "workspace",
                    &format!("profiled:{overlapping_session}:repository:{index}"),
                )
                .await
                .expect("D5 inventory"),
            None,
            "D5 no Registry effect"
        );
    }
    assert_eq!(
        repository_repo
            .get(&SessionId::from(overlapping_session))
            .await,
        Err(awaken_session_state_contract::SessionRepositoryError::NotFound),
        "D5 no root"
    );
    assert_eq!(
        repository_runtime.baseline_installs.load(Ordering::SeqCst),
        2,
        "D5 no additional phase projection install"
    );
    assert_eq!(
        repository_runtime.preparations.load(Ordering::SeqCst),
        1,
        "D5 no additional physical preparation"
    );
}

#[tokio::test]
async fn profiled_session_creation_enforces_publication_and_upfront_inputs() {
    // Profiled-Session FMECA and cause/effect graph. Failure modes are FM1 a
    // requested model bypasses the published Agent, FM2 an unavailable Agent is
    // admitted. Causes: C1 profile exists, C2 Agent available, C3 requested model
    // absent/equal, C4 requested model differs, C5 complete local inputs are
    // supplied up front, C6 Agent and Session MCP candidates are distinct or
    // overlap by name, C7 equal-origin names conflict, C8 direct resources are
    // none/valid/collide with an Agent final path/share authored spelling with
    // a Repository but project disjointly, and C9 final Repository trees
    // overlap. Effects: E1 freeze the published execution identity and local
    // inputs, E2 reject without a row, E3 retain Agent-only and Session-only MCP
    // while Session overrides Agent by logical name, E4 admit the final-path
    // disjoint pair through two phase projection installs and one physical
    // preparation, and E5 reject final Repository overlap before effects.
    // Graph: C1&&C2&&C3&&C5&&C6 -> E1+E3; C4||!C2||C7 -> E2;
    // C8(projected disjoint) -> E4; C9 -> E5.
    // Private imports preserve the create-request canonical pairing function,
    // arguments, errors and validation order; this table's MCP rows retain
    // candidate precedence. Toolpair coverage stays with the existing Managed cases
    // `session_mcp_overrides_validate_the_effective_pair_not_field_presence`
    // (four refusals/no staging) and the inherited-pair positive case
    // `session_inherits_published_agent_integrations_and_echoes_the_effective_set`.
    // Unchanged inputs/behavior reuse those cases and normal compile/lint.
    //
    // | Rule | Profile | Available | Requested model | Product MCP | Effect |
    // |---|---|---|---|---|---|
    // | P1 | yes | yes | absent/equal | distinct + Agent overlap | E1 + E3 |
    // | P2 | yes | yes | different | any | E2 |
    // | P3 | yes | no | any | any | E2 |
    // | P4 | yes | yes | absent/equal | duplicate Session name | E2 |
    // | D1 | yes | yes | absent/equal | no direct resource | existing P1-P4 semantics |
    // | D2 | yes | yes | absent/equal | valid direct File | original rev1 root contains File |
    // | D3 | yes | yes | absent/equal | direct collides Agent | E2 before root/effect |
    // | D4 | yes | yes | absent/equal | File/Repository spelling projects disjointly | E4: admit; 2 installs/1 prepare |
    // | D5 | yes | yes | absent/equal | Repository final trees overlap | E5 before registry/root/effect |
    //
    // Each cause/effect partition owns a separate boxed future. This keeps the
    // test's large, deeply composed Session values out of one aggregate async
    // frame without changing production stack limits or the decision oracles.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .expect("profiled Session repository"),
    );
    Box::pin(assert_profiled_realization_and_environment_rules(
        repository.clone(),
    ))
    .await;
    Box::pin(assert_profiled_publication_revision_rules(
        repository.clone(),
    ))
    .await;
    Box::pin(assert_profiled_override_projection(repository.clone())).await;
    Box::pin(assert_profiled_rejection_rules(repository)).await;
    Box::pin(assert_profiled_direct_resource_rules()).await;
}

#[tokio::test]
async fn server_times_do_not_change_default_create_identity_or_retry_receipts() {
    // Cause/effect table: T1 default identity + absent root -> clock persisted;
    // T2 identical command + later clock -> exact root, no repeated effects;
    // T3 genuine mutation -> update only; T4 response loss + same receipt ->
    // first mutation time; T5 clock rollback -> updated time never regresses.
    // T6 captures the real atomic insertion and omits server-owned fields that
    // did not exist in the old format (materialization and W25 presentation/
    // history). No fresh empty history may impersonate old bytes. Install its
    // original default receipt through
    // the real Store, then requires new-binary replay without history/effects.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(RecordingCreateRuntime::default());
    let captured = Arc::new(FaultingSessionRepository::new(repository.clone()));
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_800_000_000_123));
    let application = SessionApplicationBuilder::fixture(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(captured.clone(), clock.clone()),
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::RegisteredWorker,
        ..Default::default()
    })
    .build()
    .expect("valid fixture Session application");
    let make = || {
        let mut command = creation_command("default-clock-identity");
        command.intent.control.runtime_placement =
            awaken_session_types::SessionRuntimePlacement::Worker;
        command.idempotency = None;
        command
    };
    let created = application.create_session(make()).await.unwrap();
    assert!(
        created.environment_materialization().unwrap().is_empty(),
        "T1 fresh fact atomic with root"
    );
    assert_eq!(created.created_at_unix_ms, Some(1_800_000_000_123), "T1");
    clock.advance(10_000);
    assert_eq!(
        application.create_session(make()).await.unwrap(),
        created,
        "T2"
    );
    assert_eq!(runtime.preparations.load(Ordering::SeqCst), 1, "T2");
    let inserted = captured.applied_create_roots().into_iter().next().unwrap();
    let mut old_wire = serde_json::to_value(&inserted).unwrap();
    for field in [
        "environment_materialization",
        "initial_agent_configuration",
        "update_history",
    ] {
        old_wire.as_object_mut().unwrap().remove(field);
    }
    let mut old: PersistedSession = serde_json::from_value(old_wire).unwrap();
    assert!(
        old.environment_materialization().is_none(),
        "T6 old JSON stays unknown"
    );
    assert!(awaken_session_state_contract::SessionUpdateHistory::for_session(&old).is_none());
    assert!(
        awaken_session_state_contract::SessionUpdateHistory::agent_configuration(&old).is_none()
    );
    old.revision = awaken_session_types::SessionRevision(0);
    let mut old_request = old.clone();
    old_request.created_at_unix_ms = None;
    old_request.updated_at_unix_ms = None;
    let old_hash =
        awaken_session_state_contract::SessionMutationPayload::Replace(old_request).stable_hash();
    let old_receipt = awaken_session_state_contract::IdempotencyRecord {
        key: format!("session:create:{}:{old_hash}", old.session_id),
        payload_hash: old_hash,
    };
    let historical: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    historical
        .create("workspace", old, old_receipt, Vec::new())
        .await
        .unwrap();
    let old_readback = historical.get(&created.session_id).await.unwrap();
    let historical_runtime = Arc::new(RecordingCreateRuntime::default());
    let new_reader = SessionApplicationBuilder::fixture(
        historical_runtime.clone(),
        Arc::new(NoopMcpRealizer),
        crate::SessionPersistence::new(historical.clone(), clock.clone()),
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .configuration(SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::RegisteredWorker,
        ..Default::default()
    })
    .build()
    .expect("valid fixture Session application");
    assert_eq!(
        new_reader.create_session(make()).await.unwrap(),
        old_readback,
        "T6 exact old receipt replay"
    );
    assert!(
        historical
            .get(&created.session_id)
            .await
            .unwrap()
            .environment_materialization()
            .is_none()
    );
    assert_eq!(
        historical_runtime.preparations.load(Ordering::SeqCst),
        0,
        "T6 zero repeated effects"
    );
    let owner = application.owner(&created.session_id).await.unwrap();
    let mut changed = created.clone();
    changed.title = Some("time mutation".into());
    let receipt = awaken_session_state_contract::IdempotencyRecord {
        key: "time-mutation".into(),
        payload_hash: "time-mutation".into(),
    };
    let commit = application
        .commit_session_snapshot_with_record(
            &owner,
            // T3 is the real authoring CAS; T4 retains the original raw
            // candidate so receipt replay still precedes current-root checks.
            awaken_store_conformance::session_store::record_update(&created, changed.clone(), 0),
            receipt.clone(),
            vec![],
        )
        .await
        .unwrap();
    assert!(commit.transitioned(), "T3");
    let updated = commit.into_session();
    assert_eq!(updated.created_at_unix_ms, created.created_at_unix_ms);
    assert_eq!(updated.updated_at_unix_ms, Some(1_800_000_010_123));
    clock.advance(10_000);
    let commit = application
        .commit_session_snapshot_with_record(
            &owner,
            awaken_session_state_contract::SessionAggregate::try_from(changed)
                .expect("valid test snapshot"),
            receipt,
            vec![],
        )
        .await
        .unwrap();
    assert!(!commit.transitioned(), "T4");
    assert_eq!(commit.into_session(), updated);
    clock.set(1);
    let rollback = application
        .commit_session_snapshot_with_outcome(
            &owner,
            awaken_session_state_contract::SessionAggregate::try_from(updated.clone())
                .expect("valid test snapshot"),
            "clock-rollback",
            vec![],
        )
        .await
        .unwrap()
        .into_session();
    assert_eq!(
        rollback.updated_at_unix_ms, updated.updated_at_unix_ms,
        "T5"
    );
}

mod execution_context;
mod time_assurance;

/// The Registry of an ephemeral Resources application, on the host clock: the
/// one these Session tests register Repositories and Memory stores against.
pub(super) async fn ephemeral_resource_registry()
-> Arc<dyn awaken_resource_contract::ResourceRegistry> {
    awaken_resource_persistence::ephemeral(
        awaken_time::system_clock(),
        Arc::new(RecordingMemoryInvalidator::default()),
    )
    .await
    .expect("Resource authorities")
    .authorities()
    .resource_registry()
}

#[derive(Default)]
pub(super) struct RecordingMemoryInvalidator(std::sync::Mutex<Vec<(String, String)>>);
#[cfg(test)]
impl awaken_resource_contract::MemoryInvalidator for RecordingMemoryInvalidator {
    fn publish(&self, store: &str, path: &str) {
        self.0
            .lock()
            .expect("test Memory invalidations")
            .push((store.into(), path.into()));
    }
}
