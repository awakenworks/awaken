use super::*;
mod cleanup_timing;
use awaken_session_realization_contract::{
    SessionRealizationControl, SessionResourceMaterializationControl, SessionTerminalCleanupQueue,
    SessionTerminalDisposalControl, SessionTerminalMemoryControl,
    SessionTerminalPreparationControl, SessionTerminalRepositoryPublicationControl,
};
use awaken_session_repository_contract::LifecycleOutbox as _;
use awaken_session_types::SessionId;

#[test]
fn lifecycle_cancellation_interrupts_a_held_recovery_scan() {
    run_composed_async_test(|| async {
        // Timing diagnostics add no await, timer, or recovery decision. Causes:
        // cancellation at initial dispatch / candidate scan / final validation
        // => prompt shutdown, no later phase or fabricated successful cycle.
        // These three boundary rows retain the behavioral gate; wall-clock log
        // values are diagnostic data and deliberately have no duration assertion.
        // Cover initial dispatch, recovery candidates and the final audit.
        // No held read may prevent process shutdown; cancellation does not release any
        // physical predecessor or commit a synthetic successful recovery.
        for held_call in [1, 2, 3] {
            let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap(),
            );
            let observed = Arc::new(FaultingSessionRepository::new(durable));
            observed
                .hold_recovery_scan_on_call
                .store(held_call, Ordering::SeqCst);
            let application = Arc::new(application(
                observed.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            ));
            let cancellation = awaken_runtime_contract::CancellationToken::new();
            let task = tokio::spawn(
                application
                    .clone()
                    .run_lifecycle_supervisor(cancellation.clone()),
            );
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                observed.recovery_scan_entered.notified(),
            )
            .await
            .expect("recovery reached the held read");
            cancellation.cancel();
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .expect("cancellation must not wait for the held read")
                .expect("supervisor did not panic")
                .expect("cancelled cleanly");
            assert_eq!(observed.recovery_scan_count(), held_call);
            assert!(
                application
                    .run_lifecycle_supervisor(cancellation)
                    .await
                    .unwrap_err()
                    .contains("already claimed")
            );
        }
    });
}

#[tokio::test(start_paused = true)]
async fn lifecycle_maintenance_is_not_starved_by_a_held_recovery_scan() {
    // Lifecycle-supervisor cause/effect decision table. C1 the initial dispatch
    // scan completes; C2 the following full recovery scan remains blocked past
    // the 30-second maintenance cadence; C3 cancellation arrives while that
    // recovery is still blocked. E1 lease renewal and Work dispatch still enter
    // their one existing maintenance path at the cadence; E2 no second recovery
    // cycle starts concurrently; E3 cancellation drops the owned recovery task
    // and exits. Rules LM1=C1+C2=>E1+E2; LM2=C3=>E3. The repository scan count
    // is the observable dispatch boundary: call 1 is startup dispatch, call 2
    // is the held recovery, and call 3 can only be cadence maintenance.
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let observed = Arc::new(FaultingSessionRepository::new(durable));
    observed
        .hold_recovery_scan_on_call
        .store(2, Ordering::SeqCst);
    let application = Arc::new(application(
        observed.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let cancellation = awaken_runtime_contract::CancellationToken::new();
    let task = tokio::spawn(application.run_lifecycle_supervisor(cancellation.clone()));

    observed.recovery_scan_entered.notified().await;
    assert_eq!(observed.recovery_scan_count(), 2, "LM1 precondition");
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    for _ in 0..8 {
        if observed.recovery_scan_count() >= 3 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        observed.recovery_scan_count(),
        3,
        "LM1/E1 cadence maintenance must not wait for full recovery"
    );

    cancellation.cancel();
    task.await
        .expect("supervisor did not panic")
        .expect("LM2/E3 cancelled cleanly");
}

#[test]
fn terminal_cleanup_retains_exact_process_owner_requirements_through_dispose() {
    // TC1 trust x supervision x {Prepare,Dispose} is exhaustive. Every phase
    // retains the exact physical-owner requirement frozen by Environment.
    // TC2 model/ACP/credential requirements remain empty; process cleanup is
    // independent of an available inference login. No cleanup effect is run
    // here: the production requirement compiler is the subject under test.
    use awaken_provisioning_contract::{
        HostAccessPolicy, SandboxDisposalPreparation, SandboxEffectFence,
    };
    use awaken_session_state_contract::{
        SessionCleanupDisposalCommand, SessionTerminalCleanupAction,
    };
    let fence = SandboxEffectFence::new("terminal", "worker", "boot", 1, u64::MAX).unwrap();
    let disposal = SessionTerminalCleanupAction::Dispose {
        command: SessionCleanupDisposalCommand {
            session_id: "supervised-terminal".into(),
            effect_id: "terminal".into(),
            preparation_fingerprint: "fixture-preparation".into(),
            provider_disposal: SandboxDisposalPreparation::new(fence, "fixture-provider").unwrap(),
            restore_target: None,
        },
    };
    for trusted in [false, true] {
        for supervised in [false, true] {
            let mut session = persisted("supervised-terminal", true, "terminated");
            let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
                &mut session.baseline
            else {
                panic!("frozen fixture");
            };
            baseline.environment.sandbox.host_access = if trusted {
                HostAccessPolicy::TrustedHost
            } else {
                HostAccessPolicy::Isolated
            };
            baseline.environment.sandbox.process_tree_supervision = supervised;
            for action in [
                &SessionTerminalCleanupAction::Prepare { commands: vec![] },
                &disposal,
            ] {
                let requirements = crate::realization::terminal_cleanup_worker_requirements(
                    "workspace",
                    &session,
                    action,
                )
                .unwrap();
                assert_eq!(requirements.sandbox.process_tree_supervision, supervised);
                assert_eq!(
                    requirements.sandbox_backend.as_deref(),
                    trusted.then_some(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND)
                );
                assert!(requirements.required_credentials.is_empty());
                assert!(requirements.required_acp_capabilities.is_empty());
                assert!(requirements.required_sandbox_tool_recovery.is_empty());
            }
        }
    }
}

fn terminal_prepare_commands(
    work: awaken_session_realization_contract::SessionTerminalCleanupWork,
) -> Vec<awaken_session_state_contract::SessionCleanupCommand> {
    match work.action {
        awaken_session_state_contract::SessionTerminalCleanupAction::Prepare { commands } => {
            commands
        }
        action => panic!("expected terminal preparation action, got {action:?}"),
    }
}

#[tokio::test]
async fn terminal_assignment_preserves_the_complete_retained_resource_transition() {
    // Cause/effect table: C1 legacy empty/File or committed File/Skill/empty;
    // C2 pending B absent/unattempted/attempted; C3 two failed replacements;
    // C4 close/reopen SQLite; C5 current/expired realization authority.
    // Original six rows retain legacy A@0/1 with optional B@1/2. Nine history
    // rows commit A@1, roll back generations 2 and 3, then optionally prepare
    // B@4. Empty/Skill A has no input activation record to recover its revision.
    //
    // | History | Pending | Previous | Desired | Watermark |
    // | legacy empty/File | absent | A@0/1 | A@0/1 | 0/1 |
    // | legacy empty/File | either | A@0/1 | B@1/2 | 1/2 |
    // | commit + rollback twice | absent | A@1 | A@1 | 3 |
    // | commit + rollback twice | either | A@1 | B@4 | 4 |
    // Histories are constructed with the real aggregate commands while Idle,
    // then the real terminal transition runs before one durable write/reopen.
    // This does not claim each earlier rollback was separately SQL-committed.
    // Every row crosses C4/C5: a live assignment retains both exact manifests;
    // expiry rejects; neither changes the complete persisted root. Expected
    // pairs come from these historical commands, never the production
    // active_generation/desired_generation getters also used by projection.
    // Sending A -> A while B is pending regresses an already-staged Worker;
    // pairing A with attempted watermark 3 invents an installed generation.
    let empty = awaken_session_resource_contract::ResolvedSessionResources::default();
    for (history, active, rollback_twice, active_revision) in [
        ("legacy-empty", empty.clone(), false, 0),
        ("legacy-file", file_resources("active"), false, 1),
        ("committed-file", file_resources("active"), true, 1),
        ("committed-skill", skill_resources("active"), true, 1),
        ("committed-empty", empty, true, 1),
    ] {
        for pending in [None, Some(false), Some(true)] {
            let directory = tempfile::tempdir().expect("temporary terminal Session authority");
            let path = directory.path().join("terminal-transition.db");
            let path = path.to_string_lossy().to_string();
            let session_id = "terminal-transition";
            let desired = file_resources("pending");
            let mut session = persisted(session_id, true, "idle");
            session.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
                owner: "worker".into(),
                runtime_incarnation: "incarnation".into(),
                epoch: 1,
                expires_at_unix_ms: awaken_agent_contract::clock::Clock::now_ms(
                    &awaken_agent_contract::clock::SystemClock,
                ) + 60_000,
            });
            if rollback_twice {
                assert_eq!(
                    session
                        .resources
                        .prepare(
                            &awaken_session_types::SessionId::from(session_id),
                            active.clone()
                        )
                        .unwrap(),
                    1
                );
                session.resources.start_attempt().unwrap();
                session.resources.commit().unwrap();
                for revision in [2, 3] {
                    assert_eq!(
                        session
                            .resources
                            .prepare(
                                &awaken_session_types::SessionId::from(session_id),
                                file_resources("failed")
                            )
                            .unwrap(),
                        revision,
                    );
                    session.resources.start_attempt().unwrap();
                    session.resources.rollback("replacement failed").unwrap();
                }
            } else {
                session.resources =
                    awaken_session_state_contract::SessionResourceState::from_active(
                        active.clone(),
                    );
                session
                    .resources
                    .adopt_legacy_active(&awaken_session_types::SessionId::from(session_id));
            }
            let watermark = if rollback_twice { 3 } else { active_revision };
            let desired_revision = if let Some(attempted) = pending {
                assert_eq!(
                    session
                        .resources
                        .prepare(
                            &awaken_session_types::SessionId::from(session_id),
                            desired.clone()
                        )
                        .unwrap(),
                    watermark + 1,
                );
                if attempted {
                    session.resources.start_attempt().unwrap();
                }
                watermark + 1
            } else {
                active_revision
            };
            assert_eq!(session.resources.committed_revision, Some(active_revision));
            let expected_previous =
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace",
                    active_revision,
                    active.clone(),
                );
            let expected_desired =
                awaken_session_resource_contract::SessionResourceManifest::at_revision(
                    "workspace",
                    desired_revision,
                    if pending.is_some() {
                        desired
                    } else {
                        active.clone()
                    },
                );
            let resources_before_termination = session.resources.clone();
            session
                .transition_execution(
                    awaken_session_state_contract::SessionExecutionState::Terminated,
                )
                .expect("terminal transition after Resource history");
            assert_eq!(
                session.resources, resources_before_termination,
                "{history}/{pending:?} termination retains the entire Resource state"
            );
            let before_restart = {
                let repo = awaken_session_store::SqliteManagedSessionRepository::open(&path)
                    .expect("open terminal Session authority");
                create(&repo, session).await;
                repo.get(&SessionId::from(session_id)).await.unwrap()
            };
            let repo = Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(&path)
                    .expect("reopen terminal Session authority"),
            );
            let snapshot = repo.get(&SessionId::from(session_id)).await.unwrap();
            assert_eq!(snapshot, before_restart, "{history}/{pending:?} cold root");
            let application = application(
                repo.clone(),
                Arc::new(RecordingEnvironmentSource::default()),
            );
            let assignment = application
                .terminal_cleanup_assignment_from_snapshot("workspace", &snapshot)
                .await
                .unwrap();
            let transition = assignment
                .projection
                .resource_transition(
                    awaken_session_realization_contract::FrozenResourceTransitionUse::ValidateInstalled,
                )
                .unwrap();
            assert_eq!(
                transition.previous(),
                &expected_previous,
                "{history}/{pending:?} previous"
            );
            assert_eq!(
                transition.desired(),
                &expected_desired,
                "{history}/{pending:?} desired"
            );
            assert_eq!(
                repo.get(&SessionId::from(session_id)).await.unwrap(),
                snapshot,
                "{history}/{pending:?} projection must not acknowledge cleanup"
            );
            let mut expired = snapshot.clone();
            expired.realization.as_mut().unwrap().expires_at_unix_ms = 1;
            assert!(matches!(
                application
                    .terminal_cleanup_assignment_from_snapshot("workspace", &expired)
                    .await,
                Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
            ));
            assert_eq!(
                repo.get(&SessionId::from(session_id)).await.unwrap(),
                snapshot,
                "{history}/{pending:?} rejection must not mutate the complete root"
            );
        }
    }
}

/// Recovery-scan causal graph: C1 one supervisor cycle owns Resource,
/// Environment, realization, Event, and Outcome convergence; C2 every handler
/// previously opened the same durable candidate index; C3 cutover validation is
/// a distinct post-cycle audit. Effects are E1 one candidate scan feeds all
/// handlers, E2 handlers reload roots by id before effects, and E3 the final
/// audit remains an independent second scan.
///
/// | Rule | Candidate scan | Cutover audit | Expected scans |
/// |---|---|---|---|
/// | S1 | succeeds | enabled | 2 (E1 + E3) |
/// | S2 | unavailable | not reached | 1 and retryable failure |
#[test]
fn one_recovery_cycle_scans_candidates_once_before_the_final_audit() {
    run_composed_async_test(|| async {
        let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let observed = Arc::new(FaultingSessionRepository::new(durable));
        let application = Arc::new(application(
            observed.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        ));

        observed.fail_recovery_scan_once();
        let unavailable = application.clone().reconcile_pending_session_state().await;
        assert_eq!(unavailable.retryable_failures, 1, "S2");
        assert_eq!(observed.recovery_scan_count(), 1, "S2");

        let cycle = application.reconcile_pending_session_state().await;
        assert_eq!(cycle.retryable_failures, 0, "S1");
        assert_eq!(observed.recovery_scan_count(), 3, "S1/E1/E3");
    });
}

#[derive(Default)]
pub(super) struct RecordingResourceRuntime {
    applied: Mutex<
        Vec<(
            u64,
            awaken_session_resource_contract::ResolvedSessionResources,
        )>,
    >,
    repository: Option<Arc<dyn awaken_session_repository_contract::SessionStore>>,
    roots_observed_before_apply: Mutex<Vec<PersistedSession>>,
    pub(super) replaced_tools: Mutex<
        Vec<(
            String,
            awaken_session_configuration_contract::SessionToolConfiguration,
        )>,
    >,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingResourceRuntime {
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        NoopRuntime.run(agent, thread, content).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        NoopRuntime.resume(thread, tool_use_id, decision).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        NoopRuntime
            .resume_custom(thread, tool_use_id, content, is_error)
            .await
    }

    fn model(&self) -> String {
        "resource-generation-test".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingResourceRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingResourceRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingResourceRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError> {
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
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for RecordingResourceRuntime {
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        self.replaced_tools
            .lock()
            .unwrap()
            .push((thread.to_owned(), tools));
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Adopt a previously persisted environment before reopening a Session.
    /// Implementations must validate that the binding belongs to `thread` and
    /// fail closed when it is malformed, unavailable, or owned elsewhere.
    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .adopt_session_environment(agent, thread, binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_mcp_contract::McpGenerationRef],
    ) -> Result<
        awaken_session_state_contract::QuiescenceReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .quiesce_session_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
                expected_mcp_generations,
            )
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_state_contract::CheckpointReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_state_contract::SourceReleasePreparedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_state_contract::SourceDisposedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Restore a distinct environment from the exact durable checkpoint.
    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<
        awaken_session_state_contract::RestoreReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<
        Vec<awaken_session_resource_contract::ResolvedSkillBinding>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resolve_session_skills(workspace_id, skills)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
    ) -> Result<(), RunError> {
        if let Some(repository) = &self.repository {
            let root = repository
                .get(&SessionId::from(thread))
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            self.roots_observed_before_apply.lock().unwrap().push(root);
        }
        self.applied.lock().unwrap().push((
            transition.desired().revision,
            transition.desired().resources.clone(),
        ));
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for RecordingResourceRuntime {
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
    ) -> Result<u64, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<awaken_session_execution_contract::OutcomeDrive, RunError> {
        NoopRuntime
            .define_outcome(thread, description, rubric, max_iterations)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

/// Skills-only frozen-projection cause/effect graph. C1=active mounted inputs;
/// C2=one active exact Skill pin; C3=pending replacement. E1 `from_active`
/// preserves a nonzero installed generation; E2 the Session application's
/// canonical projection carries the same revision and manifest together.
/// Constraints: empty inputs do not mean an empty Resource manifest, and the
/// projection must not consult the mutable Skill catalog.
/// Decision rule S1: C1=no, C2=yes, C3=no => (revision 1, exact Skill pin).
#[tokio::test]
async fn skills_only_active_generation_is_preserved_in_the_frozen_projection() {
    // Decision rule: execute S1 and require the exact skills-only revision and
    // manifest pair in the frozen projection.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let mut session = persisted("skills-only-projection", false, "idle");
    let resources = skill_resources("design");
    session.resources =
        awaken_session_state_contract::SessionResourceState::from_active(resources.clone());

    let projection = application
        .frozen_session_projection("workspace".into(), &session, false)
        .await
        .expect("S1 canonical frozen projection");

    assert_eq!(projection.resource_revision, 1, "S1/E1");
    assert_eq!(projection.resources, resources, "S1/E2");
}

/// Rolled-back projection cause/effect graph: C1 generation 1 is active; C2 a
/// generation-2 replacement fails and rolls back; C3 the next Run rebuilds its
/// frozen projection and reconciles the resident Runtime. Effects: E1 durable
/// revision 2 remains only a monotonic watermark; E2 both rebuild paths carry
/// the exact active pair (revision 1, manifest 1); E3 no process-local pseudo
/// generation 2 is published for the strict claimed-Worker fence to reject;
/// C4 no transcript prefix was admitted, so E4 the frozen request context is
/// empty without consulting committed Thread history.
///
/// | Rule | C1 | C2 | C3 | C4 | Frozen pair | Runtime pair | Context |
/// |---|---|---|---|---|---|---|---|
/// | G1 | yes | rollback | yes | absent | (1, manifest 1) | (1, manifest 1) | empty |
#[tokio::test]
async fn rolled_back_resource_generation_recovers_only_the_installed_pair() {
    // Constraint/Invariant: desired-generation watermark never substitutes for
    // the active installed revision/manifest pair. Decision rule: execute G1 and
    // require both frozen and Runtime projections to recover pair (1, manifest 1).
    let runtime = Arc::new(RecordingResourceRuntime::default());
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let app = application_with_runtime(
        runtime.clone(),
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let installed = file_resources("installed");
    let attempted = file_resources("rejected");
    let mut session = persisted("resource-rollback-recovery", false, "idle");
    session
        .resources
        .prepare(&session.session_id, installed.clone())
        .unwrap();
    session.resources.commit().unwrap();
    session
        .resources
        .prepare(&session.session_id, attempted)
        .unwrap();
    session.resources.start_attempt().unwrap();
    session
        .resources
        .rollback("Workdir rejected read-only File")
        .unwrap();
    assert_eq!(session.resources.revision, 2, "G1/E1");

    let projection = app
        .frozen_session_projection("workspace".into(), &session, true)
        .await
        .expect("G1 frozen active projection");
    assert_eq!(projection.resource_revision, 1, "G1/E2");
    assert_eq!(projection.resources, installed, "G1/E2");
    assert!(projection.request_context.is_empty(), "G1/C4/E4");

    app.reconcile_persisted_resources("workspace", session)
        .await
        .expect("G1 active Runtime reconciliation");
    assert_eq!(
        runtime.applied.lock().unwrap().as_slice(),
        [(1, installed)].as_slice(),
        "G1/E2/E3"
    );
}

#[tokio::test]
async fn running_session_persists_manifest_and_applies_only_at_idle_boundary() {
    // Cause/effect graph: C1 Session Running/Idle; C2 valid complete desired
    // manifest; C3 no prior pending attempt; C4 Runtime external application
    // observes the repository root. Effects: E1 Running commits pending with
    // attempts=0, preserves active, and performs no Runtime IO; E2 the later
    // Idle reconciler first commits attempts=1; E3 the Runtime observes that
    // exact durable pending generation before applying it once; E4 settlement
    // promotes it to active. FMECA: mutating mounts during a Run can invalidate
    // files observed by that execution (S9/O5/D6); applying before the attempt
    // CAS would leave an untracked external effect after a crash.
    //
    // | Rule | execution | pending | attempt | Runtime IO | Effect |
    // |---|---|---|---|---|---|
    // | M1 | Running | absent | none | none | E1 |
    // | M2 | Idle | present | committed | once | E2 + E3 + E4 |
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    create(
        repo.as_ref(),
        persisted("running-manifest", false, "running"),
    )
    .await;
    let runtime = Arc::new(RecordingResourceRuntime {
        repository: Some(repo.clone()),
        ..Default::default()
    });
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .with_clock(Arc::new(awaken_agent_contract::clock::ManualClock::new(
        100,
    )));
    let desired = file_resources("safe-boundary");
    let outcome = app
        .replace_session_resource_manifest(
            &SessionId::from("running-manifest"),
            ReplaceSessionResourceManifest {
                resources: desired.clone(),
                expected_resource_revision: None,
                idempotency_key: Some("manifest-safe-boundary".into()),
                request_fingerprint: "manifest-safe-boundary-hash".into(),
            },
        )
        .await
        .expect("M1 intent accepted");
    // M3 new binding intent records its time before deferred physical effects.
    let mut desired = desired;
    desired.observe_input_times(&Default::default(), 100);
    assert!(
        outcome.session.resources.active.inputs().is_empty(),
        "M1/E1"
    );
    assert_eq!(
        outcome.session.resources.pending.as_ref(),
        Some(&desired),
        "M1/E1"
    );
    assert!(
        outcome
            .session
            .resources
            .activations
            .iter()
            .all(|activation| activation.attempts == 0),
        "M1/E1"
    );
    assert!(runtime.applied.lock().unwrap().is_empty(), "M1/E1");
    assert!(
        runtime
            .roots_observed_before_apply
            .lock()
            .unwrap()
            .is_empty(),
        "M1/E1"
    );

    let mut idle = outcome.session;
    idle.execution = SessionExecutionState::Idle;
    let _fixture = app
        .commit_session_snapshot_with_outcome("workspace", idle, "test-safe-boundary", Vec::new())
        .await
        .unwrap();
    let reconciliation = app.reconcile_resource_activations().await;
    assert!(reconciliation.failures.is_empty(), "M2/E2");
    let committed = repo
        .get(&SessionId::from("running-manifest"))
        .await
        .unwrap();
    assert!(committed.resources.pending.is_none(), "M2/E2");
    assert_eq!(committed.resources.active, desired, "M2/E4");
    let observed = runtime.roots_observed_before_apply.lock().unwrap();
    assert_eq!(observed.len(), 1, "M2/E3");
    assert_eq!(
        observed[0].resources.pending.as_ref(),
        Some(&desired),
        "M2/E3"
    );
    assert_eq!(
        observed[0]
            .resources
            .activations
            .iter()
            .filter(|activation| {
                activation.state == awaken_session_state_contract::ActivationState::Prepared
            })
            .map(|activation| activation.attempts)
            .collect::<Vec<_>>(),
        [1],
        "M2/E2/E3"
    );
    assert_eq!(
        runtime.applied.lock().unwrap().as_slice(),
        [(1, desired)].as_slice(),
        "M2/E3"
    );
}

/// Resource-staging authorization cause/effect graph: C1 the Session
/// root is nonterminal; C2 the current live root lease authorizes the asserted
/// owner/incarnation/epoch and monotonic expiry; C3 revision, target kind, and full resolved input/Skill equal
/// the authoritative generation; C4 a replacement is pending. Effects: E1
/// return the root-owned Workspace; E2 stale ownership; E3 invalid frozen
/// input; E4 terminal denial. Active and pending are independently exact
/// retained pairs (their crossover table lives in resource_resume). No Run claim or
/// parallel staging state is synthesized.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | R1 | yes | yes | yes | yes | E1 pending target |
/// | R2 | yes | no | any | any | E2 |
/// | R3 | yes | yes | no | any | E3 |
/// | R4 | no | any | any | any | E4 |
/// | R5 | yes | yes | yes | no | E1 active rebuild target |
/// Renewal regression: R6 same-generation shorter assertion (whether its old
/// timestamp is live or expired) + live renewed root => E1 for every target;
/// R7 missing/expired current root, foreign owner/incarnation/epoch, or an
/// assertion beyond the root expiry => E2. A heartbeat may extend expiry while
/// File/Skill/Memory preparation is in flight; that is not ownership loss.
#[tokio::test]
async fn resource_authority_is_the_exact_pending_session_generation() {
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-resource-memory".into(),
        runtime_incarnation: "worker-resource-memory:1:boot".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX - 1,
    };
    let input = awaken_session_resource_contract::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("pending-memory"),
        source: awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-pending"),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from("memory-pending"),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/pending".into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    };
    let file = file_resources("file-pending").inputs()[0].clone();
    let skill = skill_resources("skill-pending").skills()[0].clone();
    let desired = awaken_session_resource_contract::ResolvedSessionResources::try_new(
        vec![input.clone(), file.clone()],
        vec![skill.clone()],
    )
    .expect("pending Memory, File, and Skill targets");
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("resource Memory authority repository"),
    );
    for (session_id, terminal, active) in [
        ("resource-memory-current", false, false),
        ("resource-memory-active", false, true),
        ("resource-memory-terminal", true, false),
        ("resource-memory-expired", false, false),
        ("resource-memory-unowned", false, false),
    ] {
        let mut session = persisted(
            session_id,
            true,
            if terminal { "terminated" } else { "idle" },
        );
        session.realization = Some(lease.clone());
        if session_id == "resource-memory-expired" {
            session.realization.as_mut().unwrap().expires_at_unix_ms = 1;
        } else if session_id == "resource-memory-unowned" {
            session.realization = None;
        }
        session
            .resources
            .prepare(
                &awaken_session_types::SessionId::from(session_id),
                desired.clone(),
            )
            .expect("prepare exact Memory generation");
        if active {
            session
                .resources
                .commit()
                .expect("commit active generation");
        }
        create(repo.as_ref(), session).await;
    }
    let application = application(repo, Arc::new(RecordingEnvironmentSource::default()));
    let exact = awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
        "resource-memory-current",
        1,
        input.clone(),
        lease.clone(),
    )
    .expect("exact pending Memory intent");
    assert_eq!(
        application
            .authorize_resource_materialization_intent(&exact)
            .await
            .expect("R1/E1 exact root authority"),
        "workspace",
        "R1/E1"
    );
    let exact_active =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-active",
            1,
            input.clone(),
            lease.clone(),
        )
        .expect("exact active rebuild intent");
    assert_eq!(
        application
            .authorize_resource_materialization_intent(&exact_active)
            .await
            .expect("R5/E1 exact active rebuild authority"),
        "workspace",
        "R5/E1"
    );
    let exact_file =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            1,
            file,
            lease.clone(),
        )
        .expect("exact pending File intent");
    let exact_skill =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
            "resource-memory-current",
            1,
            skill.clone(),
            lease.clone(),
        )
        .expect("exact pending Skill intent");
    for intent in [&exact_file, &exact_skill] {
        assert_eq!(
            application
                .authorize_resource_materialization_intent(intent)
                .await
                .expect("R1/E1 exact non-Memory authority"),
            "workspace",
            "R1/E1"
        );
    }

    for session_id in ["resource-memory-current", "resource-memory-active"] {
        for asserted_expiry in [1, u64::MAX - 2] {
            for target in [&exact, &exact_file, &exact_skill] {
                let mut encoded = serde_json::to_value(target).unwrap();
                encoded["session_id"] = session_id.into();
                encoded["lease"]["expires_at_unix_ms"] = asserted_expiry.into();
                let renewed_intent = serde_json::from_value(encoded).unwrap();
                assert_eq!(
                    application
                        .authorize_resource_materialization_intent(&renewed_intent)
                        .await
                        .expect("R6 same-generation renewal keeps the target authorized"),
                    "workspace",
                );
            }
        }
    }
    for (field, value) in [
        ("owner", serde_json::json!("another-worker")),
        ("runtime_incarnation", serde_json::json!("another-boot")),
        ("epoch", serde_json::json!(lease.epoch + 1)),
        ("expires_at_unix_ms", serde_json::json!(u64::MAX)),
    ] {
        let mut encoded = serde_json::to_value(&exact).unwrap();
        encoded["lease"][field] = value;
        let denied = serde_json::from_value(encoded).unwrap();
        assert!(
            matches!(
                application
                    .authorize_resource_materialization_intent(&denied)
                    .await,
                Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
            ),
            "R7 rejects forged {field}"
        );
    }
    for session_id in ["resource-memory-expired", "resource-memory-unowned"] {
        let mut encoded = serde_json::to_value(&exact).unwrap();
        encoded["session_id"] = session_id.into();
        encoded["lease"]["expires_at_unix_ms"] = 1.into();
        let denied = serde_json::from_value(encoded).unwrap();
        assert!(
            matches!(
                application
                    .authorize_resource_materialization_intent(&denied)
                    .await,
                Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
            ),
            "R7 root must still have a live owner: {session_id}"
        );
    }

    let stale = awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
        "resource-memory-current",
        1,
        input.clone(),
        awaken_session_resource_contract::SessionRealizationLease {
            epoch: lease.epoch + 1,
            ..lease.clone()
        },
    )
    .expect("foreign lease intent");
    assert!(
        matches!(
            application
                .authorize_resource_materialization_intent(&stale)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "R2/E2"
    );

    let wrong_revision =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            2,
            input.clone(),
            lease.clone(),
        )
        .expect("wrong revision remains structurally valid");
    let mut wrong_input = input.clone();
    wrong_input.mount_path = "/memory/foreign".into();
    let wrong_input =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-current",
            1,
            wrong_input,
            lease.clone(),
        )
        .expect("foreign input remains structurally valid");
    let mut foreign_skill = skill;
    foreign_skill.version += 1;
    let wrong_skill =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new_skill(
            "resource-memory-current",
            1,
            foreign_skill,
            lease.clone(),
        )
        .expect("foreign Skill remains structurally valid");
    for intent in [&wrong_revision, &wrong_input, &wrong_skill] {
        assert!(
            matches!(
                application
                    .authorize_resource_materialization_intent(intent)
                    .await,
                Err(
                    awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(
                        _
                    )
                )
            ),
            "R3/E3"
        );
    }

    let terminal =
        awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
            "resource-memory-terminal",
            1,
            input,
            lease,
        )
        .expect("terminal-root intent");
    assert!(
        matches!(
            application
                .authorize_resource_materialization_intent(&terminal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Terminal)
        ),
        "R4/E4"
    );
}

fn profiled_policy_manifest(
    file: &str,
    skill_version: u64,
) -> awaken_session_resource_contract::ResolvedSessionResources {
    let (inputs, _) = file_resources(file).into_parts();
    let mut skill = skill_resources("review").skills()[0].clone();
    skill.version = skill_version;
    skill.bundle_sha256 = format!("sha-review-{skill_version}");
    awaken_session_resource_contract::ResolvedSessionResources::try_new(inputs, vec![skill])
        .unwrap()
}

async fn seed_profiled_resource_policy_sessions(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    for (id, policy) in [
        (
            "resource-managed",
            awaken_session_resource_contract::SessionMutationPolicy::Managed,
        ),
        (
            "resource-frozen",
            awaken_session_resource_contract::SessionMutationPolicy::Frozen,
        ),
        (
            "resource-files",
            awaken_session_resource_contract::SessionMutationPolicy::FileResources,
        ),
    ] {
        let mut session = persisted(id, false, "idle");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
        else {
            unreachable!("fixture baseline is frozen")
        };
        baseline.mutation_policy = policy;
        session.resources = awaken_session_state_contract::SessionResourceState::from_active(
            profiled_policy_manifest("initial", 1),
        );
        create(repo.as_ref(), session).await;
    }
}

async fn assert_managed_and_frozen_resource_policy_rules(
    app: Arc<SessionApplication>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let command = |resources| ReplaceSessionResourceManifest {
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&resources),
        resources,
        expected_resource_revision: None,
        idempotency_key: None,
    };

    app.replace_session_resource_manifest(
        &SessionId::from("resource-managed"),
        command(profiled_policy_manifest("next", 1)),
    )
    .await
    .expect("R1/E1");

    let frozen_before = repo.get(&SessionId::from("resource-frozen")).await.unwrap();
    assert!(
        matches!(
            app.replace_session_resource_manifest(
                &SessionId::from("resource-frozen"),
                command(profiled_policy_manifest("next", 1)),
            )
            .await,
            Err(SessionResourceManifestError::Rejected(_))
        ),
        "R2/E2"
    );
    assert_eq!(
        repo.get(&SessionId::from("resource-frozen")).await.unwrap(),
        frozen_before,
        "R2/E2"
    );
}

async fn assert_file_resource_and_credential_policy_rules(
    app: Arc<SessionApplication>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
) {
    let command = |resources| ReplaceSessionResourceManifest {
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&resources),
        resources,
        expected_resource_revision: None,
        idempotency_key: None,
    };
    app.replace_session_resource_manifest(
        &SessionId::from("resource-files"),
        command(profiled_policy_manifest("next", 1)),
    )
    .await
    .expect("R3/E3");
    let files_before = repo.get(&SessionId::from("resource-files")).await.unwrap();
    assert!(
        matches!(
            app.replace_session_resource_manifest(
                &SessionId::from("resource-files"),
                command(profiled_policy_manifest("next", 2)),
            )
            .await,
            Err(SessionResourceManifestError::Rejected(_))
        ),
        "R4/E4"
    );
    assert_eq!(
        repo.get(&SessionId::from("resource-files")).await.unwrap(),
        files_before,
        "R4/E4"
    );

    let rotation = app
        .rotate_repository_credential(
            &SessionId::from("resource-files"),
            "workspace",
            &awaken_resource_contract::BindingId::from("missing-repository"),
            CredentialMaterialInput::opaque(
                "test",
                awaken_agent_contract::RedactedString::new("never-consumed"),
            ),
        )
        .await
        .expect_err("R5/E5");
    assert!(
        rotation.to_string().contains("credentials are immutable"),
        "R5/E5 policy must precede missing Vault/binding checks: {rotation}"
    );
    assert_eq!(
        repo.get(&SessionId::from("resource-files")).await.unwrap(),
        files_before,
        "R5/E5"
    );
}

#[tokio::test]
async fn profiled_resource_mutation_policy_fences_the_root_before_external_effects() {
    // Cause/effect graph: C1 policy Managed/Frozen/FileResources; C2 complete
    // manifest changes a File; C3 it changes an exact Skill; C4 Repository-token
    // rotation is requested while no Vault ingress is configured. Effects: E1
    // Managed+C2 commits; E2 Frozen rejects with no root change; E3
    // FileResources+C2 commits; E4 FileResources+C3 rejects with no root change;
    // E5 profiled C4 rejects on immutable policy before consulting Vault.
    // Decision rules R1=Managed+C2=>E1, R2=Frozen+C2=>E2,
    // R3=FileResources+C2=>E3, R4=FileResources+C3=>E4,
    // R5=profiled+C4=>E5. Each independent cause partition owns a boxed
    // test-only future so the decision oracles do not share one oversized async
    // frame; production stack limits and state transitions remain unchanged.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("profiled policy repository"),
    );
    Box::pin(seed_profiled_resource_policy_sessions(repo.clone())).await;
    let app = Arc::new(application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    Box::pin(assert_managed_and_frozen_resource_policy_rules(
        app.clone(),
        repo.clone(),
    ))
    .await;
    Box::pin(assert_file_resource_and_credential_policy_rules(app, repo)).await;
}

#[tokio::test]
async fn retryable_initial_realization_is_fenced_and_budgeted_durably() {
    // Cause/effect graph: C1 failure retryable/permanent; C2 persisted attempts
    // below/at budget; C3 asserted lease exact/stale. Effects: E1 retryable below
    // budget keeps Preparing and expires the lease; E2 the next assignment
    // advances epoch and attempts; E3 retryable at budget or permanent enters
    // activation_failed; E4 stale delivery mutates nothing. Decision rules
    // covered: R1 retryable+attempt1<2+exact => E1; R2 next exact claim => E2;
    // R3 retryable+attempt2=2+exact => E3; stale ownership is covered by the
    // realization authority table. C4 executable refresh fails before claim;
    // E5 no lease/revision mutation, and successful retry enters R1. Constraint
    // K1 refresh owns no realization state. D1 C4=>E5; D2 !C4=>R1.
    // FMECA: an unbounded retry can strand create
    // forever (S8/O5/D6), while immediate terminal failure loses recoverability
    // (S8/O4/D4); persisted attempts plus lease expiry bound both modes.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("retry-budget", false, "preparing")).await;
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    app.set_executable_projection_refresh(paired_projection_refresh(refresh.clone()))
        .unwrap();
    let target =
        |incarnation: &str| awaken_session_realization_contract::SessionRealizationTarget {
            owner: "worker".into(),
            runtime_incarnation: incarnation.into(),
            lease_expires_at_unix_ms: u64::MAX,
            reassign_existing_lease: false,
        };

    let before_refresh_failure = repo.get(&SessionId::from("retry-budget")).await.unwrap();
    assert!(
        matches!(
            app.begin_session_realization(
                awaken_session_realization_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: "retry-budget".into(),
                    target: target("worker/refresh-failure"),
                }
            )
            .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "D1/E5"
    );
    assert_eq!(
        repo.get(&SessionId::from("retry-budget")).await.unwrap(),
        before_refresh_failure,
        "D1/E5 no claim mutation"
    );
    refresh.fail.store(false, Ordering::SeqCst);

    let first = app
        .begin_session_realization(
            awaken_session_realization_contract::BeginSessionRealization {
                expected_resources: None,
                session_id: "retry-budget".into(),
                target: target("worker/1"),
            },
        )
        .await
        .expect("R1 begin");
    app.fail_session_realization(
        awaken_session_realization_contract::FailSessionRealization {
            session_id: "retry-budget".into(),
            lease: first.authority.lease,
            prepared_resource_revision: None,
            retryable: true,
            source_run_id: None,
            reason: "worker unavailable".into(),
        },
    )
    .await
    .expect("R1 retryable failure");
    let after_first = repo.get(&SessionId::from("retry-budget")).await.unwrap();
    assert_eq!(
        after_first.execution,
        SessionExecutionState::Preparing,
        "R1/E1"
    );
    assert_eq!(after_first.realization_progress.attempts, 1, "R1/E1");
    assert_eq!(
        after_first.realization.as_ref().unwrap().expires_at_unix_ms,
        0,
        "R1/E1"
    );

    let second = app
        .begin_session_realization(
            awaken_session_realization_contract::BeginSessionRealization {
                expected_resources: None,
                session_id: "retry-budget".into(),
                target: target("worker/2"),
            },
        )
        .await
        .expect("R2 reclaim");
    assert_eq!(second.authority.lease.epoch, 2, "R2/E2");
    assert_eq!(
        repo.get(&SessionId::from("retry-budget"))
            .await
            .unwrap()
            .realization_progress
            .attempts,
        2,
        "R2/E2"
    );
    app.fail_session_realization(
        awaken_session_realization_contract::FailSessionRealization {
            session_id: "retry-budget".into(),
            lease: second.authority.lease,
            prepared_resource_revision: None,
            retryable: true,
            source_run_id: None,
            reason: "worker unavailable again".into(),
        },
    )
    .await
    .expect("R3 exhausted");
    assert_eq!(
        repo.get(&SessionId::from("retry-budget"))
            .await
            .unwrap()
            .execution,
        SessionExecutionState::ActivationFailed,
        "R3/E3"
    );
}

#[tokio::test]
async fn failed_nonterminal_realization_expires_assignment_before_same_incarnation_recovery() {
    // Causes: C1 initial Preparing / already Idle; C2 retryable / permanent;
    // C3 attempt below / at budget; C4 exact / stale failure lease.
    // Decision table: F0 stale -> no root mutation; F1 initial+retry+below ->
    // Preparing, expired assignment; F2 other initial -> ActivationFailed;
    // F3 Idle+any C2/C3 -> Idle, expired assignment, retained active resources.
    // F4 F1/F3+same-incarnation begin -> successor epoch, never reuse the closed
    // generation; old failure replay cannot mutate that successor. The existing
    // running-realization test owns interval/terminal settlement. The HTTP MCP
    // hot-swap test additionally proves the previous Active server still runs.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        Arc::new(awaken_session_store::SqliteManagedSessionRepository::open_in_memory().unwrap());
    let app = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            realization_retry_budget: 2,
            ..Default::default()
        },
    );
    for idle in [false, true] {
        for retryable in [false, true] {
            for attempts in [1, 2] {
                let id = format!("failure-{idle}-{retryable}-{attempts}");
                let lease = awaken_session_resource_contract::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "worker/1".into(),
                    epoch: 7,
                    expires_at_unix_ms: u64::MAX,
                };
                let mut root = persisted(&id, false, if idle { "idle" } else { "preparing" });
                root.realization = Some(lease.clone());
                root.realization_progress.attempts = attempts;
                create(repo.as_ref(), root).await;
                let before = repo.get(&SessionId::from(&id)).await.unwrap();
                let command = awaken_session_realization_contract::FailSessionRealization {
                    session_id: SessionId::from(&id),
                    lease: lease.clone(),
                    prepared_resource_revision: None,
                    retryable,
                    source_run_id: None,
                    reason: "external stage failed".into(),
                };
                let mut stale = command.clone();
                stale.lease.epoch -= 1;
                assert!(app.fail_session_realization(stale).await.is_err(), "F0");
                assert_eq!(repo.get(&SessionId::from(&id)).await.unwrap(), before, "F0");
                app.fail_session_realization(command.clone()).await.unwrap();
                let failed = repo.get(&SessionId::from(&id)).await.unwrap();
                let recoverable = idle || (retryable && attempts < 2);
                assert_eq!(
                    failed.execution,
                    if idle {
                        SessionExecutionState::Idle
                    } else if recoverable {
                        SessionExecutionState::Preparing
                    } else {
                        SessionExecutionState::ActivationFailed
                    },
                    "F1/F2/F3"
                );
                assert_eq!(
                    failed.resources, before.resources,
                    "retain active resource authority"
                );
                assert_eq!(failed.realization.as_ref().unwrap().epoch, 7);
                if recoverable {
                    assert_eq!(
                        failed.realization.as_ref().unwrap().expires_at_unix_ms,
                        0,
                        "F1/F3 root cannot reissue the locally closed generation"
                    );
                    let directive = app
                        .begin_session_realization(
                            awaken_session_realization_contract::BeginSessionRealization {
                                session_id: SessionId::from(&id),
                                expected_resources: None,
                                target:
                                    awaken_session_realization_contract::SessionRealizationTarget {
                                        owner: lease.owner.clone(),
                                        runtime_incarnation: lease.runtime_incarnation.clone(),
                                        lease_expires_at_unix_ms: u64::MAX,
                                        reassign_existing_lease: false,
                                    },
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(directive.authority.lease.epoch, 8, "F4");
                    let successor = repo.get(&SessionId::from(&id)).await.unwrap();
                    assert!(
                        app.fail_session_realization(command).await.is_err(),
                        "F4 stale replay"
                    );
                    assert_eq!(
                        repo.get(&SessionId::from(id)).await.unwrap(),
                        successor,
                        "F4 successor retained"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn lease_renewal_is_a_projection_free_root_cas() {
    // Renewal cause/effect graph: C1 exact live owner/incarnation/epoch; C2 the
    // requested expiry is newer/equal/older; C3 executable projection refresh
    // is unavailable; C4 asserted epoch is stale. Effects: E1 one root CAS
    // extends only lease expiry; E2 exact replay returns the current lease with
    // no write; E3 malformed/non-monotonic input is rejected; E4 stale fencing
    // is rejected; E5 no catalog refresh, transcript materialization, credential
    // migration, or desired-state phase is entered.
    //
    // | Rule | fence | requested expiry | refresh | Effect |
    // |---|---|---|---|---|
    // | R1 | exact/live | newer | unavailable | E1 + E5 |
    // | R2 | exact/live | equal/current | unavailable | E2 + E5 |
    // | R3 | exact/live | older than assertion | any | E3 |
    // | R4 | stale epoch | newer | any | E4 + E5 |
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("renewal Session repository"),
    );
    let asserted = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: u64::MAX - 2,
    };
    let mut session = persisted("lease-only-renewal", true, "idle");
    session.realization = Some(asserted.clone());
    create(repo.as_ref(), session).await;
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    application
        .set_executable_projection_refresh(paired_projection_refresh(refresh.clone()))
        .unwrap();

    let renewed = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: "lease-only-renewal".into(),
                asserted_lease: asserted.clone(),
                requested_expires_at_unix_ms: u64::MAX - 1,
            },
        )
        .await
        .expect("R1 renewal bypasses projection refresh");
    assert_eq!(renewed.lease.expires_at_unix_ms, u64::MAX - 1, "R1/E1");
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 0, "R1/E5");
    let after_first = repo
        .get(&SessionId::from("lease-only-renewal"))
        .await
        .unwrap();

    let replayed = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: "lease-only-renewal".into(),
                asserted_lease: asserted.clone(),
                requested_expires_at_unix_ms: u64::MAX - 1,
            },
        )
        .await
        .expect("R2 response-loss replay");
    // R2 replay preserves the durable fence; response-time metadata may age.
    assert_eq!(replayed.lease, renewed.lease, "R2/E2");
    assert_eq!(
        repo.get(&SessionId::from("lease-only-renewal"))
            .await
            .unwrap(),
        after_first,
        "R2/E2 no second write"
    );

    let older = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: "lease-only-renewal".into(),
                asserted_lease: asserted.clone(),
                requested_expires_at_unix_ms: asserted.expires_at_unix_ms - 1,
            },
        )
        .await;
    assert!(
        matches!(
            older,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "R3/E3"
    );
    let mut stale = asserted;
    stale.epoch -= 1;
    let stale = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: "lease-only-renewal".into(),
                asserted_lease: stale,
                requested_expires_at_unix_ms: u64::MAX,
            },
        )
        .await;
    assert_eq!(
        stale,
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership),
        "R4/E4"
    );
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 0, "R4/E5");
}

#[tokio::test]
async fn terminal_root_renewal_preserves_frozen_mcp_cleanup_identity() {
    // TR1: an active MCP generation was published under execution epoch 7,
    // then terminal cleanup acquired root epoch 8. Renewing that cleanup lease
    // extends only root authority: rewriting or validating the frozen MCP claim
    // against epoch 8 would deadlock cleanup forever. Nonterminal propagation
    // remains covered by `lease_renewal_is_a_projection_free_root_cas` and the
    // MCP aggregate decision table.
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("terminal renewal Session repository"),
    );
    let prior_expiry = u64::MAX - 3;
    let current = awaken_session_resource_contract::SessionRealizationLease {
        owner: "terminal-worker".into(),
        runtime_incarnation: "terminal-worker/boot-2".into(),
        epoch: 8,
        expires_at_unix_ms: u64::MAX - 2,
    };
    let mut session = persisted("terminal-mcp-renewal", true, "terminated");
    assert!(
        session
            .terminal_cleanup
            .request(&session.session_id.clone())
    );
    session.realization = Some(current.clone());
    session.mcp = awaken_session_mcp_contract::SessionMcpAttachmentSet::from_initial(
        vec![awaken_session_mcp_contract::McpAttachmentDraft {
            name: "browser".into(),
            target: awaken_session_mcp_contract::McpTarget::parse_http(
                "https://browser.example.test/mcp",
            )
            .unwrap(),
            prompts_as_skills: false,
            credential: None,
            origin: awaken_session_mcp_contract::McpAttachmentOrigin::Session,
        }],
        None,
    )
    .unwrap();
    // The witness carries the attachment identity, so the claim takes the
    // observed head alone; a separate id could disagree with it.
    let generation = session.mcp.attachments[0].generation.clone();
    session
        .mcp
        .claim_realization(
            &generation,
            awaken_session_mcp_contract::McpRealizationClaim {
                realization_id: "browser-realization".into(),
                runtime_incarnation: "terminal-worker/boot-1".into(),
                lease_epoch: 7,
                lease_expires_at_unix_ms: prior_expiry,
                stage_idempotency_key: "browser-stage".into(),
            },
        )
        .unwrap();
    session
        .mcp
        .activate(&generation, "browser-realization")
        .unwrap();
    let frozen_mcp = session.mcp.clone();
    create(repo.as_ref(), session).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let renewed = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: "terminal-mcp-renewal".into(),
                asserted_lease: current,
                requested_expires_at_unix_ms: u64::MAX - 1,
            },
        )
        .await
        .expect("TR1 terminal root renewal does not claim MCP ownership");
    assert_eq!(renewed.lease.expires_at_unix_ms, u64::MAX - 1);
    assert_eq!(
        repo.get(&SessionId::from("terminal-mcp-renewal"))
            .await
            .unwrap()
            .mcp,
        frozen_mcp,
        "TR1 frozen MCP cleanup identity is unchanged"
    );
}

#[tokio::test]
async fn local_realization_uses_stable_owner_and_process_incarnation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(repo.as_ref(), persisted("local-restart", false, "idle")).await;
    let configured = |owner: &str| SessionApplicationConfiguration {
        execution_placement: SessionExecutionPlacement::LocalWorker,
        local_realization_owner: owner.into(),
        ..Default::default()
    };

    let first = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let first_session = first
        .realize_session(&SessionId::from("local-restart"))
        .await
        .expect("L1");
    let first_lease = first_session.realization.expect("L1 lease");
    assert_eq!(first_lease.owner, "embedded-worker", "L1/E1");
    assert_eq!(
        first
            .realize_session(&SessionId::from("local-restart"))
            .await
            .expect("L1 exact replay")
            .realization
            .expect("L1 replay lease"),
        first_lease,
        "L1/E1"
    );

    let replacement = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        configured("embedded-worker"),
    );
    let replaced = replacement
        .realize_session(&SessionId::from("local-restart"))
        .await
        .expect("L2 same logical owner may fence a crashed incarnation");
    let replaced_lease = replaced.realization.expect("L2 lease");
    assert_eq!(replaced_lease.owner, first_lease.owner, "L2/E2");
    assert_ne!(
        replaced_lease.runtime_incarnation, first_lease.runtime_incarnation,
        "L2/E2"
    );
    assert_eq!(replaced_lease.epoch, first_lease.epoch + 1, "L2/E2");

    let other = application_with_configuration(
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        configured("other-worker"),
    );
    assert!(
        matches!(
            other
                .realize_session(&SessionId::from("local-restart"))
                .await,
            Err(crate::SessionRealizationError::Control(
                awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership
            ))
        ),
        "L3/E3"
    );
    let reassigned =
        awaken_session_realization_contract::SessionRealizationControl::begin_session_realization(
            &other,
            awaken_session_realization_contract::BeginSessionRealization {
                expected_resources: None,
                session_id: "local-restart".into(),
                target: awaken_session_realization_contract::SessionRealizationTarget {
                    owner: "other-worker".into(),
                    runtime_incarnation: "other-worker/boot-2".into(),
                    lease_expires_at_unix_ms: u64::MAX,
                    reassign_existing_lease: true,
                },
            },
        )
        .await
        .expect("L4 claim-authorized reassignment");
    assert_eq!(
        reassigned.authority.lease.epoch,
        replaced_lease.epoch + 1,
        "L4/E4"
    );
    assert_eq!(reassigned.authority.lease.owner, "other-worker", "L4/E4");

    // L5: after Work settlement, the same process can hold a CLOSED local
    // predecessor while Control still has its live root lease. An explicit
    // claim-authorized resume must persist a successor before resource staging.
    // L6: a delayed predecessor renewal cannot extend this successor; ordinary
    // exact replay above (L1) remains unchanged. Cross repeated continuations
    // to rule out a one-time owner-change fix.
    let mut predecessor = reassigned.authority.lease;
    for _ in 0..3 {
        let successor =
            awaken_session_realization_contract::SessionRealizationControl::begin_session_realization(
                &other,
                awaken_session_realization_contract::BeginSessionRealization {
                    expected_resources: None,
                    session_id: "local-restart".into(),
                    target: awaken_session_realization_contract::SessionRealizationTarget {
                        owner: predecessor.owner.clone(),
                        runtime_incarnation: predecessor.runtime_incarnation.clone(),
                        lease_expires_at_unix_ms: u64::MAX,
                        reassign_existing_lease: true,
                    },
                },
            )
            .await
            .expect("L5 same-process continuation");
        assert_eq!(successor.authority.lease.epoch, predecessor.epoch + 1, "L5");
        assert_eq!(
            awaken_session_realization_contract::SessionRealizationControl::renew_session_realization(
                &other,
                awaken_session_realization_contract::RenewSessionRealization {
                    session_id: "local-restart".into(),
                    asserted_lease: predecessor,
                    requested_expires_at_unix_ms: u64::MAX,
                },
            )
            .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership),
            "L6 old renewal cannot extend successor"
        );
        predecessor = successor.authority.lease;
    }
}

/// Reference-projection FMECA and cause/effect decision table. C1 the
/// Resource index accepts or rejects a replacement; C2 the Session root CAS
/// wins or conflicts after projection. Effects are E1 no durable intent is
/// exposed without its safety edge, E2 a losing candidate is repaired back
/// to repository truth, and E3 a winning pending replacement retains both
/// installed and desired targets until settlement.
///
/// | Rule | C1 index | C2 root CAS | Effect |
/// |---|---|---|---|
/// | R1 | rejects | not attempted | E1 unchanged Session |
/// | R2 | accepts | conflicts | E2 installed target only |
/// | R3 | accepts | wins | E3 installed + desired targets |
#[tokio::test]
async fn resource_reference_projection_fails_closed_and_repairs_cas_losers() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut application = application(repo.clone(), environments);
    let references = Arc::new(RecordingReferenceIndex::default());
    application.set_resource_reference_authority(references.clone(), Arc::new(UnusedFileCatalog));

    let mut original = persisted("reference-fence", false, "idle");
    original.resources =
        awaken_session_state_contract::SessionResourceState::from_active(skill_resources("old"));
    create(repo.as_ref(), original).await;

    let mut rejected = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    let original_revision = rejected.revision;
    rejected
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    references.fail_replace.store(true, Ordering::SeqCst);
    assert!(
        application
            .commit_resource_snapshot("workspace", rejected, "R1", Vec::new())
            .await
            .is_err(),
        "R1"
    );
    assert_eq!(
        repo.get(&SessionId::from("reference-fence"))
            .await
            .unwrap()
            .revision,
        original_revision,
        "R1/E1"
    );

    references.fail_replace.store(false, Ordering::SeqCst);
    let mut stale = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    let mut winner = stale.clone();
    winner.title = Some("concurrent winner".into());
    let _fixture = application
        .commit_session_snapshot_with_outcome("workspace", winner, "concurrent", Vec::new())
        .await
        .unwrap();
    stale
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    assert!(matches!(
        application
            .commit_resource_snapshot("workspace", stale, "R2", Vec::new())
            .await,
        Err(SessionMutationError::Conflict)
    ));
    let ids = || {
        references
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|record| record.target.resource_id.clone())
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(ids(), BTreeSet::from(["old".to_string()]), "R2/E2");

    let mut accepted = repo.get(&SessionId::from("reference-fence")).await.unwrap();
    accepted
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("reference-fence"),
            skill_resources("new"),
        )
        .unwrap();
    application
        .commit_resource_snapshot("workspace", accepted, "R3", Vec::new())
        .await
        .unwrap();
    assert_eq!(
        ids(),
        BTreeSet::from(["new".to_string(), "old".to_string()]),
        "R3/E3"
    );
}

/// Legacy Resource adoption FMECA and cause/effect decision table. C1 the
/// retained manifest has no activation records; C2 the Worker reports that
/// it prepared the exact active revision; C3 the reported revision is stale.
/// Effects are E1 one Active/attempted record is committed, E2 no second
/// physical-state authority is created, and E3 a mismatched generation is
/// rejected without mutating durable truth.
///
/// | Rule | Legacy inputs | Prepared revision | Effect |
/// |---|---|---|---|
/// | L1 | present | exact | E1 adopt once |
/// | L2 | present | stale | E3 reject, unchanged |
/// | L3 | absent/already adopted | any | E2 normal realization path |
#[tokio::test]
async fn remote_realization_adopts_only_the_exact_legacy_resource_generation() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 1,
        expires_at_unix_ms: u64::MAX,
    };
    for id in ["legacy-exact", "legacy-stale"] {
        let mut session = persisted(id, false, "preparing");
        session.resources =
            awaken_session_state_contract::SessionResourceState::from_active(file_resources(id));
        session.realization = Some(lease.clone());
        create(repo.as_ref(), session).await;
    }

    let exact = awaken_session_realization_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_realization_contract::ActivateSessionRealization {
            session_id: "legacy-exact".into(),
            lease: lease.clone(),
            prepared_resource_revision: Some(1),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect("L1 exact legacy generation");
    assert!(
        matches!(
            exact.action,
            awaken_session_realization_contract::SessionRealizationAction::Publish { .. }
        ),
        "L1/E1"
    );
    let adopted = repo
        .get(&SessionId::from("legacy-exact"))
        .await
        .expect("L1 durable truth");
    assert_eq!(adopted.resources.activations.len(), 1, "L1/E1");
    assert_eq!(
        adopted.resources.activations[0].state,
        awaken_session_state_contract::ActivationState::Active,
        "L1/E1"
    );
    assert_eq!(adopted.resources.activations[0].attempts, 1, "L1/E1");

    let stale = awaken_session_realization_contract::SessionRealizationControl::activate_session_realization(
        &application,
        awaken_session_realization_contract::ActivateSessionRealization {
            session_id: "legacy-stale".into(),
            lease,
            prepared_resource_revision: Some(2),
            mcp_receipts: Vec::new(),
        },
    )
    .await
    .expect_err("L2 stale generation must fail closed");
    assert!(
        matches!(
            stale,
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "L2/E3: {stale}"
    );
    assert!(
        repo.get(&SessionId::from("legacy-stale"))
            .await
            .expect("L2 durable truth")
            .resources
            .activations
            .is_empty(),
        "L2/E3"
    );
}

/// Terminal Worker-placement FMECA and cause/effect decision table. C1 the
/// Session placement delegates live realization to a Worker; C2 the Session
/// is live with pending work; C3 it is terminal with Releasing work. Effects
/// are E1 no Coordinator execution effect for a live generation and E2 the
/// sole terminal cleanup path converges because no future Run can claim it.
///
/// | Rule | Placement | Lifecycle | Resource state | Effect |
/// |---|---|---|---|---|
/// | W1 | Worker | idle | Prepared | E1 remain unattempted |
/// | W2 | Worker | terminal, current renewal live | Releasing, no preparation | E2 remain pending without Coordinator failure |
/// | W3 | Worker | expired assertion + current renewal | canonical Worker driver | E3 prepare, dispose, release durably |
#[test]
fn worker_placement_defers_live_effects_but_not_terminal_cleanup() {
    // Coverage rationale: the async case below remains the sole W1-W3 oracle.
    // The shared composed-test executor changes stack placement only and adds
    // no realization owner, cleanup queue, or alternate receipt path.
    run_composed_async_test(worker_placement_defers_live_effects_but_not_terminal_cleanup_case);
}

async fn worker_placement_defers_live_effects_but_not_terminal_cleanup_case() {
    // Constraint/Invariant: Worker placement owns live realization, while the
    // Session repository still owns terminal cleanup progress. Decision rule:
    // execute W1-W3 and distinguish deferred live effects, pending cleanup, and
    // exact two-stage driver completion.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let worker_baseline = |id: &str, status: &str| {
        let mut session = persisted(id, false, status);
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = SessionRuntimePlacement::Worker;
        session
    };

    let mut live = worker_baseline("worker-live", "idle");
    live.resources
        .prepare(
            &awaken_session_types::SessionId::from("worker-live"),
            file_resources("live"),
        )
        .expect("W1 prepared generation");
    create(repo.as_ref(), live).await;

    let mut terminal = worker_baseline("worker-terminal", "terminated");
    let asserted_terminal_lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-terminal-owner".into(),
        runtime_incarnation: "worker-terminal-incarnation".into(),
        epoch: 1,
        expires_at_unix_ms: awaken_agent_contract::clock::Clock::now_ms(
            &awaken_agent_contract::clock::SystemClock,
        )
        .saturating_sub(20_001),
    };
    let terminal_lease = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: awaken_agent_contract::clock::Clock::now_ms(
            &awaken_agent_contract::clock::SystemClock,
        )
        .saturating_add(30_000),
        ..asserted_terminal_lease.clone()
    };
    terminal.realization = Some(terminal_lease.clone());
    terminal.resources = awaken_session_state_contract::SessionResourceState::from_active(
        file_resources("terminal"),
    );
    terminal
        .resources
        .adopt_legacy_active(&awaken_session_types::SessionId::from("worker-terminal"));
    terminal
        .resources
        .begin_release()
        .expect("W2 release intent");
    create(repo.as_ref(), terminal).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let report = application.reconcile_resource_activations().await;
    assert!(report.failures.is_empty(), "W2/E2: {:#?}", report.failures);
    assert_eq!(report.settled.len(), 1, "W2/E2 durable pending root");

    let live = repo
        .get(&SessionId::from("worker-live"))
        .await
        .expect("W1 durable truth");
    assert!(live.resources.pending.is_some(), "W1/E1");
    assert_eq!(live.resources.activations[0].attempts, 0, "W1/E1");
    let pending = repo
        .get(&SessionId::from("worker-terminal"))
        .await
        .expect("W2 durable truth");
    assert!(pending.terminal_cleanup.is_requested(), "W2/E2");
    let work = application
        .terminal_cleanup_work(
            &SessionId::from("worker-terminal"),
            &asserted_terminal_lease,
        )
        .await
        .unwrap()
        .expect("W2 terminal fence");
    assert_eq!(
        work.assignment.authority.lease, terminal_lease,
        "W2 exact root lease"
    );
    let commands = terminal_prepare_commands(work);
    assert_eq!(commands.len(), 1, "W2/E2");
    assert_eq!(commands[0].thread_id, "worker-terminal", "W2/E2");
    crate::drive_session_terminal_cleanup(
        &SessionId::from("worker-terminal"),
        &asserted_terminal_lease,
        &application,
        &NoopRuntime,
    )
    .await
    .expect("W3 canonical Worker driver");
    let terminal = repo
        .get(&SessionId::from("worker-terminal"))
        .await
        .expect("W3 durable truth");
    assert!(
        terminal
            .resources
            .activations
            .iter()
            .all(|activation| activation.state
                == awaken_session_state_contract::ActivationState::Released),
        "W3/E3 durable={:?}",
        terminal.resources.activations,
    );
}

#[tokio::test]
async fn remote_terminal_cleanup_uses_canonical_driver_and_cold_completion_replay() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 a terminal Worker-placed Session retains one exact
    // realization lease and may receive a compatible shorter assertion; C2 the
    // frozen target set contains root and child; C3 the external Worker has not
    // driven, drives the canonical two-stage protocol, or replays after the
    // final disposal CAS; C4 the asserted lease is exact or stale; C5 an
    // internal lifecycle owner requests a completion-bearing force archive.
    // Effects: E1 Coordinator performs no Runtime effect, retains the aggregate
    // queue, and reports pending rather than a false successful completion;
    // E2 the Worker sees child-first preparation under the exact assignment;
    // E3 its single driver durably prepares both targets before one disposal;
    // E4 stale authority fails closed; E5 Completed replay performs no effect.
    //
    // | Rule | lease | durable phase | Effect |
    // | W1 | force archive + external owner not driven | Requested | E1 + E2 |
    // | W2 | stale | Requested | E4 |
    // | W3 | exact | Requested | E3 |
    // | W4 | force archive replay after exact receipt | Completed | E5 + success |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("remote cleanup Session repository"),
    );
    let asserted_lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:incarnation-1".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    };
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: awaken_agent_contract::clock::Clock::now_ms(
            &awaken_agent_contract::clock::SystemClock,
        )
        .saturating_add(30_000),
        ..asserted_lease.clone()
    };
    let mut session = persisted("remote-cleanup", true, "terminated");
    session.realization = Some(lease.clone());
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    *runtime.delegated_snapshot.lock().unwrap() =
        awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: Vec::new(),
            coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
                "remote-cleanup-child".into(),
            )],
            watermark: 31,
            runtime_commit_cursor: 41,
        };
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );

    let pending_force = application
        .force_terminate_session(
            "remote-cleanup",
            "2026-09-05T00:00:00Z",
            awaken_session_state_contract::ManagedLifecycleFact {
                id: "remote-cleanup:terminated".into(),
                object_id: "remote-cleanup".into(),
                workspace_id: Some("workspace".into()),
                event_type: "session.status_terminated".into(),
                timestamp: 1,
                runtime_interval: None,
            },
        )
        .await;
    assert!(
        matches!(pending_force, Err(SessionPreparationError::Unavailable(ref message))
            if message == "Session archive cleanup remains pending"),
        "W1/E1: {pending_force:?}"
    );
    let retained = repo
        .get(&SessionId::from("remote-cleanup"))
        .await
        .expect("W1 aggregate remains the Worker queue");
    assert!(retained.terminal_cleanup.is_requested(), "W1/E1");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W1/E1");
    let mut admitted_lease = asserted_lease.clone();
    admitted_lease.expires_at_unix_ms = 0;
    let work = application
        .terminal_cleanup_work(&SessionId::from("remote-cleanup"), &admitted_lease)
        .await
        .expect("W1 exact lease")
        .expect("W1 terminal fence");
    assert_eq!(work.assignment.session_id, "remote-cleanup", "W1/E2");
    assert_eq!(
        work.assignment.authority.lease, lease,
        "W1/E2 current lease"
    );
    let commands = terminal_prepare_commands(work);
    assert_eq!(commands.len(), 1, "W1/E2 child phase");
    assert_eq!(commands[0].thread_id, "remote-cleanup-child", "W1/E2");

    let mut stale = admitted_lease;
    stale.epoch += 1;
    assert!(
        matches!(
            application
                .terminal_cleanup_work(&SessionId::from("remote-cleanup"), &stale)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "W2/E4"
    );

    let worker_runtime = RecordingCleanupRuntime::default();
    let outcome = crate::drive_session_terminal_cleanup(
        &SessionId::from("remote-cleanup"),
        &asserted_lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("W3 canonical Worker driver");
    assert_eq!(
        outcome,
        crate::SessionTerminalCleanupDriveOutcome::Completed,
        "W3/E3"
    );
    let replay = application
        .force_terminate_session(
            "remote-cleanup",
            "2026-09-05T00:00:00Z",
            awaken_session_state_contract::ManagedLifecycleFact {
                id: "remote-cleanup:terminated".into(),
                object_id: "remote-cleanup".into(),
                workspace_id: Some("workspace".into()),
                event_type: "session.status_terminated".into(),
                timestamp: 1,
                runtime_interval: None,
            },
        )
        .await
        .expect("W4 completed cleanup replay");
    assert!(!replay.transitioned(), "W4 exact archive replay");
    let completed = repo
        .get(&SessionId::from("remote-cleanup"))
        .await
        .expect("W3 durable Session");
    assert!(completed.terminal_cleanup.is_completed(), "W3/E3");
    assert_eq!(worker_runtime.intents.lock().unwrap().len(), 2, "W3/E3");
    assert_eq!(
        *worker_runtime.terminal_effect_order.lock().unwrap(),
        vec![
            "cleanup:remote-cleanup-child".to_string(),
            "cleanup:remote-cleanup".to_string(),
            "dispose".to_string(),
        ],
        "W3/E3 prepare child, prepare root, dispose once"
    );
    assert!(runtime.intents.lock().unwrap().is_empty(), "W3/E1");

    crate::drive_session_terminal_cleanup(
        &SessionId::from("remote-cleanup"),
        &asserted_lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("W4/E5 exact Completed replay");
    assert_eq!(worker_runtime.intents.lock().unwrap().len(), 2, "W4/E5");
    assert!(runtime.intents.lock().unwrap().is_empty(), "W4/E5");
}

#[tokio::test]
async fn restoring_terminal_cleanup_binds_the_exact_target_only_to_the_root() {
    // Cause/effect graph: C1 the durable Environment is Restoring; C2 the
    // frozen cleanup set contains a child and the root; C3 a preparation
    // command is child/root; C4 a child carries no target or forges the root
    // target. Effects: E1 child-first work remains executable with no restore
    // target; E2 a forged child target is rejected before Runtime I/O; E3 the
    // later root preparation carries the exact durable request; E4 Disposal
    // carries that same request and atomically completes the one cleanup.
    //
    // | Rule | thread | asserted target | Effect |
    // |---|---|---|---|
    // | R1 | child | none | E1 |
    // | R2 | child | root target | E2 |
    // | R3 | root | exact durable target | E3 then E4 |
    //
    // Constraint: child and root are phases of one aggregate operation. The
    // restore request is transport evidence for the root physical target, not
    // another cleanup state machine or a child-owned effect.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Restoring cleanup Session repository"),
    );
    let session_id = "restoring-cleanup";
    let child_id = "restoring-cleanup-child";
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-restoring".into(),
        runtime_incarnation: "worker-restoring:incarnation-1".into(),
        epoch: 3,
        expires_at_unix_ms: awaken_agent_contract::clock::Clock::now_ms(
            &awaken_agent_contract::clock::SystemClock,
        )
        .saturating_add(30_000),
    };
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(session_id),
        1,
        90_000,
        "restoring-environment",
        "restoring-base-image",
    );
    let checkpoint = awaken_session_state_contract::SandboxCheckpointRef {
        id: "restoring-checkpoint".into(),
        format: "awaken-fs-tar-v1".into(),
        digest: "restoring-checkpoint-digest".into(),
        size_bytes: 7,
        created_at_unix_ms: 1,
        expires_at_unix_ms: 90_000,
        environment_fingerprint: generation.environment_fingerprint.clone(),
        base_image_fingerprint: generation.base_image_fingerprint.clone(),
        excluded_mounts: Vec::new(),
        suspend_effect_id: "restoring-suspend-effect".into(),
    };
    let operation = awaken_session_state_contract::SessionEnvironmentOperation::new(
        "workspace",
        &awaken_session_types::SessionId::from(session_id),
        "restore",
        &generation,
        1,
        Some(lease.clone()),
        Some(&checkpoint),
    );
    let mut session = persisted(session_id, true, "terminated");
    session.realization = Some(lease.clone());
    session.environment = awaken_session_state_contract::SessionEnvironmentState::Restoring {
        operation,
        checkpoint,
        generation,
    };
    let exact_restore = session
        .environment
        .restoring_request(
            "workspace",
            &awaken_session_types::SessionId::from(session_id),
        )
        .expect("R3 exact durable restore request");
    create(repo.as_ref(), session).await;

    let coordinator_runtime = Arc::new(RecordingCleanupRuntime::default());
    *coordinator_runtime.delegated_snapshot.lock().unwrap() =
        awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: Vec::new(),
            coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(child_id.into())],
            watermark: 5,
            runtime_commit_cursor: 8,
        };
    let application = SessionApplication::new_with_configuration(
        coordinator_runtime,
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    application
        .release_terminal_resources("workspace", session_id)
        .await
        .expect("R1 terminal intent")
        .expect("R1 aggregate remains pending");

    let work = application
        .terminal_cleanup_work(&SessionId::from(session_id), &lease)
        .await
        .expect("R1 child work projection")
        .expect("R1 pending child work");
    let child = terminal_prepare_commands(work)
        .into_iter()
        .next()
        .expect("R1 child command");
    assert_eq!(child.thread_id, child_id, "R1/E1 child-first");
    assert_eq!(child.restore_target, None, "R1/E1 root-only evidence");

    let mut forged_child = child.clone();
    forged_child.restore_target = Some(exact_restore.clone());
    let forged_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        forged_child,
        lease.clone(),
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&forged_effect)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "R2/E2"
    );

    let worker_runtime = RecordingCleanupRuntime::default();
    let outcome = crate::drive_session_terminal_cleanup(
        &SessionId::from(session_id),
        &lease,
        &application,
        &worker_runtime,
    )
    .await
    .expect("R3/R4 canonical cleanup drive");
    assert_eq!(
        outcome,
        crate::SessionTerminalCleanupDriveOutcome::Completed,
        "R3/R4"
    );
    {
        let intents = worker_runtime.intents.lock().unwrap();
        assert_eq!(intents.len(), 2, "R1/R3 child then root");
        assert_eq!(intents[0].thread_id, child_id, "R1/E1");
        assert_eq!(intents[0].restore_target, None, "R1/E1");
        assert_eq!(intents[1].thread_id, session_id, "R3/E3");
        assert_eq!(
            intents[1].restore_target.as_ref(),
            Some(&exact_restore),
            "R3/E3 exact root request"
        );
    }
    let completed = repo
        .get(&SessionId::from(session_id))
        .await
        .expect("R4 durable completion");
    assert!(completed.terminal_cleanup.is_completed(), "R4/E4");
    assert!(
        matches!(
            completed.environment,
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
        ),
        "R4/E4"
    );
}

/// Repository-publication control cause/effect graph. C1 a Requested terminal
/// operation has an exact publication intent; C2 a child preparation is absent
/// or durable; C3 the asserted lease is exact or stale; C4 the publication
/// receipt is mismatched, exact, or an exact response-loss replay; C5 ordinary
/// cleanup commands may be empty while publication is pending; C6 the durable
/// Session row has one immutable Workspace owner; C7 the publication outcome is
/// an exact receipt or typed permanent rejection; C8 each cleanup poll projects
/// its assignment and action from the same current root snapshot. Effects: E1 the
/// child is the sole first preparation; E2 publication remains hidden behind the
/// child and projects `Waiting`; E3 a Waiting publication remains cold-claimable; E4 stale/wrong
/// evidence fails without mutation; E5 the exact receipt is durably replayable;
/// E6 only then is root preparation exposed and the canonical driver performs
/// the one aggregate disposal;
/// E7 the command projection carries that canonical Workspace beside the
/// command so transports need not accept a Worker-selected tenant; E8 an exact
/// rejection is durable and replayable before ordinary root preparation without
/// a second Git command; E9 every cleanup work value carries the current lease
/// and post-outcome root revision. C9 a pending credentialed Repository publication is claimed only by a Worker
/// with the existing Session-Resources and Repository-credential capabilities;
/// C10 its aggregate Environment either names an existing live publication
/// source or would require terminal cleanup to invent/restore one; C11 the
/// asserted terminal timestamp may be expired while the current same-generation
/// root is live, but an expired current root cannot start publication.
///
/// | Rule | child preparation | lease | publication outcome | Effect |
/// |---|---|---|---|---|
/// | P1 | pending | exact | none | E1 + E2 + E9 |
/// | P2 | complete | stale | none | E4 |
/// | P3 | none | expired same-owner Resident source | none, action=Waiting | E3 + E7 + E9 |
/// | P3a | none | unclaimed, credential capability absent | none | E4 |
/// | P3b | none | Unmaterialized source | none | E4, zero root CAS |
/// | P3c | none | asserted expired, current same-generation live | none | E3 + E7 + E9 |
/// | P3d | none | asserted and current expired | none | E4 before projection |
/// | P4 | complete | exact | wrong | E4 |
/// | P5 | complete | exact | exact/replay | E5 + E6 + E9 |
/// | P6 | complete | exact successor of expired Resident affinity | rejection/replay | E8 + E6 + E9 |
fn repository_publication_terminal_session(
    session_id: &str,
    child: Option<&str>,
    lease: Option<awaken_session_resource_contract::SessionRealizationLease>,
    environment: awaken_session_state_contract::SessionEnvironmentState,
) -> PersistedSession {
    let resources = credentialed_repository_resources("source", "repo-1");
    let intent = repository_publication_intent(&resources);
    let mut session = persisted(session_id, true, "terminated");
    session.environment = environment;
    session.resources = awaken_session_state_contract::SessionResourceState::from_active(resources);
    session
        .terminal_cleanup
        .request_with_publication(&awaken_session_types::SessionId::from(session_id), intent)
        .expect("publication fence");
    session
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from(session_id),
            child.into_iter().map(str::to_string),
            17,
            19,
        )
        .expect("publication cleanup target");
    session.realization = lease;
    session
}
#[tokio::test]
async fn remote_repository_publication_is_child_gated_lease_fenced_and_replayable() {
    // Constraint: SessionCleanupOperation is the only queue/receipt registry;
    // this matrix therefore observes only its command projections and root CAS.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Repository publication Session repository"),
    );
    let exact_lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:publication".into(),
        epoch: 4,
        expires_at_unix_ms: u64::MAX,
    };
    create(
        repo.as_ref(),
        repository_publication_terminal_session(
            "publication-child-barrier",
            Some("publication-child"),
            Some(exact_lease.clone()),
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding: "publication-child-barrier:publication-source".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        ),
    )
    .await;
    let expired_cold_lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-b".into(),
        runtime_incarnation: "worker-b:previous-publication".into(),
        epoch: 2,
        expires_at_unix_ms: 1,
    };
    create(
        repo.as_ref(),
        repository_publication_terminal_session(
            "publication-cold-claim",
            None,
            Some(expired_cold_lease.clone()),
            awaken_session_state_contract::SessionEnvironmentState::Resident {
                binding: "publication-cold-claim:publication-source".into(),
                effect_id: None,
                generation: None,
                idle_since_unix_ms: None,
            },
        ),
    )
    .await;
    let resources = credentialed_repository_resources("source", "repo-1");
    let mut rejected_intent = repository_publication_intent(&resources);
    rejected_intent.expectation.expected_prior_commit =
        Some("1111111111111111111111111111111111111111".into());
    let mut rejected = persisted("publication-rejected", true, "terminated");
    rejected.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "publication-rejected:publication-source".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    rejected.realization = Some(expired_cold_lease.clone());
    rejected.resources =
        awaken_session_state_contract::SessionResourceState::from_active(resources);
    rejected
        .terminal_cleanup
        .request_with_publication(
            &awaken_session_types::SessionId::from("publication-rejected"),
            rejected_intent,
        )
        .unwrap();
    rejected
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from("publication-rejected"),
            [],
            17,
            19,
        )
        .unwrap();
    create(repo.as_ref(), rejected).await;
    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            application
                .terminal_repository_publication_command(
                    &SessionId::from("publication-cold-claim"),
                    &expired_cold_lease,
                )
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "P3d/E4 expired current root cannot project a publication"
    );

    let child_work = application
        .terminal_cleanup_work(&SessionId::from("publication-child-barrier"), &exact_lease)
        .await
        .expect("P1 exact lease")
        .expect("P1 terminal fence");
    assert_eq!(
        child_work.assignment.authority.lease, exact_lease,
        "P1 current lease"
    );
    let child_commands = terminal_prepare_commands(child_work);
    assert_eq!(child_commands.len(), 1, "P1/E1");
    assert_eq!(child_commands[0].thread_id, "publication-child", "P1/E1");
    assert!(
        application
            .terminal_repository_publication_command(
                &SessionId::from("publication-child-barrier"),
                &exact_lease,
            )
            .await
            .expect("P1 exact lease")
            .is_none(),
        "P1/E2"
    );
    let child_worker = RecordingCleanupRuntime::default();
    child_worker
        .fail_publication_once
        .store(true, Ordering::SeqCst);
    crate::drive_session_terminal_cleanup(
        &SessionId::from("publication-child-barrier"),
        &exact_lease,
        &application,
        &child_worker,
    )
    .await
    .expect_err("P2 injected publication failure stops after child preparation");
    assert!(
        matches!(
            application
                .terminal_cleanup_work(&SessionId::from("publication-child-barrier"), &exact_lease)
                .await
                .unwrap()
                .unwrap()
                .action,
            awaken_session_state_contract::SessionTerminalCleanupAction::Waiting
        ),
        "P2 publication barrier withholds root cleanup"
    );
    let mut stale = exact_lease.clone();
    stale.epoch += 1;
    assert!(
        matches!(
            application
                .terminal_repository_publication_command(
                    &SessionId::from("publication-child-barrier"),
                    &stale,
                )
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "P2/E4"
    );

    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker-b".into(),
        runtime_incarnation: "worker-b:1:publication".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(&target, manifest.clone())]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install publication Worker observations");
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("P3a incompatible Worker is skipped")
            .is_none(),
        "P3a/E4 missing Repository credential capability"
    );
    manifest
        .capabilities
        .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
    let assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("P3 claim")
        .expect("P3 publication-only assignment");
    assert_eq!(assignment.session_id, "publication-cold-claim", "P3/E3");
    assert!(
        matches!(
            application
                .terminal_cleanup_work(&assignment.session_id, &assignment.authority.lease)
                .await
                .unwrap()
                .unwrap()
                .action,
            awaken_session_state_contract::SessionTerminalCleanupAction::Waiting
        ),
        "P3/E3"
    );
    let expired_assertion = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: 1,
        ..assignment.authority.lease.clone()
    };
    let publication = application
        .terminal_repository_publication_command(&assignment.session_id, &expired_assertion)
        .await
        .expect("P3c expired assertion publication poll")
        .expect("P3c publication projection");
    assert_eq!(publication.workspace_id, "workspace", "P3c/E7");
    assert_eq!(
        publication.current_lease, assignment.authority.lease,
        "P3c/E8 projection carries the current live same-generation root"
    );
    let publication = publication.command;
    let awaken_session_resource_contract::ResolvedInputSource::Repository {
        repository_id,
        config,
        ..
    } = &publication.intent.input.source
    else {
        panic!("publication fixture is a Repository")
    };
    let wrong = awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
        &publication,
        awaken_provisioning_contract::RepositoryPublicationReceipt {
            repository_id: repository_id.to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: "wrong-branch".into(),
            commit: publication.intent.expectation.commit.clone(),
        },
    );
    assert!(
        matches!(
            application
                .record_terminal_repository_publication_receipt(
                    &assignment.session_id,
                    &assignment.authority.lease,
                    wrong,
                )
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "P4/E4"
    );
    assert!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&assignment.session_id)
            .unwrap()
            .is_none(),
        "P4/E4 no mutation"
    );

    let exact = awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
        &publication,
        awaken_provisioning_contract::RepositoryPublicationReceipt {
            repository_id: repository_id.to_string(),
            source_remote_url: config.remote_url.clone(),
            branch: publication.intent.expectation.branch.clone(),
            commit: publication.intent.expectation.commit.clone(),
        },
    );
    application
        .record_terminal_repository_publication_receipt(
            &assignment.session_id,
            &assignment.authority.lease,
            exact.clone(),
        )
        .await
        .expect("P5 exact receipt");
    application
        .record_terminal_repository_publication_receipt(
            &assignment.session_id,
            &assignment.authority.lease,
            exact.clone(),
        )
        .await
        .expect("P5 response-loss replay");
    assert_eq!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .repository_publication_receipt(&assignment.session_id)
            .unwrap(),
        Some(&exact),
        "P5/E5"
    );
    assert!(
        application
            .terminal_repository_publication_command(
                &assignment.session_id,
                &assignment.authority.lease
            )
            .await
            .unwrap()
            .is_none(),
        "P5/E6"
    );
    let root_work = application
        .terminal_cleanup_work(&assignment.session_id, &assignment.authority.lease)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        root_work.assignment.session_id, assignment.session_id,
        "P5 current assignment Session"
    );
    assert_eq!(
        root_work.assignment.authority.lease, assignment.authority.lease,
        "P5 current assignment generation"
    );
    assert!(
        root_work.assignment.projection.revision.0 > assignment.projection.revision.0,
        "P5 work refreshes the frozen root snapshot after publication"
    );
    let root = terminal_prepare_commands(root_work);
    assert_eq!(root.len(), 1, "P5/E6");
    assert_eq!(root[0].thread_id, assignment.session_id, "P5/E6");
    let root_worker = RecordingCleanupRuntime::default();
    crate::drive_session_terminal_cleanup(
        &assignment.session_id,
        &assignment.authority.lease,
        &application,
        &root_worker,
    )
    .await
    .expect("P5 root preparation and aggregate disposal");
    assert!(
        repo.get(&assignment.session_id)
            .await
            .unwrap()
            .terminal_cleanup
            .is_completed(),
        "P5/E6"
    );
    let rejected_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("P6 claim")
        .expect("P6 rejected publication assignment");
    assert_eq!(
        rejected_assignment.session_id, "publication-rejected",
        "P6 exact Session"
    );
    let projection = application
        .terminal_repository_publication_command(
            &rejected_assignment.session_id,
            &rejected_assignment.authority.lease,
        )
        .await
        .unwrap()
        .expect("P6 command");
    let rejection = awaken_session_state_contract::SessionRepositoryPublicationRejection::new(
        &projection.command,
        awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
    )
    .unwrap();
    application
        .record_terminal_repository_publication_rejection(
            &rejected_assignment.session_id,
            &rejected_assignment.authority.lease,
            rejection.clone(),
        )
        .await
        .expect("P6 exact rejection");
    application
        .record_terminal_repository_publication_rejection(
            &rejected_assignment.session_id,
            &rejected_assignment.authority.lease,
            rejection.clone(),
        )
        .await
        .expect("P6 response-loss replay");
    assert!(
        application
            .terminal_repository_publication_command(
                &rejected_assignment.session_id,
                &rejected_assignment.authority.lease,
            )
            .await
            .unwrap()
            .is_none(),
        "P6/E8 no second Git command"
    );
    let root_work = application
        .terminal_cleanup_work(
            &rejected_assignment.session_id,
            &rejected_assignment.authority.lease,
        )
        .await
        .unwrap()
        .expect("P6 ordinary root work");
    assert!(
        root_work.assignment.projection.revision.0 > rejected_assignment.projection.revision.0,
        "P6/E9 work refreshes the root after the rejection CAS"
    );
    let root = terminal_prepare_commands(root_work);
    assert_eq!(root.len(), 1, "P6 ordinary root preparation is exposed");
    assert_eq!(
        root[0].thread_id, rejected_assignment.session_id,
        "P6 root preparation"
    );
    let rejected_worker = RecordingCleanupRuntime::default();
    crate::drive_session_terminal_cleanup(
        &rejected_assignment.session_id,
        &rejected_assignment.authority.lease,
        &application,
        &rejected_worker,
    )
    .await
    .expect("P6 root preparation and aggregate disposal");
    let durable = repo
        .get(&SessionId::from("publication-rejected"))
        .await
        .unwrap();
    assert!(durable.terminal_cleanup.is_completed(), "P6/E6 completed");
    assert_eq!(
        durable
            .terminal_cleanup
            .repository_publication_rejection(&awaken_session_types::SessionId::from(
                "publication-rejected"
            ))
            .unwrap(),
        Some(&rejection),
        "P6/E8 rejection survives the same root CAS lifecycle"
    );
    let invalid_source = repository_publication_terminal_session(
        "publication-unmaterialized",
        None,
        None,
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
    );
    create(repo.as_ref(), invalid_source).await;
    assert!(
        matches!(
            application.claim_next_terminal_cleanup(target).await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "P3b/E4 an Unmaterialized Session cannot acquire a publication lease"
    );
    assert!(
        repo.get(&SessionId::from("publication-unmaterialized"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "P3b/E4 invalid source admission performs zero root mutation"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_pages_past_a_saturated_invalid_batch() {
    // Cause/effect graph: C1 the reconciliation index begins with one complete
    // fixed-size page of decode-valid terminal rows; C2 every row in that page
    // has a pending Repository publication but no canonical live source; C3 a
    // healthy Requested terminal row sorts immediately after the saturated
    // page; C4 the authenticated Worker is eligible for the healthy action;
    // C5 the healthy row's root CAS commits but its response is lost once.
    // Effects: E1 each invalid row fails closed with zero root mutation; E2 the
    // scan advances only by the repository's typed keyset cursor; E3 the same
    // claim call reaches and fences the healthy row; E4 no unbounded query,
    // quarantine track, or ignore-list becomes a second recovery authority;
    // E5 the retry starts from canonical readback and returns the same epoch.
    //
    // | Rule | first page | next row | Worker | Effect |
    // |---|---|---|---|---|
    // | H1 | fewer than 256 invalid | any | eligible | existing in-page scan |
    // | H2 | exactly 256 invalid | healthy + one lost CAS response | eligible | E1 + E2 + E3 + E4 + E5 |
    // | H3 | exactly 256 invalid | absent | eligible | first Invalid after all pages |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("paged cleanup Session repository"),
    );
    for index in 0..256 {
        let session_id = format!("hol-a-invalid-{index:03}");
        create(
            repo.as_ref(),
            repository_publication_terminal_session(
                &session_id,
                None,
                None,
                awaken_session_state_contract::SessionEnvironmentState::Unmaterialized,
            ),
        )
        .await;
    }
    create(
        repo.as_ref(),
        registry_race_terminal_session("hol-z-healthy", None),
    )
    .await;

    let target = registry_race_target();
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = repo.clone();
    let faulting = Arc::new(FaultingSessionRepository::new(repository));
    faulting.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let application = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");

    let assignment = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("H2 typed pagination skips the saturated invalid page")
        .expect("H2 healthy row remains visible");
    assert_eq!(assignment.session_id, "hol-z-healthy", "H2/E2/E3");
    assert_eq!(
        assignment.authority.lease.epoch, 1,
        "H2/E5 exact CAS readback"
    );
    assert!(
        workers.list_calls.load(Ordering::SeqCst) >= 2,
        "H2/E5 retries revalidate the Worker before traversing the keyset again"
    );
    assert!(
        repo.get(&SessionId::from("hol-a-invalid-000"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "H2/E1 invalid rows remain mutation-free"
    );
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(registry_race_target())
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "H3 returns the first fail-closed projection error only after every page has no claimable row"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_uses_the_existing_scan_and_fences_one_assignment() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    // Cause/effect graph: C1 placement is Worker-owned; C2 cleanup has an
    // immutable Requested command; C3 the realization lease is absent, expired
    // under the same logical Worker installation, exact-current with a strictly
    // older or equal expiry, live under that owner's predecessor incarnation,
    // or belongs to a foreign logical Worker; C4 the authenticated registry
    // snapshot is current and explicitly supports the closed cleanup protocol;
    // C5 the root CAS response is lost after commit. Effects: E1 an unbound,
    // expired same-owner, or exact-current strictly older lease allocates epoch
    // N+1 and returns the frozen projection/lease; E2 foreign, predecessor,
    // ineligible, Fenced, nonterminal, and local rows perform zero root CAS; E3
    // exact-current equal expiry is skipped so one unavailable candidate cannot
    // starve the scan; E4 this invocation replays its exact committed assignment
    // after an ambiguous CAS response. Commands remain exclusively in
    // terminal_cleanup, and Worker compatibility is an AND gate, never authority
    // to cross the provider's stable installation namespace.
    //
    // | Rule | C1+C2 | lease | Effect |
    // | C1 | yes | same owner, predecessor incarnation, live | E2 |
    // | C2 | yes | absent | E1 |
    // | C3 | yes | expired same logical owner | E1 after C2 is current/skipped |
    // | C4 | yes | exact current, strictly older expiry | E1 once per heartbeat target |
    // | C5 | yes | exact current, equal expiry | E3 |
    // | C6 | yes | live or expired foreign owner | E2; immutable provider affinity |
    // | C7 | no / Fenced / unauthenticated or legacy Worker | any | E2 |
    // | C8 | yes + ambiguous committed CAS | exact attempted | E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("cold cleanup Session repository"),
    );
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a:2:boot-new".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let lease = |owner: &str, incarnation: &str, epoch: u64, expiry: u64| {
        awaken_session_resource_contract::SessionRealizationLease {
            owner: owner.into(),
            runtime_incarnation: incarnation.into(),
            epoch,
            expires_at_unix_ms: expiry,
        }
    };
    let requested = |id: &str, current| {
        let mut session = persisted(id, true, "terminated");
        assert!(
            session
                .terminal_cleanup
                .request(&awaken_session_types::SessionId::from(id))
        );
        session
            .terminal_cleanup
            .freeze_targets(&awaken_session_types::SessionId::from(id), [], 0, 0)
            .expect("freeze root cleanup target");
        session.realization = current;
        session
    };

    create(
        repo.as_ref(),
        requested(
            "claim-a-current",
            Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-b-foreign-live",
            Some(lease("worker-b", "worker-b:1:boot", 5, u64::MAX)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-b0-current-renewable",
            Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX - 1)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-c-predecessor",
            Some(lease("worker-a", "worker-a:1:boot-old", 7, u64::MAX)),
        ),
    )
    .await;
    create(repo.as_ref(), requested("claim-d-absent", None)).await;
    create(
        repo.as_ref(),
        requested(
            "claim-e-expired",
            Some(lease("worker-a", "worker-a:1:boot-old", 9, 1)),
        ),
    )
    .await;
    create(
        repo.as_ref(),
        requested(
            "claim-f-expired-foreign",
            Some(lease("worker-b", "worker-b:1:boot", 11, 1)),
        ),
    )
    .await;
    let mut fenced = persisted("claim-g-fenced", true, "terminated");
    assert!(
        fenced
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from("claim-g-fenced"))
    );
    create(repo.as_ref(), fenced).await;
    let mut local = requested("claim-h-local", None);
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut local.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Local;
    create(repo.as_ref(), local).await;
    create(
        repo.as_ref(),
        persisted("claim-i-nonterminal", true, "idle"),
    )
    .await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");
    for (rule, expected_id, expected_epoch) in [
        ("C4", "claim-b0-current-renewable", 5),
        ("C2", "claim-d-absent", 1),
        ("C3", "claim-e-expired", 10),
    ] {
        let assignment = application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect(rule)
            .expect(rule);
        assert_eq!(assignment.session_id, expected_id, "{rule}/E1");
        assert_eq!(
            assignment.authority.lease.epoch, expected_epoch,
            "{rule}/E1"
        );
        assert_eq!(assignment.authority.lease.owner, target.owner, "{rule}/E1");
        assert_eq!(
            assignment.authority.lease.runtime_incarnation, target.runtime_incarnation,
            "{rule}/E1"
        );
        assert_eq!(
            assignment.projection.baseline.agent_id, "agent",
            "{rule}/E1"
        );
    }
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("C1+C5-C7")
            .is_none(),
        "C1+C5-C7/E2+E3"
    );
    assert_eq!(
        repo.get(&SessionId::from("claim-a-current"))
            .await
            .unwrap()
            .realization,
        Some(lease("worker-a", "worker-a:2:boot-new", 4, u64::MAX)),
        "C5/E3 equal expiry performs zero root mutation"
    );
    assert_eq!(
        repo.get(&SessionId::from("claim-c-predecessor"))
            .await
            .unwrap()
            .realization,
        Some(lease("worker-a", "worker-a:1:boot-old", 7, u64::MAX)),
        "C1/E2 live predecessor affinity performs zero root mutation"
    );
    let foreign = repo
        .get(&SessionId::from("claim-f-expired-foreign"))
        .await
        .unwrap();
    assert_eq!(
        foreign.realization,
        Some(lease("worker-b", "worker-b:1:boot", 11, 1)),
        "C6/E2 foreign provider installation remains authoritative"
    );
    let invalid = application
        .claim_next_terminal_cleanup(
            awaken_session_realization_contract::SessionRealizationTarget {
                reassign_existing_lease: true,
                ..target.clone()
            },
        )
        .await;
    assert!(
        matches!(
            invalid,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "claim-next never doubles as live reassignment"
    );

    let inner: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("ambiguous cleanup Session repository"),
    );
    create(inner.as_ref(), requested("claim-conflict", None)).await;
    let faulting = Arc::new(FaultingSessionRepository::new(inner.clone()));
    faulting.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let application = application_with_configuration(
        faulting,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let workers = Arc::new(RecordingWorkerObservations::default());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install the one Worker observation authority");
    let replayed = application
        .claim_next_terminal_cleanup(target)
        .await
        .expect("C8 ambiguous CAS replay")
        .expect("C8 assignment");
    assert_eq!(replayed.session_id, "claim-conflict", "C8/E4");
    assert_eq!(
        replayed.authority.lease.epoch, 1,
        "C8/E4 does not allocate twice"
    );
    assert!(
        workers.list_calls.load(Ordering::SeqCst) >= 2,
        "C8 recomputes Worker eligibility after the root CAS conflict"
    );
}

fn registry_race_terminal_session(
    session_id: &str,
    realization: Option<awaken_session_resource_contract::SessionRealizationLease>,
) -> PersistedSession {
    let mut session = persisted(session_id, true, "terminated");
    assert!(
        session
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from(session_id))
    );
    session
        .terminal_cleanup
        .freeze_targets(&awaken_session_types::SessionId::from(session_id), [], 0, 0)
        .expect("freeze Registry-race cleanup target");
    session.realization = realization;
    session
}

fn registry_race_target() -> awaken_session_realization_contract::SessionRealizationTarget {
    awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker-registry-race".into(),
        runtime_incarnation: "worker-registry-race:2:boot-current".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    }
}

/// Terminal effect authorization cause/effect graph: C1 the asserted effect
/// lease expired more than 20 seconds ago; C2 the current Session root is the
/// same owner/incarnation/epoch and is either renewed/live or still expired;
/// C3 the asserted epoch is exact or a foreign successor. Effects: E1 an old
/// assertion may start cleanup preparation, terminal Memory, and physical
/// disposal only through its live same-generation current renewal; E2 an
/// unrenewed current root is stale before any new effect or Workspace/provider
/// authority is returned; E3 a foreign epoch remains stale even while the
/// current predecessor is live. Control's current root is the only lease clock;
/// receipt recording remains generation-based so an admitted long preparation
/// may finish after ordinary expiry and durably expose the later disposal.
///
/// | Rule | asserted | current root | Authorized effects |
/// |---|---|---|---|
/// | A1 | exact, expired >20s | same generation renewed/live | E1: preparation + Memory + disposal |
/// | A2 | exact, expired >20s | same generation expired | E2: none; no Workspace/provider authority |
/// | A3 | foreign epoch | predecessor renewed/live | E3: none |
#[tokio::test]
async fn terminal_effect_authorization_requires_current_live_same_generation() {
    let now =
        awaken_agent_contract::clock::Clock::now_ms(&awaken_agent_contract::clock::SystemClock);
    let asserted = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-terminal-authority".into(),
        runtime_incarnation: "worker-terminal-authority:2:boot-current".into(),
        epoch: 29,
        expires_at_unix_ms: now.saturating_sub(20_001),
    };
    let renewed = awaken_session_resource_contract::SessionRealizationLease {
        expires_at_unix_ms: now.saturating_add(30_000),
        ..asserted.clone()
    };
    let memory_input = awaken_session_resource_contract::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from("terminal-memory"),
        source: awaken_session_resource_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                "terminal-authority-memory",
            ),
            config: awaken_resource_contract::MemoryStoreConfigVersion {
                memory_store_id: awaken_resource_contract::MemoryStoreId::from(
                    "terminal-authority-memory",
                ),
                version: awaken_resource_contract::ConfigVersion::INITIAL,
                retention_policy: Default::default(),
            },
        },
        mount_path: "/memory/terminal-authority".into(),
        access: awaken_resource_contract::ResourceAccess::ReadWrite,
        instructions: None,
    };
    let materialization = awaken_provisioning_contract::MemoryMaterializationEvidence::new(
        "terminal-authority-memory",
        awaken_provisioning_contract::memory_materialization_mount_path(&memory_input.mount_path),
        Vec::new(),
    )
    .expect("terminal authority Memory evidence");
    let terminal_session = |session_id: &str,
                            realization: Option<
        awaken_session_resource_contract::SessionRealizationLease,
    >| {
        let mut session = registry_race_terminal_session(session_id, realization);
        session.resources = awaken_session_state_contract::SessionResourceState::from_active(
            awaken_session_resource_contract::ResolvedSessionResources::try_new(
                vec![memory_input.clone()],
                Vec::new(),
            )
            .expect("terminal authority Memory input"),
        );
        let realization_fingerprint =
            awaken_provisioning_contract::SandboxRealizationFingerprint::from_spec(
                &awaken_provisioning_contract::SandboxSpec {
                    process_tree_supervision: false,
                    scope: session_id.into(),
                    isolation: awaken_provisioning_contract::IsolationClass::Workdir,
                    environment: None,
                    command: Vec::new(),
                    deny_tool_egress: false,
                    mounts: Vec::new(),
                    env: Vec::new(),
                    packages: Default::default(),
                    network: awaken_provisioning_contract::NetworkPolicy::Unrestricted,
                    outputs_path: "/mnt/session/outputs".into(),
                    requests: Default::default(),
                    limits: Default::default(),
                    filesystem_continuity: Default::default(),
                    control_services: Default::default(),
                    lease_ttl_secs: None,
                },
            );
        let handle = awaken_provisioning_contract::test_support::fixture_current_handle(
            session_id,
            &awaken_provisioning_contract::test_support::FixtureHandleLocator {
                outputs_path: "/mnt/session/outputs".into(),
                physical_incarnation: format!("{session_id}:physical"),
            },
            &realization_fingerprint,
            &asserted
                .sandbox_effect_fence("create-terminal-authority-memory")
                .expect("terminal authority Memory creation fence"),
            &[
                awaken_provisioning_contract::memory_materialization_mount_path(
                    &memory_input.mount_path,
                ),
            ],
        )
        .with_memory_materializations(vec![materialization.clone()])
        .expect("terminal authority durable Memory evidence");
        session.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
            binding: serde_json::to_string(&handle)
                .expect("terminal authority Environment binding"),
            effect_id: None,
            generation: None,
            idle_since_unix_ms: None,
        };
        session
    };
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("terminal effect authority Session repository"),
    );
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-renewed", Some(renewed)),
    )
    .await;
    create(
        repo.as_ref(),
        terminal_session("terminal-authority-expired", Some(asserted.clone())),
    )
    .await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    let effect_for =
        |session_id: &str,
         work: awaken_session_realization_contract::SessionTerminalCleanupWork,
         lease: awaken_session_resource_contract::SessionRealizationLease| {
            awaken_session_state_contract::SessionTerminalCleanupEffect::new(
                terminal_prepare_commands(work)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| panic!("{session_id} root preparation command")),
                lease,
            )
        };
    let renewed_work = application
        .terminal_cleanup_work(&SessionId::from("terminal-authority-renewed"), &asserted)
        .await
        .expect("A1 exact generation")
        .expect("A1 pending work");
    let renewed_effect = effect_for("terminal-authority-renewed", renewed_work, asserted.clone());
    application
        .authorize_terminal_cleanup_effect(&renewed_effect)
        .await
        .expect("A1/E1 old assertion uses current renewal");
    let renewed_memory = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &renewed_effect,
    )
    .expect("A1 exact terminal Memory intent");
    application
        .authorize_terminal_memory_intent(&renewed_memory)
        .await
        .expect("A1/E1 old Memory assertion uses current renewal");
    let renewed_preparation = awaken_session_state_contract::SessionCleanupPreparation::try_new(
        &renewed_effect,
        renewed_effect
            .sandbox_effect_fence()
            .expect("A1 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A1 terminal preparation");
    application
        .record_terminal_cleanup_preparation(&asserted, renewed_preparation)
        .await
        .expect("A1 admitted preparation remains recordable");
    let renewed_disposal = match application
        .terminal_cleanup_work(&SessionId::from("terminal-authority-renewed"), &asserted)
        .await
        .expect("A1 exact disposal generation")
        .expect("A1 pending disposal")
        .action
    {
        awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A1 expected disposal action, got {action:?}"),
    };
    assert_eq!(
        application
            .authorize_terminal_cleanup_disposal(&renewed_disposal)
            .await
            .expect("A1/E1 old disposal assertion uses current renewal"),
        "workspace",
        "A1/E1 disposal Workspace authority"
    );

    // A2 an expired root cannot mint a fresh assignment time proof. Retained
    // commands still derive from the canonical durable action; constructing
    // that fixture is not runtime admission. Every effect gate below must
    // reject it, while an already-completed preparation may still be recorded.
    assert!(matches!(
        application
            .terminal_cleanup_work(&SessionId::from("terminal-authority-expired"), &asserted)
            .await,
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
    ));
    let expired_action = repo
        .get(&SessionId::from("terminal-authority-expired"))
        .await
        .unwrap()
        .terminal_cleanup_work_action()
        .unwrap()
        .unwrap();
    let awaken_session_state_contract::SessionTerminalCleanupAction::Prepare { commands } =
        expired_action
    else {
        panic!("A2 retained preparation command");
    };
    let expired_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        commands.into_iter().next().unwrap(),
        asserted.clone(),
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&expired_effect)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2"
    );
    let expired_memory = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &expired_effect,
    )
    .expect("A2 exact terminal Memory intent");
    assert!(
        matches!(
            application
                .authorize_terminal_memory_intent(&expired_memory)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal Memory"
    );
    let expired_preparation = awaken_session_state_contract::SessionCleanupPreparation::try_new(
        &expired_effect,
        expired_effect
            .sandbox_effect_fence()
            .expect("A2 terminal preparation fence"),
        Vec::new(),
    )
    .expect("A2 terminal preparation");
    application
        .record_terminal_cleanup_preparation(&asserted, expired_preparation)
        .await
        .expect("A2 admitted preparation receipt remains generation-based");
    assert!(matches!(
        application
            .terminal_cleanup_work(&SessionId::from("terminal-authority-expired"), &asserted)
            .await,
        Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
    ));
    let expired_disposal = match repo
        .get(&SessionId::from("terminal-authority-expired"))
        .await
        .unwrap()
        .terminal_cleanup_work_action()
        .unwrap()
        .unwrap()
    {
        awaken_session_state_contract::SessionTerminalCleanupAction::Dispose { command } => {
            awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
                command,
                asserted.clone(),
            )
        }
        action => panic!("A2 expected disposal action, got {action:?}"),
    };
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_disposal(&expired_disposal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A2/E2 terminal disposal returns no Workspace/provider authority"
    );

    let foreign_effect = awaken_session_state_contract::SessionTerminalCleanupEffect::new(
        renewed_effect.command.clone(),
        awaken_session_resource_contract::SessionRealizationLease {
            epoch: asserted.epoch + 1,
            ..asserted.clone()
        },
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_effect(&foreign_effect)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3"
    );
    let foreign_memory = awaken_session_state_contract::terminal_memory_reconciliation_intent(
        &memory_input,
        &materialization,
        &foreign_effect,
    )
    .expect("A3 foreign terminal Memory intent");
    assert!(
        matches!(
            application
                .authorize_terminal_memory_intent(&foreign_memory)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal Memory"
    );
    let foreign_disposal = awaken_session_state_contract::SessionTerminalCleanupDisposalEffect::new(
        renewed_disposal.command,
        foreign_effect.lease,
    );
    assert!(
        matches!(
            application
                .authorize_terminal_cleanup_disposal(&foreign_disposal)
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "A3/E3 terminal disposal"
    );
}

/// Registry/root-CAS race cause/effect graph: C1 the exact Worker incarnation
/// is valid before the recovery scan; C2 the canonical Worker observation at
/// the CAS boundary is exact, revoked, or unavailable; C3 the root CAS wins;
/// C4 the exact post-CAS readback remains valid or is revoked; C5 a concurrent
/// root mutation replaces the just-minted realization before compensation.
/// Effects: E1 only current C2 authority may enter root CAS; E2 an unavailable
/// C2 read returns unavailable with the prior root unchanged; E3 revoked C4
/// authority restores the exact prior realization; E4 compensation never
/// overwrites a concurrent successor. Both reads use the installed
/// `WorkerObservationSource`; Session root CAS remains the sole lease writer.
///
/// | Rule | C2 at CAS | C3 | C4 readback | C5 successor | Effect |
/// |---|---|---|---|---|---|
/// | R1 | revoked | no | - | no | E1; no assignment/root mutation |
/// | R2 | unavailable | no | - | no | E2 |
/// | R3a | exact | yes | revoked | no | E3 |
/// | R3b | exact | yes | revoked | yes | E4 |
/// | R3c | exact | ambiguous win | revoked on retry | no | E3 |
#[tokio::test]
async fn cold_terminal_cleanup_claim_revalidates_registry_around_root_cas() {
    let target = registry_race_target();
    let registered = || terminal_cleanup_worker(&target, terminal_cleanup_manifest());

    let r1_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R1 Session repository"),
    );
    create(
        r1_repo.as_ref(),
        registry_race_terminal_session("registry-race-r1", None),
    )
    .await;
    let r1_revision = r1_repo
        .get(&SessionId::from("registry-race-r1"))
        .await
        .unwrap()
        .revision;
    let r1_application = application_with_configuration(
        r1_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r1_workers = Arc::new(RecordingWorkerObservations::default());
    r1_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r1_application
        .set_worker_observation_source(r1_workers.clone())
        .expect("R1 Worker observation authority");
    assert!(
        r1_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R1 revoked Worker is a closed scan result")
            .is_none(),
        "R1/E1"
    );
    let r1_durable = r1_repo
        .get(&SessionId::from("registry-race-r1"))
        .await
        .unwrap();
    assert_eq!(r1_durable.realization, None, "R1/E1");
    assert_eq!(r1_durable.revision, r1_revision, "R1/E1 zero root CAS");
    assert_eq!(r1_workers.list_calls.load(Ordering::SeqCst), 2, "R1/E1");

    let r2_repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R2 Session repository"),
    );
    create(
        r2_repo.as_ref(),
        registry_race_terminal_session("registry-race-r2", None),
    )
    .await;
    let r2_revision = r2_repo
        .get(&SessionId::from("registry-race-r2"))
        .await
        .unwrap()
        .revision;
    let r2_application = application_with_configuration(
        r2_repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r2_workers = Arc::new(RecordingWorkerObservations::default());
    r2_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Fail,
    ]);
    r2_application
        .set_worker_observation_source(r2_workers.clone())
        .expect("R2 Worker observation authority");
    assert!(
        matches!(
            r2_application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "R2/E2"
    );
    let r2_durable = r2_repo
        .get(&SessionId::from("registry-race-r2"))
        .await
        .unwrap();
    assert_eq!(r2_durable.realization, None, "R2/E2");
    assert_eq!(r2_durable.revision, r2_revision, "R2/E2 zero root CAS");
    assert_eq!(r2_workers.list_calls.load(Ordering::SeqCst), 2, "R2/E2");

    let r3_prior = awaken_session_resource_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-registry-race:1:boot-prior".into(),
        epoch: 7,
        expires_at_unix_ms: 1,
    };
    let r3_inner: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3 Session repository"),
    );
    create(
        r3_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3", Some(r3_prior.clone())),
    )
    .await;
    let r3_repo = Arc::new(FaultingSessionRepository::new(r3_inner.clone()));
    let r3_application = application_with_configuration(
        r3_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3_workers = Arc::new(RecordingWorkerObservations::default());
    r3_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3_application
        .set_worker_observation_source(r3_workers.clone())
        .expect("R3 Worker observation authority");
    assert!(
        r3_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R3 revoked readback is compensated")
            .is_none(),
        "R3a/E3"
    );
    assert_eq!(
        r3_inner
            .get(&SessionId::from("registry-race-r3"))
            .await
            .unwrap()
            .realization,
        Some(r3_prior),
        "R3a/E3 restores the exact prior realization"
    );
    assert_eq!(r3_workers.list_calls.load(Ordering::SeqCst), 3, "R3a/E3");

    let r3b_inner: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3b Session repository"),
    );
    create(
        r3b_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3b", None),
    )
    .await;
    let r3b_successor = awaken_session_resource_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-registry-race:3:boot-successor".into(),
        epoch: 99,
        expires_at_unix_ms: u64::MAX,
    };
    let r3b_repo = Arc::new(FaultingSessionRepository::new(r3b_inner.clone()));
    r3b_repo.commit_concurrent_realization_then_conflict_once(
        "revoke-terminal-cleanup-recovery",
        r3b_successor.clone(),
    );
    let r3b_application = application_with_configuration(
        r3b_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3b_workers = Arc::new(RecordingWorkerObservations::default());
    r3b_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3b_application
        .set_worker_observation_source(r3b_workers)
        .expect("R3b Worker observation authority");
    assert!(
        r3b_application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("R3b concurrent successor resolves the revoked claim")
            .is_none(),
        "R3b/E4"
    );
    assert_eq!(
        r3b_inner
            .get(&SessionId::from("registry-race-r3b"))
            .await
            .unwrap()
            .realization,
        Some(r3b_successor),
        "R3b/E4 compensation cannot overwrite the concurrent root"
    );

    let r3c_inner: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("R3c Session repository"),
    );
    create(
        r3c_inner.as_ref(),
        registry_race_terminal_session("registry-race-r3c", None),
    )
    .await;
    let r3c_repo = Arc::new(FaultingSessionRepository::new(r3c_inner.clone()));
    r3c_repo.commit_then_conflict_once("claim-terminal-cleanup-recovery");
    let r3c_application = application_with_configuration(
        r3c_repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    let r3c_workers = Arc::new(RecordingWorkerObservations::default());
    r3c_workers.script([
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(vec![registered()]),
        WorkerObservationStep::Workers(Vec::new()),
    ]);
    r3c_application
        .set_worker_observation_source(r3c_workers)
        .expect("R3c Worker observation authority");
    assert!(
        r3c_application
            .claim_next_terminal_cleanup(registry_race_target())
            .await
            .expect("R3c ambiguous win is compensated after retry revocation")
            .is_none(),
        "R3c/E3"
    );
    assert_eq!(
        r3c_inner
            .get(&SessionId::from("registry-race-r3c"))
            .await
            .unwrap()
            .realization,
        None,
        "R3c/E3 ambiguous committed claim restores its prior realization"
    );
}

#[tokio::test]
async fn cold_terminal_cleanup_claim_requires_current_compatible_worker_before_root_cas() {
    // Worker-admission cause/effect graph: C1 the canonical registry source is
    // absent; C2 the exact current incarnation is registered; C3 its manifest
    // explicitly supports the current runtime protocol; C4 the frozen active Resource
    // generation requires Session Resources and the manifest supplies it; C5
    // an installed Repository has a credential binding and the manifest
    // supplies the Repository-credential capability; C6 an unowned Environment
    // has never been materialized; C7 a retained checkpoint format is
    // explicitly supported. E1 only the applicable C2-C7 facts admit
    // the root lease CAS; E2 every missing/stale/incompatible fact leaves the
    // Session root unchanged; E3 Draining/full-capacity Workers may release
    // existing resources because cleanup is not new Run capacity; E4 physical
    // Environment state without a historical owner fails closed.
    //
    // | Rule | source | protocol | resources | Environment | Effect |
    // |---|---|---|---|---|---|
    // | W1a | absent | - | - | unmaterialized | E2 unavailable |
    // | W1b | read failure | - | - | unmaterialized | E2 unavailable |
    // | W2 | stale incarnation | current | present | unmaterialized | E2 skip |
    // | W3 | Starting | current | present | unmaterialized | E2 skip |
    // | W4 | exact | ANY/zero-based/old/inverted/future | present | unmaterialized | E2 skip, unchanged root |
    // | W5 | exact | current | missing capability | unmaterialized | E2 skip |
    // | W6 | exact Draining/full | current | exact capability | unmaterialized | E1+E3 |
    // | W7 | exact | current, Repository credential missing/exact | credentialed | E2 then E1 |
    // | W8 | exact | current, checkpoint format missing | none | Hibernated/owned | E2 |
    // | W9 | exact | current, checkpoint format exact | none | Hibernated/owned | E1 |
    // | W10 | exact | current | exact capability | Resident/no owner | E2+E4 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("Worker-compatible cleanup Session repository"),
    );
    let target = awaken_session_realization_contract::SessionRealizationTarget {
        owner: "worker-compatible".into(),
        runtime_incarnation: "worker-compatible:3:boot".into(),
        lease_expires_at_unix_ms: u64::MAX,
        reassign_existing_lease: false,
    };
    let mut requested = persisted("claim-compatible", true, "terminated");
    requested.resources = awaken_session_state_contract::SessionResourceState::from_active(
        file_resources("claim-compatible-file"),
    );
    assert!(
        requested
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from("claim-compatible"))
    );
    requested
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from("claim-compatible"),
            [],
            0,
            0,
        )
        .expect("freeze compatible cleanup target");
    create(repo.as_ref(), requested).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "W1/E2"
    );
    assert!(
        repo.get(&SessionId::from("claim-compatible"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "W1/E2 zero root mutation"
    );

    let workers = Arc::new(RecordingWorkerObservations::default());
    let mut stale = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    stale.snapshot.identity =
        awaken_worker_contract::WorkerIdentity::new(target.owner.clone(), "stale-boot", 4);
    workers.replace(vec![stale]);
    application
        .set_worker_observation_source(workers.clone())
        .expect("install exact Worker observations");
    workers.fail.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            application
                .claim_next_terminal_cleanup(target.clone())
                .await,
            Err(
                awaken_session_realization_contract::SessionRealizationControlFailure::Unavailable(
                    _
                )
            )
        ),
        "W1b/E2"
    );
    workers.fail.store(false, Ordering::SeqCst);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W2 stale Worker is ineligible")
            .is_none(),
        "W2/E2"
    );

    let mut starting = terminal_cleanup_worker(&target, terminal_cleanup_manifest());
    starting.snapshot.state = awaken_worker_contract::WorkerState::Starting;
    workers.replace(vec![starting]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W3 unready Worker is ineligible")
            .is_none(),
        "W3/E2"
    );

    // W4 also includes v3 stage-scoped workers: the whole-claim protocol floor
    // applies equally to ordinary Run admission and terminal cleanup placement.
    for (min, max) in [
        (0, u32::MAX),
        (0, 3),
        (1, 1),
        (1, 2),
        (3, 2),
        (1, 3),
        (3, 3),
        (
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
            awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION + 1,
        ),
    ] {
        let mut manifest = terminal_cleanup_manifest();
        manifest.runtime_protocol.min = min;
        manifest.runtime_protocol.max = max;
        manifest
            .capabilities
            .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
        workers.replace(vec![terminal_cleanup_worker(&target, manifest)]);
        let before = repo
            .get(&SessionId::from("claim-compatible"))
            .await
            .unwrap();
        assert!(
            application
                .claim_next_terminal_cleanup(target.clone())
                .await
                .expect("W4 protocol-incompatible candidate")
                .is_none(),
            "{min}..{max}"
        );
        assert_eq!(
            repo.get(&SessionId::from("claim-compatible"))
                .await
                .unwrap(),
            before,
            "W4/E2"
        );
    }

    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W5 missing Resource capability is ineligible")
            .is_none(),
        "W5/E2"
    );

    let mut manifest = terminal_cleanup_manifest();
    manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    let mut worker = terminal_cleanup_worker(&target, manifest);
    worker.snapshot.state = awaken_worker_contract::WorkerState::Draining;
    worker.snapshot.in_flight = worker.snapshot.manifest.capacity.max_concurrent;
    workers.replace(vec![worker]);
    let assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W6 compatible Worker admission")
        .expect("W6 assignment");
    assert_eq!(assignment.session_id, "claim-compatible", "W6/E1");

    let credentialed_session_id = "claim-active-credential";
    let resources = credentialed_repository_resources("source", "repo-active");
    let mut credentialed = persisted(credentialed_session_id, true, "terminated");
    credentialed.resources =
        awaken_session_state_contract::SessionResourceState::from_active(resources);
    assert!(
        credentialed
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from(
                credentialed_session_id
            ))
    );
    credentialed
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from(credentialed_session_id),
            [],
            0,
            0,
        )
        .expect("freeze credentialed cleanup target");
    create(repo.as_ref(), credentialed).await;
    let mut resource_only_manifest = terminal_cleanup_manifest();
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest.clone(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W7 missing Repository credential capability is ineligible")
            .is_none(),
        "W7/E2"
    );
    resource_only_manifest
        .capabilities
        .insert(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        resource_only_manifest,
    )]);
    let credentialed_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W7 credential-compatible Worker admission")
        .expect("W7 assignment");
    assert_eq!(
        credentialed_assignment.session_id, credentialed_session_id,
        "W7/E1"
    );

    let checkpoint_session_id = "claim-checkpoint";
    let generation = awaken_session_state_contract::SandboxGeneration::new(
        &awaken_session_types::SessionId::from(checkpoint_session_id),
        1,
        u64::MAX,
        "env-7",
        "base-image",
    );
    let mut checkpoint = persisted(checkpoint_session_id, true, "terminated");
    assert!(
        checkpoint
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from(
                checkpoint_session_id
            ))
    );
    checkpoint
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from(checkpoint_session_id),
            [],
            0,
            0,
        )
        .expect("freeze checkpoint cleanup target");
    checkpoint.environment = awaken_session_state_contract::SessionEnvironmentState::Hibernated {
        checkpoint: awaken_session_state_contract::SandboxCheckpointRef {
            id: "checkpoint-object".into(),
            format: "portable-checkpoint-v1".into(),
            digest: "sha256:checkpoint".into(),
            size_bytes: 7,
            created_at_unix_ms: 1,
            expires_at_unix_ms: u64::MAX,
            environment_fingerprint: "env-7".into(),
            base_image_fingerprint: "base-image".into(),
            excluded_mounts: Vec::new(),
            suspend_effect_id: "suspend-effect".into(),
        },
        generation,
    };
    checkpoint.realization = Some(awaken_session_resource_contract::SessionRealizationLease {
        owner: target.owner.clone(),
        runtime_incarnation: "worker-compatible:2:old-boot".into(),
        epoch: 5,
        expires_at_unix_ms: 1,
    });
    create(repo.as_ref(), checkpoint).await;
    workers.replace(vec![terminal_cleanup_worker(
        &target,
        terminal_cleanup_manifest(),
    )]);
    assert!(
        application
            .claim_next_terminal_cleanup(target.clone())
            .await
            .expect("W8 unsupported checkpoint format is ineligible")
            .is_none(),
        "W8/E2"
    );
    let mut checkpoint_manifest = terminal_cleanup_manifest();
    checkpoint_manifest
        .checkpoint_formats
        .insert("portable-checkpoint-v1".into());
    workers.replace(vec![terminal_cleanup_worker(&target, checkpoint_manifest)]);
    let checkpoint_assignment = application
        .claim_next_terminal_cleanup(target.clone())
        .await
        .expect("W9 supported checkpoint admission")
        .expect("W9 assignment");
    assert_eq!(
        checkpoint_assignment.session_id, checkpoint_session_id,
        "W9/E1"
    );

    let mut unowned = persisted("claim-unowned-resident", true, "terminated");
    assert!(
        unowned
            .terminal_cleanup
            .request(&awaken_session_types::SessionId::from(
                "claim-unowned-resident"
            ))
    );
    unowned
        .terminal_cleanup
        .freeze_targets(
            &awaken_session_types::SessionId::from("claim-unowned-resident"),
            [],
            0,
            0,
        )
        .expect("freeze unowned cleanup target");
    unowned.environment = awaken_session_state_contract::SessionEnvironmentState::Resident {
        binding: "physical-without-owner".into(),
        effect_id: None,
        generation: None,
        idle_since_unix_ms: None,
    };
    create(repo.as_ref(), unowned).await;
    assert!(
        matches!(
            application.claim_next_terminal_cleanup(target).await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_))
        ),
        "W10/E2+E4"
    );
    assert!(
        repo.get(&SessionId::from("claim-unowned-resident"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "W10/E2 zero root mutation"
    );
}

#[tokio::test]
async fn local_terminal_cleanup_claim_reads_the_exact_root_beyond_the_recovery_batch() {
    // Exact-local claim cause/effect graph: C1 the requested local Session is
    // inside or beyond the repository's bounded recovery page; C2 earlier
    // terminal rows remain pending; C3 a lease is exact-current or live under
    // another process incarnation. E1 the named root alone is read, fenced,
    // and returned; E2 no earlier row is claimed or allowed to starve it; E3
    // exact response-loss replay returns the same lease; E4 a live predecessor
    // is never preempted before expiry; E5 the co-located owner renews that
    // exact generation without requiring an external Worker Registry record.
    //
    // | Rule | target position | earlier pending rows | Effect |
    // |---|---|---|---|
    // | X1 | first page | any | E1 |
    // | X2 | after 256-row page | 257 | E1 + E2 |
    // | X3 | exact-current | any | E3 |
    // | X4 | live other incarnation | any | E4 |
    // | X5 | exact-current local renewal | any | E5 |
    //
    // X2 is the regression rule: local execution must never reuse the external
    // claim-next scan as an approximate lookup for a known Session id. X1-X4
    // also require the internal claim-result representation to return the
    // complete assignment unchanged; indirection cannot alter any lease fact.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("exact local terminal Session repository"),
    );
    let requested_local = |id: &str| {
        let mut session = persisted(id, true, "terminated");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut session.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = SessionRuntimePlacement::Local;
        assert!(
            session
                .terminal_cleanup
                .request(&awaken_session_types::SessionId::from(id))
        );
        session
            .terminal_cleanup
            .freeze_targets(&awaken_session_types::SessionId::from(id), [], 0, 0)
            .expect("freeze local cleanup target");
        session
    };
    for index in 0..257 {
        let id = format!("batch-prefix-{index:03}");
        create(repo.as_ref(), requested_local(&id)).await;
    }
    let target_id = "zz-exact-local-terminal";
    create(repo.as_ref(), requested_local(target_id)).await;

    let application = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: "embedded-terminal-worker".into(),
            ..Default::default()
        },
    );
    let assignment = application
        .claim_local_terminal_cleanup_assignment(&SessionId::from(target_id))
        .await
        .expect("X2 exact local assignment");
    assert_eq!(assignment.session_id, target_id, "X2/E1");
    assert_eq!(assignment.authority.lease.epoch, 1, "X2/E1");
    assert_eq!(
        assignment.authority.lease.owner, "embedded-terminal-worker",
        "X2/E1"
    );
    let replay = application
        .claim_local_terminal_cleanup_assignment(&SessionId::from(target_id))
        .await
        .expect("X3 exact replay");
    assert_eq!(replay.authority.lease, assignment.authority.lease, "X3/E3");
    let requested_expiry_unix_ms = assignment
        .authority
        .lease
        .expires_at_unix_ms
        .saturating_add(crate::realization::LOCAL_SESSION_REALIZATION_LEASE_MS);
    let renewed = application
        .renew_session_realization(
            awaken_session_realization_contract::RenewSessionRealization {
                session_id: target_id.into(),
                asserted_lease: assignment.authority.lease.clone(),
                requested_expires_at_unix_ms: requested_expiry_unix_ms,
            },
        )
        .await
        .expect("X5 exact local generation renewal");
    assert_eq!(
        renewed.lease.owner, assignment.authority.lease.owner,
        "X5/E5"
    );
    assert_eq!(
        renewed.lease.runtime_incarnation, assignment.authority.lease.runtime_incarnation,
        "X5/E5"
    );
    assert_eq!(
        renewed.lease.epoch, assignment.authority.lease.epoch,
        "X5/E5"
    );
    assert_eq!(
        renewed.lease.expires_at_unix_ms, requested_expiry_unix_ms,
        "X5/E5"
    );
    let replacement = application_with_configuration(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            local_realization_owner: "embedded-terminal-worker".into(),
            ..Default::default()
        },
    );
    assert!(
        matches!(
            replacement
                .claim_local_terminal_cleanup_assignment(&SessionId::from(target_id))
                .await,
            Err(awaken_session_realization_contract::SessionRealizationControlFailure::StaleOwnership)
        ),
        "X4/E4"
    );
    assert!(
        repo.get(&SessionId::from("batch-prefix-000"))
            .await
            .unwrap()
            .realization
            .is_none(),
        "X2/E2"
    );
}

#[tokio::test]
async fn recovery_treats_a_concurrently_deleted_terminal_session_as_converged() {
    let inner: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        inner.as_ref(),
        persisted("deleted-during-scan", false, "terminated"),
    )
    .await;
    let repository = Arc::new(FaultingSessionRepository::new(inner));
    let application = application(
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    // The recovery scan already holds the terminal candidate when an eager
    // cleanup actor wins and removes the aggregate before the re-read.
    repository.get_not_found_once();
    let report = application.reconcile_resource_activations().await;

    assert!(
        report.failures.is_empty(),
        "stale scan is not a retryable outage"
    );
}

/// No-publication crash-window graph. C1 the terminal fence carries no
/// Repository publication intent; C2 preparation fails before its receipt;
/// C3 recovery/replay observes Requested or Completed. Effects: E1 the durable
/// target stays Requested until preparation is accepted; E2 no publication
/// command/effect is invented; E3 recovery reuses the exact cleanup effect id,
/// then performs one aggregate disposal; E4 Completed is absorbing and only
/// replays the aggregate-level process-local acknowledgement.
///
/// | Rule | publication intent | cleanup phase | Effect |
/// |---|---|---|---|
/// | L1 | absent | first preparation fails | E1 + E2, Requested retained |
/// | L2 | absent | retry succeeds | E3, preparation ack once |
/// | L3 | absent | Completed replay | E2 + E4, no preparation replay |
#[tokio::test]
async fn terminal_cleanup_recovery_reuses_intent_and_skips_completed_effects() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("cleanup-recovery", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.fail_once.store(true, Ordering::SeqCst);
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect_err("R1 injected crash leaves cleanup pending");
    let pending = repo
        .get(&SessionId::from("cleanup-recovery"))
        .await
        .expect("R1 durable intent");
    assert!(pending.terminal_cleanup.is_requested(), "R1 intent");
    assert_eq!(
        pending.environment.binding(),
        Some("opaque-environment"),
        "R1 Environment remains authoritative until receipt commit"
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R2 recovery")
        .expect("R2 retained Session");
    assert!(completed.terminal_cleanup.is_completed(), "R2 receipt");
    assert!(
        matches!(
            completed.environment,
            awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
        ),
        "R2 Environment projection removed with receipt"
    );
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "R2 acknowledges preparation only after its durable root CAS"
    );
    assert_eq!(runtime.acknowledged_completed.lock().unwrap().len(), 1);

    application
        .release_terminal_resources("workspace", "cleanup-recovery")
        .await
        .expect("R3 completed replay");
    let intents = runtime.intents.lock().unwrap();
    assert_eq!(intents.len(), 2, "R3 performs no effect");
    assert_eq!(
        intents[0].effect_id, intents[1].effect_id,
        "R1/R2 stable intent"
    );
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "R1/R2");
    assert!(
        runtime.publication_intents.lock().unwrap().is_empty(),
        "L1-L3/E2 legacy cleanup never invents publication"
    );
    let acknowledgements = runtime.acknowledged_effects.lock().unwrap();
    assert_eq!(
        acknowledgements.len(),
        1,
        "R3 does not replay preparation acknowledgement"
    );
    let completed_acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(
        completed_acknowledgements.len(),
        2,
        "R3 replays only the aggregate completion acknowledgement"
    );
    assert_eq!(
        completed_acknowledgements[0], completed_acknowledgements[1],
        "R3 completion acknowledgement retains the exact lease generation"
    );
}

/// A competing cleanup worker may win any root CAS after performing the exact
/// durable phase. The loser must re-read that phase and continue; returning the
/// conflict would leave a hidden Session and its Environment indefinitely live.
#[tokio::test]
async fn terminal_cleanup_rebases_after_competing_root_writer() {
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-root-race", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;
    repo.commit_then_conflict_once("terminal-cleanup-fence");

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-root-race")
        .await
        .expect("CAS loser resumes from the winning durable phase")
        .expect("archived Session remains");
    assert!(completed.terminal_cleanup.is_completed());
    assert!(matches!(
        completed.environment,
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
    ));
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1);
    assert!(
        repo.scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
}

/// Completion-response-loss design. Causes: C1 the exact terminal effect and
/// receipt succeed; C2 the `terminal-cleanup-disposal` CAS commits; C3 its
/// response is reported as a conflict; C4 retry reads the aggregate's Completed
/// target set and exact realization lease. Effects: E1 physical cleanup runs
/// once; E2 retry performs no second provider effect; E3 the same command+lease
/// is acknowledged from durable truth; E4 the caller observes completion.
/// Constraint: acknowledgement is process-local retirement, never a receipt or
/// a substitute cleanup authority.
///
/// | Rule | C1 | C2 | C3 | C4 | Effect |
/// |---|---|---|---|---|---|
/// | A1 | T | T | T | T | E1 + E2 + E3 + E4 |
#[tokio::test]
async fn terminal_cleanup_acknowledges_a_committed_completion_after_response_loss() {
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-completion-response-loss", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;
    repo.commit_then_conflict_once("terminal-cleanup-disposal");

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo,
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let completed = application
        .release_terminal_resources("workspace", "cleanup-completion-response-loss")
        .await
        .expect("A1 response-loss retry reads the committed completion")
        .expect("A1 archived Session remains");
    assert!(completed.terminal_cleanup.is_completed(), "A1/E4");
    assert_eq!(runtime.intents.lock().unwrap().len(), 1, "A1/E1/E2");
    let assignments = runtime.terminal_assignments.lock().unwrap();
    assert_eq!(
        assignments.len(),
        2,
        "A1 installs Prepare and Dispose under one terminal generation"
    );
    assert_eq!(
        assignments[0].authority.lease, assignments[1].authority.lease,
        "A1 one generation"
    );
    let acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(acknowledgements.len(), 1, "A1/E3");
    assert_eq!(
        acknowledgements[0].1, assignments[0].authority.lease,
        "A1/E3 reuses the durable terminal generation"
    );
    assert_eq!(
        acknowledgements[0].0, "cleanup-completion-response-loss",
        "A1/E3 reuses the durable target"
    );
}

/// Delete cleanup has two legitimate triggers but one cleanup kernel. Causes:
/// C1 the eager actor and lifecycle actor share a hidden Session; C2 the winner
/// commits the final cleanup receipt and tombstone while the loser is at its
/// next root CAS; C3 the loser observes repository NotFound rather than a CAS
/// Conflict. Effects: E1 the winner's tombstone remains authoritative; E2 the
/// loser returns the existing `None` completion receipt; E3 no duplicate
/// Runtime effect or recoverable-cleanup warning is produced. Decision rule
/// T1=C1+C2+C3 => E1+E2+E3. A NotFound before the first read follows the same
/// terminal no-op; other mutation failures remain errors in the crash matrix.
#[tokio::test]
async fn terminal_cleanup_stale_loser_accepts_the_winners_tombstone() {
    // Constraint/Invariant: the winner's repository tombstone is final; a stale
    // loser may acknowledge it but cannot replay Runtime cleanup.
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    create(
        repo.as_ref(),
        persisted("cleanup-tombstone-race", false, "idle"),
    )
    .await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );
    let transition = application
        .commit_delete_intent(SessionDeleteCommand::new("cleanup-tombstone-race"))
        .await
        .expect("delete fence");
    repo.tombstone_after_operation_once("resource-release-complete");

    let loser = application
        .release_terminal_resources(&transition.owner_scope, "cleanup-tombstone-race")
        .await
        .expect("T1/E2 concurrent winner is terminal success");

    assert!(loser.is_none(), "T1/E2 tombstone is the completion receipt");
    assert!(matches!(
        repo.get(&SessionId::from("cleanup-tombstone-race")).await,
        Err(awaken_session_state_contract::SessionRepositoryError::NotFound)
    ));
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "T1/E3");
}

/// Complete durable terminal-cleanup crash matrix. Causes are the process stop
/// location relative to C1=fence CAS, C2=quiesce, C3=target-intent CAS,
/// C4=idempotent preparation, C5=preparation CAS plus physical disposal, and
/// C6=disposal-receipt CAS.
/// Effects are E1 no pre-intent I/O, E2 recover from the last committed phase,
/// E3 reuse one effect identity, E4 keep Environment authority until completion,
/// E5 replay Completed without another provider effect, and E6 retire the
/// process-local preparation/disposal projections only after their respective
/// CAS is durably observable.
///
/// | Rule | Crash point | Durable phase | Recovery effect |
/// |---|---|---|---|
/// | F0 | before C1 | NotRequested | E1; normal fence begins later |
/// | F1 | after C1/before C2 | Fenced | E2 quiesces before targets |
/// | F2 | after C2/before C3 | Fenced | E2 repeats quiesce, freezes once |
/// | F3 | after C3/before C4 | Requested | E2 executes exact frozen targets |
/// | F4 | preparation response lost | Requested | E3 idempotent preparation replay |
/// | F5 | disposal receipt CAS unavailable | Disposing | E4 retained; preparation acked; disposal replayable |
/// | F6 | disposal CAS accepted | Completed | E5 no preparation replay + E6 |
#[tokio::test]
async fn terminal_cleanup_f0_through_f6_recover_from_durable_phase() {
    let durable: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let repo = Arc::new(FaultingSessionRepository::new(durable));
    let mut session = persisted("cleanup-f0-f6", false, "terminated");
    session.environment.set_resident("opaque-environment");
    create(repo.as_ref(), session).await;

    let runtime = Arc::new(RecordingCleanupRuntime::default());
    let application = SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    );

    let f0 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F0 truth");
    assert!(!f0.terminal_cleanup.needs_reconciliation(), "F0/E1");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F0/E1");

    runtime.fail_quiesce_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F1 crash before quiescence");
    let f1 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F1 truth");
    assert!(f1.terminal_cleanup.is_fenced(), "F1/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F1/E1");

    repo.fail_once("resource-release-intent");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F2 target-intent CAS outage after quiescence");
    let f2 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F2 truth");
    assert!(f2.terminal_cleanup.is_fenced(), "F2/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F2/E1");

    runtime
        .fail_before_effect_once
        .store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F3 crash after target freeze before effect");
    let f3 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F3 truth");
    assert!(f3.terminal_cleanup.is_requested(), "F3/E2");
    assert!(runtime.intents.lock().unwrap().is_empty(), "F3/E1");
    assert!(runtime.effective_ids.lock().unwrap().is_empty(), "F3/E1");

    runtime.fail_once.store(true, Ordering::SeqCst);
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F4 lost effect receipt");
    let f4 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F4 truth");
    assert!(f4.terminal_cleanup.is_requested(), "F4/E2");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F4/E3");
    assert_eq!(
        f4.environment.binding(),
        Some("opaque-environment"),
        "F4/E4"
    );

    repo.fail_once("terminal-cleanup-disposal");
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect_err("F5 completion CAS outage");
    let f5 = repo
        .get(&SessionId::from("cleanup-f0-f6"))
        .await
        .expect("F5 truth");
    assert!(
        f5.terminal_cleanup.is_requested(),
        "F5/E2 Disposing wraps Requested"
    );
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F5/E3");
    assert_eq!(
        f5.environment.binding(),
        Some("opaque-environment"),
        "F5/E4"
    );
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "F5/E6 preparation acknowledgement follows its own durable CAS"
    );
    let assignment_projections_after_f5 = runtime.terminal_assignments.lock().unwrap().len();

    let f6 = application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 recovery")
        .expect("archived Session retained");
    assert!(f6.terminal_cleanup.is_completed(), "F6/E5");
    assert!(matches!(
        f6.environment,
        awaken_session_state_contract::SessionEnvironmentState::Unmaterialized
    ));
    assert_eq!(
        runtime.acknowledged_effects.lock().unwrap().len(),
        1,
        "F6/E6 disposal recovery does not replay preparation acknowledgement"
    );
    assert_eq!(
        runtime.terminal_assignments.lock().unwrap().len(),
        assignment_projections_after_f5 + 1,
        "F6/E6 retries only the durable Disposing action"
    );
    assert_eq!(runtime.acknowledged_completed.lock().unwrap().len(), 1);
    let attempts = runtime.intents.lock().unwrap().len();
    application
        .release_terminal_resources("workspace", "cleanup-f0-f6")
        .await
        .expect("F6 response-loss replay");
    assert_eq!(runtime.intents.lock().unwrap().len(), attempts, "F6/E5");
    assert_eq!(runtime.effective_ids.lock().unwrap().len(), 1, "F6/E5");
    let acknowledgements = runtime.acknowledged_completed.lock().unwrap();
    assert_eq!(
        acknowledgements.len(),
        2,
        "F6/E5 aggregate completion acknowledgement is replay-safe"
    );
    assert_eq!(
        acknowledgements[0], acknowledgements[1],
        "F6/E5 exact replay"
    );
}

/// Deterministic archive/child race: the cleanup worker is paused after the
/// durable terminal fence but before its one Runtime snapshot. Both a legacy
/// synchronous delegated Run and an asynchronous coordinated Thread committed
/// in that interval must be included in the same frozen target set and can
/// never be lost behind a premature Completed marker.
#[tokio::test]
async fn terminal_fence_quiesces_before_freezing_concurrent_child() {
    // Test design. Causes: C1 terminal cleanup has committed its fence and pauses
    // before snapshot; C2 synchronous and coordinated children arrive in that
    // interval. Effects: E1 quiescence includes both in one frozen target set; E2
    // Completed cannot precede their cleanup. Constraint/Invariant: terminal fence
    // closes admission before target freeze. Decision rule: race C1+C2 and require
    // both children in E1 before completion.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repo.as_ref(),
        persisted("cleanup-concurrent-child", false, "terminated"),
    )
    .await;
    let runtime = Arc::new(RecordingCleanupRuntime::default());
    runtime.block_quiesce.store(true, Ordering::SeqCst);
    let application = Arc::new(SessionApplication::new_with_configuration(
        runtime.clone(),
        Arc::new(NoopMcpRealizer),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
        SessionApplicationConfiguration::default(),
    ));

    let cleanup = {
        let application = application.clone();
        tokio::spawn(async move {
            application
                .release_terminal_resources("workspace", "cleanup-concurrent-child")
                .await
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        runtime.quiesce_entered.notified(),
    )
    .await
    .expect("timed out waiting for runtime.quiesce_entered.notified()");
    let fenced = repo
        .get(&SessionId::from("cleanup-concurrent-child"))
        .await
        .expect("terminal fence committed before quiescence");
    assert!(fenced.terminal_cleanup.is_fenced());

    *runtime.delegated_snapshot.lock().unwrap() =
        awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: vec![awaken_session_application_contract::DelegatedRun {
                run_id: awaken_agent_contract::agent::run::Id("child-after-fence".into()),
                parent_call_id: "delegate-call".into(),
                agent_id: "child-agent".into(),
                status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
            }],
            coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
                "coordinated-after-fence".into(),
            )],
            watermark: 23,
            runtime_commit_cursor: 29,
        };
    runtime.quiesce_release.notify_one();

    let completed = cleanup
        .await
        .expect("cleanup task")
        .expect("cleanup succeeds")
        .expect("archived Session remains");
    assert!(
        completed.terminal_cleanup.is_completed(),
        "terminal cleanup must be completed"
    );
    assert_eq!(completed.terminal_cleanup.delegation_watermark(), Some(23));
    let thread_ids = completed
        .terminal_cleanup
        .thread_ids()
        .cloned()
        .expect("completed cleanup retains frozen targets");
    assert_eq!(
        thread_ids,
        BTreeSet::from([
            "cleanup-concurrent-child".to_string(),
            "child-after-fence".to_string(),
            "coordinated-after-fence".to_string(),
        ])
    );
    let cleaned = runtime
        .intents
        .lock()
        .unwrap()
        .iter()
        .map(|intent| intent.thread_id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(cleaned, thread_ids);
}

#[tokio::test]
async fn terminal_cleanup_restart_soak_preserves_authority_and_effect_identity() {
    // Constraint/Invariant: the authoritative Session inputs and repository CAS
    // documented here remain the only decision source; no parallel ledger is admitted.
    // Decision rule: execute every reachable cause partition documented here and
    // require its stated effects, including each fail-closed outcome.
    /* Soak cause/effect model. For each of 64 independent Sessions: C1 a
     * synchronous delegated Run and an asynchronous coordinated Thread exist in
     * the one Runtime snapshot; C2 terminal cleanup completes; C3 the
     * Coordinator/Store is reopened and the command is replayed. Effects: E1
     * root+both child kinds are frozen exactly once in the existing cleanup
     * operation; E2 one stable effective cleanup per thread; E3 restart replay
     * performs no new I/O; E4 no pending/quarantined Session authority leaks.
     * The blocked-quiescence test owns the concurrent interleaving oracle; this
     * loop owns repetition and restart leakage.
     *
     * | Rule | Sync child | Async Thread | Restart | Effects |
     * | S1 | yes | yes | no | E1+E2 |
     * | S2 | frozen | frozen | yes | E3+E4 | */
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cleanup-soak.db");
    let path = path.to_string_lossy().to_string();
    for iteration in 0..64 {
        let session_id = format!("cleanup-soak-{iteration}");
        let child_id = format!("cleanup-soak-child-{iteration}");
        let coordinated_id = format!("cleanup-soak-thread-{iteration}");
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("open Session authority"),
        );
        create(repo.as_ref(), persisted(&session_id, false, "terminated")).await;
        let runtime = Arc::new(RecordingCleanupRuntime::default());
        *runtime.delegated_snapshot.lock().unwrap() =
            awaken_session_application_contract::DelegatedRunSnapshot {
                delegated_runs: vec![awaken_session_application_contract::DelegatedRun {
                    run_id: awaken_agent_contract::agent::run::Id(child_id.clone()),
                    parent_call_id: format!("delegate-{iteration}"),
                    agent_id: "child-agent".into(),
                    status: awaken_agent_contract::agent::delegation::DelegationStatus::Open,
                }],
                coordinated_thread_ids: vec![awaken_agent_contract::agent::thread::Id(
                    coordinated_id.clone(),
                )],
                watermark: iteration + 1,
                runtime_commit_cursor: iteration + 11,
            };
        let application = SessionApplication::new_with_configuration(
            runtime.clone(),
            Arc::new(NoopMcpRealizer),
            repo,
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        let completed = application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completed.terminal_cleanup.thread_ids().unwrap(),
            &BTreeSet::from([session_id.clone(), child_id, coordinated_id]),
            "E1 iteration {iteration}",
        );
        assert_eq!(runtime.effective_ids.lock().unwrap().len(), 3, "E2");

        drop(application);
        let reopened = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(&path)
                .expect("restart Session authority"),
        );
        let replay_runtime = Arc::new(RecordingCleanupRuntime::default());
        let replay_application = SessionApplication::new_with_configuration(
            replay_runtime.clone(),
            Arc::new(NoopMcpRealizer),
            reopened.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
            SessionApplicationConfiguration::default(),
        );
        replay_application
            .release_terminal_resources("workspace", &session_id)
            .await
            .unwrap();
        assert!(replay_runtime.intents.lock().unwrap().is_empty(), "E3");
        let scan = reopened
            .scan(awaken_session_state_contract::SessionScanQuery::Reconcilable { after: None })
            .await
            .unwrap();
        assert!(scan.quarantined.is_empty(), "E4");
        assert!(
            scan.sessions
                .iter()
                .all(|row| row.session.session_id != session_id),
            "E4",
        );
    }
}

/// Cause/effect graph: C1 a baseline is frozen; C2 its explicit Runtime
/// placement is Local or Worker; C3 the process startup is local or registered.
/// The realization lease is intentionally absent from the causes: it is an
/// assignment fence, never placement policy. Effects are E1 local physical
/// realization or E2 dispatch-only Coordinator projection.
///
/// | Rule | Frozen placement | Process placement | Effect |
/// |---|---|---|---|
/// | P1 | preparing | any | E1 (not yet realizable) |
/// | P2 | local | local/registered | E1 |
/// | P3 | worker | local/registered | E2 |
/// | P4 | legacy | local | E1 |
/// | P5 | legacy | registered | E2 |
///
/// P4/P5 are the one-way upgrade interpretation for rows serialized before
/// placement existed. New creation is separately asserted never to emit the
/// legacy value.
#[test]
fn realization_owner_follows_the_frozen_placement_decision_table() {
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let local = application_with_configuration(
        repo.clone(),
        environments.clone(),
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::LocalWorker,
            ..Default::default()
        },
    );
    let registered = application_with_configuration(
        repo,
        environments,
        SessionApplicationConfiguration {
            execution_placement: SessionExecutionPlacement::RegisteredWorker,
            ..Default::default()
        },
    );

    let frozen = |placement: SessionRuntimePlacement| {
        let mut value = persisted("placement", false, "idle");
        let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
            &mut value.baseline
        else {
            unreachable!("fixture is frozen")
        };
        baseline.runtime_placement = placement;
        value
    };
    let preparing = {
        let mut value = frozen(SessionRuntimePlacement::Local);
        let baseline = value.frozen_baseline().expect("frozen").clone();
        value.baseline = awaken_session_configuration_contract::SessionBaselineState::Preparing(
            awaken_session_configuration_contract::SessionCreationIntent {
                control: awaken_session_configuration_contract::ControlSessionCreationInputs {
                    mutation_policy:
                        awaken_session_resource_contract::SessionMutationPolicy::Managed,
                    environment: baseline.environment,
                    runtime_placement: SessionRuntimePlacement::Local,
                    agent_id: baseline.agent_id,
                    agent_revision: baseline.agent_revision,
                    model_override: baseline.model_override,
                    system_prompt: *baseline.system_prompt,
                    model: baseline.model,
                    execution_model_ref: baseline.execution_model_ref,
                    runtime: baseline.runtime,
                    mcp_authoring: baseline.mcp_authoring,
                    delegate_ids: baseline.delegate_ids,
                    toolsets: baseline.toolsets,
                    mounts: baseline.mounts,
                    env: baseline.env,
                    prompts: baseline.prompts,
                    transcript_prefix: baseline.transcript_prefix,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            },
        );
        value
    };
    for (rule, value, local_expected, registered_expected) in [
        ("P1", preparing, false, false),
        ("P2", frozen(SessionRuntimePlacement::Local), false, false),
        ("P3", frozen(SessionRuntimePlacement::Worker), true, true),
    ] {
        assert_eq!(
            local.requires_external_realization(&value),
            local_expected,
            "{rule}/local"
        );
        assert_eq!(
            registered.requires_external_realization(&value),
            registered_expected,
            "{rule}/registered"
        );
    }
    assert_eq!(local.runtime_placement(), SessionRuntimePlacement::Local);
    assert_eq!(
        registered.runtime_placement(),
        SessionRuntimePlacement::Worker
    );
}

pub(super) fn publication_fixture(
    id: &str,
) -> (
    PersistedSession,
    awaken_session_resource_contract::SessionRealizationLease,
    awaken_session_mcp_contract::McpGenerationRef,
) {
    let mut session = persisted(id, false, "activating");
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-1".into(),
        epoch: 7,
        expires_at_unix_ms: (u64::MAX - 1_024),
    };
    let attachment_id = awaken_session_mcp_contract::McpAttachmentId("browser".into());
    let target =
        awaken_session_mcp_contract::McpTarget::parse_http("https://browser.example.test/mcp")
            .expect("MCP target");
    let generation = awaken_session_mcp_contract::McpGeneration::new(
        attachment_id,
        1,
        awaken_session_mcp_contract::McpAttachmentContentDigest::of_content(
            &target,
            false,
            None,
            awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
        ),
    );
    session.realization = Some(lease.clone());
    session
        .mcp
        .attachments
        .push(awaken_session_mcp_contract::SessionMcpAttachment {
            attachment_id: generation.attachment_id().clone(),
            name: "browser".into(),
            generation: generation.clone(),
            target,
            prompts_as_skills: false,
            origin: awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
            credential: None,
            selected_plaintext_holder: None,
            state: awaken_session_mcp_contract::McpAttachmentState::Active,
            publication_acknowledged: false,
            realization: Some(awaken_session_mcp_contract::McpRealizationClaim {
                realization_id: "realize-browser-1".into(),
                runtime_incarnation: lease.runtime_incarnation.clone(),
                lease_epoch: lease.epoch,
                lease_expires_at_unix_ms: (u64::MAX - 1_024),
                stage_idempotency_key: "renew-browser-1".into(),
            }),
            attempts: 1,
            last_error: None,
        });
    let generation_ref = awaken_session_mcp_contract::McpGenerationRef {
        session_id: id.into(),
        generation,
        runtime_incarnation: lease.runtime_incarnation.clone(),
        lease_epoch: lease.epoch,
        lease_expires_at_unix_ms: (u64::MAX - 1_024),
    };
    (session, lease, generation_ref)
}

#[tokio::test]
async fn publication_acknowledgement_follows_the_renewal_decision_table() {
    // Cause/effect graph: C1 acknowledgement names the exact active
    // generation; C2 it names the same owner/incarnation/epoch under a
    // shorter expiry; C3 any immutable generation coordinate differs.
    // Effects: E1 acknowledge and make the Session idle; E2 acknowledge the
    // completed effect under current live authority without restaging; E3 fail closed
    // without changing publication state.
    //
    // | Rule | exact | monotonic predecessor | replacement | Effect |
    // |---|---|---|---|---|
    // | A1 | yes | no | no | E1 |
    // | A2 | no | yes | no | E2 |
    // | A3 | no | no | yes | E3 |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let application = application(repo.clone(), environments);
    let current_expiry = u64::MAX - 1_024;

    let fixture = publication_fixture;

    // A7: cardinality 0/1/many x no predecessor/old unacknowledged Active x
    // exact/renewed lease. With zero attachments, predecessor=true is
    // unreachable and is omitted instead of counting duplicate coverage.
    // Actual lease expiries accompany the rule ID so relabeling cannot fake renewal.
    // Stage returns both generations; activation retires
    // old authority, so only new generations may appear in Publish. Recreate
    // the application between activation and ACK, then replay the ACK: the
    // durable publication/drain sets and receipt times must remain unchanged.
    let mut publication_traces = Vec::new();
    for count in 0..=2 {
        for predecessor in [false, true] {
            if count == 0 && predecessor {
                continue;
            }
            for renewed in [false, true] {
                let id = format!("publish-set-{count}-{predecessor}-{renewed}");
                let (mut session, lease, _) = fixture(&id);
                let template = session.mcp.attachments[0].clone();
                session.mcp.attachments.clear();
                session.mcp.desired_names = Some((0..count).map(|i| format!("tool-{i}")).collect());
                for index in 0..count {
                    let mut next = template.clone();
                    next.attachment_id =
                        awaken_session_mcp_contract::McpAttachmentId(format!("tool-{index}"));
                    // The witness carries the attachment identity, so renaming
                    // the row must re-mint it or the two would disagree.
                    next.generation = awaken_session_mcp_contract::McpGeneration::new(
                        next.attachment_id.clone(),
                        next.generation.version(),
                        next.generation.digest().clone(),
                    );
                    next.name = format!("tool-{index}");
                    if predecessor {
                        session.mcp.attachments.push(next.clone());
                    }
                    next.generation = next.generation.at_version(2);
                    next.state = awaken_session_mcp_contract::McpAttachmentState::Realizing;
                    next.realization.as_mut().unwrap().realization_id =
                        format!("realize-{index}-2");
                    next.realization.as_mut().unwrap().stage_idempotency_key =
                        format!("stage-{index}-2");
                    session.mcp.attachments.push(next);
                }
                let requests = session
                    .mcp
                    .attachments
                    .iter()
                    .map(|attachment| {
                        crate::projection::stage_mcp_request(
                            "workspace",
                            &SessionId::from(&id),
                            attachment,
                        )
                        .unwrap()
                    })
                    .collect::<Vec<_>>();
                create(repo.as_ref(), session).await;
                let mut receipts = Vec::new();
                for request in requests {
                    receipts.push(NoopMcpRealizer.stage_mcp_attachment(request).await.unwrap());
                }
                let key = |g: &awaken_session_mcp_contract::McpGenerationRef| {
                    format!("{}:{}", g.attachment_id().0, g.generation.version())
                };
                let staged = receipts
                    .iter()
                    .map(|r| key(&r.generation))
                    .collect::<Vec<_>>();
                let expiry_before = repo
                    .get(&SessionId::from(&id))
                    .await
                    .unwrap()
                    .realization
                    .unwrap()
                    .expires_at_unix_ms;
                if renewed {
                    application
                        .renew_session_realization(
                            awaken_session_realization_contract::RenewSessionRealization {
                                session_id: SessionId::from(&id),
                                asserted_lease: lease.clone(),
                                requested_expires_at_unix_ms: lease.expires_at_unix_ms + 1,
                            },
                        )
                        .await
                        .unwrap();
                }
                let directive = application
                    .activate_session_realization(
                        awaken_session_realization_contract::ActivateSessionRealization {
                            session_id: SessionId::from(&id),
                            lease,
                            prepared_resource_revision: None,
                            mcp_receipts: receipts,
                        },
                    )
                    .await
                    .unwrap();
                let awaken_session_realization_contract::SessionRealizationAction::Publish {
                    publish,
                    drain,
                } = directive.action
                else {
                    panic!("A7 expected Publish");
                };
                let durable = repo.get(&SessionId::from(&id)).await.unwrap();
                let active = durable
                    .mcp
                    .attachments
                    .iter()
                    .filter(|a| a.state == awaken_session_mcp_contract::McpAttachmentState::Active)
                    .map(|a| format!("{}:{}", a.attachment_id.0, a.generation.version()))
                    .collect::<Vec<_>>();
                publication_traces.push(serde_json::json!({"rule": format!("count-{count}-predecessor-{predecessor}-renewed-{renewed}"), "cause": {"expiry_before": expiry_before, "expiry_after": durable.realization.as_ref().unwrap().expires_at_unix_ms}, "staged": staged, "active": active, "published": publish.iter().map(key).collect::<Vec<_>>(), "drained": drain.iter().map(key).collect::<Vec<_>>() }));
                assert_eq!(publish.len(), count, "A7 {id}");
                assert!(
                    publish.iter().all(|g| g.generation.version() == 2),
                    "A7 no retired publication"
                );
                assert_eq!(drain.len(), if predecessor { count } else { 0 }, "A7 {id}");
                let recovered = super::application(
                    repo.clone(),
                    Arc::new(RecordingEnvironmentSource::default()),
                );
                let ack = awaken_session_realization_contract::AcknowledgeSessionRealization {
                    session_id: SessionId::from(&id),
                    lease: directive.authority.lease,
                    published: publish,
                    drained: drain,
                };
                recovered
                    .acknowledge_session_realization(ack.clone())
                    .await
                    .unwrap();
                let first = repo.get(&SessionId::from(&id)).await.unwrap();
                recovered
                    .acknowledge_session_realization(ack)
                    .await
                    .unwrap();
                assert_eq!(
                    repo.get(&SessionId::from(id)).await.unwrap(),
                    first,
                    "A7 duplicate ACK is inert"
                );
            }
        }
    }

    awaken_reliability_testkit::write_observation_trace(
        "publication-cardinality-renewal",
        "publication",
        serde_json::json!(publication_traces),
    );

    // A4: a successful same-incarnation/epoch heartbeat occurs after EVERY
    // directive is frozen and before its phase response. All effects and root CAS writes
    // succeed; no immutable binding changes. Renewal must preserve authority
    // without making completed work chase a moving expiry forever. Unlike the
    // finite-renewal Host fixture, this interleaving never stops renewing to
    // make progress possible. Eight rounds are a test failure bound, not a new
    // production retry budget. The real application and SQLite root own both
    // renewal and phase decisions; the existing no-op MCP adapter supplies only
    // exact physical receipts.
    let id = "ack-continuous-renewal";
    let (continuous, _, _) = fixture(id);
    create(repo.as_ref(), continuous).await;
    let initial = repo.get(&SessionId::from(id)).await.unwrap();
    let owner = application.owner(&SessionId::from(id)).await.unwrap();
    let mut directive = awaken_session_realization_contract::SessionRealizationDirective {
        projection: application
            .active_frozen_session_projection(owner.clone(), &initial, true)
            .await
            .unwrap(),
        authority: awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
            initial.realization.clone().unwrap(),
            awaken_agent_contract::clock::Clock::now_ms(&awaken_agent_contract::clock::SystemClock),
        )
        .unwrap(),
        action: awaken_session_realization_contract::SessionRealizationAction::Stage {
            prepare_session: false,
            mcp_stages: vec![
                crate::projection::stage_mcp_request(
                    &owner,
                    &SessionId::from(id),
                    &initial.mcp.attachments[0],
                )
                .unwrap(),
            ],
        },
    };
    let mut stages = 0;
    let mut publications = 0;
    for _ in 0..8 {
        use awaken_session_realization_contract::SessionRealizationAction as Action;
        if matches!(directive.action, Action::Complete) {
            break;
        }
        let current = repo
            .get(&SessionId::from(id))
            .await
            .unwrap()
            .realization
            .unwrap();
        application
            .renew_session_realization(
                awaken_session_realization_contract::RenewSessionRealization {
                    session_id: id.into(),
                    requested_expires_at_unix_ms: current.expires_at_unix_ms + 1,
                    asserted_lease: current,
                },
            )
            .await
            .expect("A4 successful heartbeat before every phase response");
        directive = match directive.action {
            Action::Stage { mcp_stages, .. } => {
                stages += 1;
                let mut receipts = Vec::new();
                for request in mcp_stages {
                    receipts.push(NoopMcpRealizer.stage_mcp_attachment(request).await.unwrap());
                }
                application
                    .activate_session_realization(
                        awaken_session_realization_contract::ActivateSessionRealization {
                            session_id: id.into(),
                            lease: directive.authority.lease,
                            prepared_resource_revision: None,
                            mcp_receipts: receipts,
                        },
                    )
                    .await
                    .unwrap()
            }
            Action::Publish { publish, drain } => {
                publications += 1;
                application
                    .acknowledge_session_realization(
                        awaken_session_realization_contract::AcknowledgeSessionRealization {
                            session_id: id.into(),
                            lease: directive.authority.lease,
                            published: publish,
                            drained: drain,
                        },
                    )
                    .await
                    .unwrap()
            }
            Action::Complete => unreachable!(),
        };
    }
    assert!(
        matches!(
            directive.action,
            awaken_session_realization_contract::SessionRealizationAction::Complete
        ),
        "A4 successful continuous renewal must not prevent publication; stages={stages}, publications={publications}"
    );
    assert_eq!(
        (stages, publications),
        (1, 1),
        "A4 no repeated completed effects"
    );

    // A5: only monotonic expiry may differ. Mutate each immutable request or
    // receipt coordinate independently; even a self-consistent fingerprint for
    // a different target is not this generation's receipt. A6: equal-sized
    // sets containing two expiry variants of attachment A must not substitute
    // for A+B. Every rejection leaves the whole SQLite root unchanged.
    for fault in [
        "workspace",
        "target",
        "name",
        "realization",
        "stage-key",
        "prompts",
        "credential",
        "holder",
        "session",
        "attachment",
        "generation",
        "incarnation",
        "epoch",
        "future-expiry",
        "fingerprint",
        "owner",
        "expired-root",
        "duplicate-expiry",
    ] {
        let id = format!("receipt-reject-{fault}");
        let (mut original, mut lease, _) = fixture(&id);
        if fault == "duplicate-expiry" {
            let mut other = original.mcp.attachments[0].clone();
            other.attachment_id = awaken_session_mcp_contract::McpAttachmentId("other".into());
            other.name = "other".into();
            original.mcp.attachments.push(other);
        }
        let mut request = crate::projection::stage_mcp_request(
            "workspace",
            &SessionId::from(&id),
            &original.mcp.attachments[0],
        )
        .unwrap();
        request.generation.lease_expires_at_unix_ms -= 1;
        match fault {
            "workspace" => request.workspace_id = "foreign".into(),
            "target" => {
                request.target =
                    McpTarget::parse_http("https://different.example.test/mcp").unwrap()
            }
            "name" => request.name = "other".into(),
            "realization" => request.realization_id = "other".into(),
            "stage-key" => request.stage_idempotency_key = "other".into(),
            "prompts" => request.prompts_as_skills = true,
            "holder" => {
                request.selected_plaintext_holder =
                    Some(awaken_runtime_contract::PlaintextHolder::new(
                        awaken_runtime_contract::PlaintextBoundary::Worker,
                        "foreign",
                    ))
            }
            "credential" => {
                use awaken_runtime_contract::{
                    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource,
                    CredentialRef, CredentialUsage, ModelExposurePolicy, PlaintextBoundary,
                    PlaintextHolder,
                };
                request.credential = Some(CredentialAccess::new(
                    CredentialRef {
                        id: "foreign".into(),
                        revision: 1,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    CredentialExecutionPolicy::exact(
                        PlaintextHolder::new(PlaintextBoundary::Worker, "foreign"),
                        ModelExposurePolicy::VirtualOnly,
                    ),
                ));
            }
            "session" => request.generation.session_id = "foreign".into(),
            "attachment" => {
                request.generation.generation = awaken_session_mcp_contract::McpGeneration::new(
                    awaken_session_mcp_contract::McpAttachmentId("foreign".into()),
                    request.generation.generation.version(),
                    request.generation.generation.digest().clone(),
                );
            }
            "generation" => {
                request.generation.generation = request.generation.generation.at_version(2)
            }
            "incarnation" => request.generation.runtime_incarnation = "replacement".into(),
            "epoch" => request.generation.lease_epoch += 1,
            "future-expiry" => request.generation.lease_expires_at_unix_ms = current_expiry + 1,
            "owner" => lease.owner = "foreign".into(),
            "expired-root" => original.realization.as_mut().unwrap().expires_at_unix_ms = 1,
            _ => {}
        }
        let mut receipt = NoopMcpRealizer
            .stage_mcp_attachment(request.clone())
            .await
            .unwrap();
        if fault == "fingerprint" {
            receipt.receipt_fingerprint = "forged".into();
        }
        let mut receipts = vec![receipt];
        if fault == "duplicate-expiry" {
            request.generation.lease_expires_at_unix_ms -= 1;
            receipts.push(NoopMcpRealizer.stage_mcp_attachment(request).await.unwrap());
        }
        create(repo.as_ref(), original).await;
        let before = repo.get(&SessionId::from(&id)).await.unwrap();
        assert!(
            application
                .activate_session_realization(
                    awaken_session_realization_contract::ActivateSessionRealization {
                        session_id: SessionId::from(&id),
                        lease,
                        prepared_resource_revision: None,
                        mcp_receipts: receipts,
                    }
                )
                .await
                .is_err(),
            "A5/A6 {fault} rejected"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "A5/A6 {fault} no mutation"
        );
    }

    // A7: publication acknowledgements may outlive their old expiry only while
    // current same-owner authority is live. Owner/epoch/generation changes,
    // future assertions and duplicate-expiry substitutions remain forbidden.
    for fault in [
        "session",
        "attachment",
        "generation",
        "incarnation",
        "epoch",
        "future-expiry",
        "owner",
        "duplicate-expiry",
    ] {
        let id = format!("ack-reject-{fault}");
        let (mut original, mut lease, mut generation) = fixture(&id);
        generation.lease_expires_at_unix_ms -= 1;
        match fault {
            "session" => generation.session_id = "foreign".into(),
            "attachment" => {
                generation.generation = awaken_session_mcp_contract::McpGeneration::new(
                    awaken_session_mcp_contract::McpAttachmentId("foreign".into()),
                    generation.generation.version(),
                    generation.generation.digest().clone(),
                );
            }
            "generation" => generation.generation = generation.generation.at_version(2),
            "incarnation" => generation.runtime_incarnation = "replacement".into(),
            "epoch" => generation.lease_epoch += 1,
            "future-expiry" => generation.lease_expires_at_unix_ms = current_expiry + 1,
            "owner" => lease.owner = "foreign".into(),
            "duplicate-expiry" => {
                let mut other = original.mcp.attachments[0].clone();
                other.attachment_id = awaken_session_mcp_contract::McpAttachmentId("other".into());
                other.name = "other".into();
                original.mcp.attachments.push(other);
            }
            _ => unreachable!(),
        }
        let mut published = vec![generation.clone()];
        if fault == "duplicate-expiry" {
            generation.lease_expires_at_unix_ms -= 1;
            published.push(generation);
        }
        create(repo.as_ref(), original).await;
        let before = repo.get(&SessionId::from(&id)).await.unwrap();
        assert!(
            application
                .acknowledge_session_realization(
                    awaken_session_realization_contract::AcknowledgeSessionRealization {
                        session_id: SessionId::from(&id),
                        lease,
                        published,
                        drained: Vec::new(),
                    }
                )
                .await
                .is_err(),
            "A7 {fault} rejected"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "A7 {fault} no mutation"
        );
    }

    // A8: lost-ack replay after the asserted expiry has elapsed must read
    // committed truth without another write, both Idle and Running. A current
    // root lease still proves authority; duplicate expiry aliases remain invalid
    // even on this no-write replay edge.
    for running in [false, true] {
        let id = format!("ack-replay-{running}");
        let (mut original, mut lease, mut generation) = fixture(&id);
        original.execution = if running {
            SessionExecutionState::Running
        } else {
            SessionExecutionState::Idle
        };
        original.mcp.attachments[0].publication_acknowledged = true;
        lease.expires_at_unix_ms = 1;
        generation.lease_expires_at_unix_ms = 1;
        create(repo.as_ref(), original).await;
        let before = repo.get(&SessionId::from(&id)).await.unwrap();
        let command = awaken_session_realization_contract::AcknowledgeSessionRealization {
            session_id: SessionId::from(&id),
            lease,
            published: vec![generation.clone()],
            drained: Vec::new(),
        };
        let replay = application
            .acknowledge_session_realization(command.clone())
            .await
            .unwrap();
        assert!(matches!(
            replay.action,
            awaken_session_realization_contract::SessionRealizationAction::Complete
        ));
        assert_eq!(
            repo.get(&SessionId::from(&id)).await.unwrap(),
            before,
            "A8 no duplicate write or activity transition"
        );
        let mut duplicate = command;
        generation.lease_expires_at_unix_ms = 2;
        duplicate.published.push(generation);
        assert!(
            application
                .acknowledge_session_realization(duplicate)
                .await
                .is_err(),
            "A8 duplicate alias rejected on replay"
        );
        assert_eq!(
            repo.get(&SessionId::from(id)).await.unwrap(),
            before,
            "A8 rejection leaves root unchanged"
        );
    }

    let (exact, exact_lease, exact_generation) = fixture("ack-exact");
    create(repo.as_ref(), exact).await;
    let exact_result =
        awaken_session_realization_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_realization_contract::AcknowledgeSessionRealization {
                session_id: "ack-exact".into(),
                lease: exact_lease,
                published: vec![exact_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A1 exact acknowledgement");
    assert!(
        matches!(
            exact_result.action,
            awaken_session_realization_contract::SessionRealizationAction::Complete
        ),
        "A1/E1"
    );
    assert_eq!(
        repo.get(&SessionId::from("ack-exact"))
            .await
            .unwrap()
            .execution
            .as_str(),
        "idle",
        "A1/E1"
    );

    let (renewed, renewed_lease, mut predecessor) = fixture("ack-renewed");
    create(repo.as_ref(), renewed).await;
    predecessor.lease_expires_at_unix_ms -= 1;
    let mut predecessor_lease = renewed_lease.clone();
    predecessor_lease.expires_at_unix_ms -= 1;
    let renewed_result =
        awaken_session_realization_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_realization_contract::AcknowledgeSessionRealization {
                session_id: "ack-renewed".into(),
                lease: predecessor_lease,
                published: vec![predecessor],
                drained: Vec::new(),
            },
        )
        .await
        .expect("A2 monotonic predecessor");
    assert!(
        matches!(
            renewed_result.action,
            awaken_session_realization_contract::SessionRealizationAction::Complete
        ),
        "A2/E2"
    );
    let renewed_truth = repo.get(&SessionId::from("ack-renewed")).await.unwrap();
    assert_eq!(renewed_truth.execution.as_str(), "idle", "A2/E2");
    assert!(
        renewed_truth.mcp.attachments[0].publication_acknowledged,
        "A2/E2"
    );

    let (replacement, replacement_lease, mut wrong_generation) = fixture("ack-replaced");
    create(repo.as_ref(), replacement).await;
    wrong_generation.generation = awaken_session_mcp_contract::McpGeneration::new(
        awaken_session_mcp_contract::McpAttachmentId("other".into()),
        wrong_generation.generation.version(),
        wrong_generation.generation.digest().clone(),
    );
    let error =
        awaken_session_realization_contract::SessionRealizationControl::acknowledge_session_realization(
            &application,
            awaken_session_realization_contract::AcknowledgeSessionRealization {
                session_id: "ack-replaced".into(),
                lease: replacement_lease,
                published: vec![wrong_generation],
                drained: Vec::new(),
            },
        )
        .await
        .expect_err("A3 replacement must fail closed");
    assert!(
        matches!(
            error,
            awaken_session_realization_contract::SessionRealizationControlFailure::Invalid(_)
        ),
        "A3/E3: {error}"
    );
    assert!(
        !repo
            .get(&SessionId::from("ack-replaced"))
            .await
            .unwrap()
            .mcp
            .attachments[0]
            .publication_acknowledged,
        "A3/E3"
    );
}

#[tokio::test]
async fn realization_preserves_or_closes_the_activity_interval_by_terminal_outcome() {
    /* Cause/effect graph: C1 an admitted activity is Running with one open
     * aggregate interval; C2 realization activates and acknowledges exact
     * Resource/publication generations; C3 realization fails retryably below
     * budget; C4 realization fails permanently or after budget. Effects: E1
     * activation/acknowledgement preserve Running and the exact interval; E2
     * activity settlement alone transitions Idle and emits one interval fact;
     * E3 a retry retains Running plus the open interval for recovery; E4 a
     * terminal failure atomically transitions ActivationFailed, closes the
     * interval, accumulates runtime, and emits one fact. Constraints: the root
     * CAS owns execution and interval together; realization never authors a
     * second billing path; stale lease cases are covered by the realization
     * authority table.
     *
     * | Rule | Running | Realization outcome | Retry budget | Effect |
     * |---|---|---|---|---|
     * | R1 | yes | activate + acknowledge | n/a | E1 |
     * | R2 | yes | activity settles | n/a | E2 |
     * | R3 | yes | retryable failure | available | E3 |
     * | R4 | yes | permanent failure | n/a | E4 |
     *
     * This test covers R1, R2, and R4. R3's durable retry/lease-expiry rule is
     * covered by `retryable_initial_realization_is_fenced_and_budgeted_durably`;
     * the production branch is identical for Running except it deliberately
     * retains the already-open interval. */
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let clock = Arc::new(awaken_agent_contract::clock::ManualClock::new(500));
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    )
    .with_clock(clock.clone());
    let lease = awaken_session_resource_contract::SessionRealizationLease {
        owner: "worker-a".into(),
        runtime_incarnation: "worker-a/boot-2".into(),
        epoch: 2,
        expires_at_unix_ms: u64::MAX,
    };
    let interval = |id: &str| awaken_session_state_contract::SessionRuntimeIntervalStart {
        interval_id: format!("interval:{id}"),
        activity_epoch: 1,
        started_at_unix_ms: 1,
        opened_revision: Default::default(),
        observations: Vec::new(),
    };

    let mut recovering = persisted("running-realization", false, "running");
    recovering.activity_epoch = 1;
    recovering.running_interval = Some(interval("running-realization"));
    recovering.realization = Some(lease.clone());
    recovering
        .resources
        .prepare(
            &awaken_session_types::SessionId::from("running-realization"),
            file_resources("running-realization"),
        )
        .expect("R1 pending Resource generation");
    let prepared_resource_revision = recovering.resources.revision;
    create(repo.as_ref(), recovering).await;

    let activated =
        awaken_session_realization_contract::SessionRealizationControl::activate_session_realization(
            &application,
            awaken_session_realization_contract::ActivateSessionRealization {
                session_id: "running-realization".into(),
                lease: lease.clone(),
                prepared_resource_revision: Some(prepared_resource_revision),
                mcp_receipts: Vec::new(),
            },
        )
        .await
        .expect("R1 activation");
    assert!(
        matches!(
            activated.action,
            awaken_session_realization_contract::SessionRealizationAction::Publish { .. }
        ),
        "R1/E1"
    );
    let activated_truth = repo
        .get(&SessionId::from("running-realization"))
        .await
        .unwrap();
    assert_eq!(
        activated_truth.execution,
        SessionExecutionState::Running,
        "R1/E1"
    );
    assert_eq!(
        activated_truth.running_interval,
        Some(interval("running-realization")),
        "R1/E1"
    );

    awaken_session_realization_contract::SessionRealizationControl::acknowledge_session_realization(
        &application,
        awaken_session_realization_contract::AcknowledgeSessionRealization {
            session_id: "running-realization".into(),
            lease: lease.clone(),
            published: Vec::new(),
            drained: Vec::new(),
        },
    )
    .await
    .expect("R1 acknowledgement");
    let acknowledged = repo
        .get(&SessionId::from("running-realization"))
        .await
        .unwrap();
    assert_eq!(
        acknowledged.execution,
        SessionExecutionState::Running,
        "R1/E1"
    );
    assert_eq!(
        acknowledged.running_interval,
        Some(interval("running-realization")),
        "R1/E1"
    );

    let settled = application
        .settle_activity("running-realization", 1)
        .await
        .expect("R2 activity settlement");
    assert_eq!(settled.execution, SessionExecutionState::Idle, "R2/E2");
    assert!(settled.running_interval.is_none(), "R2/E2");

    let mut failed = persisted("running-realization-failed", false, "running");
    failed.activity_epoch = 1;
    failed.running_interval = Some(interval("running-realization-failed"));
    failed.realization = Some(lease.clone());
    create(repo.as_ref(), failed).await;
    application
        .fail_session_realization(
            awaken_session_realization_contract::FailSessionRealization {
                session_id: "running-realization-failed".into(),
                lease,
                prepared_resource_revision: None,
                retryable: false,
                source_run_id: Some(awaken_agent_contract::agent::run::Id(
                    "running-realization-run".into(),
                )),
                reason: "permanent realization failure".into(),
            },
        )
        .await
        .expect("R4 terminal realization failure");
    let failed_truth = repo
        .get(&SessionId::from("running-realization-failed"))
        .await
        .unwrap();
    assert_eq!(
        failed_truth.realization_progress.failed_at_unix_ms,
        Some(500),
        "R5 failure observes application clock"
    );
    // R5 later housekeeping commits cannot rewrite the failure-owned time;
    // terminal failure redelivery is inert under a different recovery clock.
    clock.set(900);
    let owner = application
        .owner(&SessionId::from("running-realization-failed"))
        .await
        .unwrap();
    let later = application
        .commit_session_snapshot_with_outcome(
            &owner,
            failed_truth.clone(),
            "later-housekeeping",
            vec![],
        )
        .await
        .unwrap()
        .into_session();
    assert_eq!(later.updated_at_unix_ms, Some(900));
    assert_eq!(later.realization_progress.failed_at_unix_ms, Some(500));
    application
        .fail_session_realization(
            awaken_session_realization_contract::FailSessionRealization {
                session_id: "running-realization-failed".into(),
                lease: later.realization.clone().unwrap(),
                prepared_resource_revision: None,
                retryable: false,
                source_run_id: None,
                reason: "permanent realization failure".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        repo.get(&SessionId::from("running-realization-failed"))
            .await
            .unwrap(),
        later,
        "R5 terminal redelivery"
    );
    assert_eq!(
        failed_truth.execution,
        SessionExecutionState::ActivationFailed,
        "R4/E4"
    );
    assert_eq!(
        failed_truth
            .realization_progress
            .failure_source_run_id
            .as_deref(),
        Some(&awaken_agent_contract::agent::run::Id(
            "running-realization-run".into()
        )),
        "R4/E4 retains the exact Run failure owner"
    );
    assert!(failed_truth.running_interval.is_none(), "R4/E4");
    assert!(failed_truth.runtime_active_millis > 0, "R4/E4");
    let facts = repo.pending_lifecycle().await.expect("R2/R4 outbox");
    for session_id in ["running-realization", "running-realization-failed"] {
        assert_eq!(
            facts
                .iter()
                .filter(|fact| fact.object_id == session_id && fact.runtime_interval.is_some())
                .count(),
            1,
            "{session_id} emits exactly one interval fact"
        );
    }
}
