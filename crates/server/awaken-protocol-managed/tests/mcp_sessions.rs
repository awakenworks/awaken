// awaken-allow(file-limit-ceiling): 3955 (legacy oversized test file: split, never grow)
//! Session-create MCP binding (ADR-0043 Phase 3): a session's `mcp_servers` are
//! bound to vault credentials by the canonical normalized-URL rule. Preparing,
//! frozen generation-1 state and realization claims are durable before Runtime
//! I/O; a failed realization fails the create with the mapped error envelope.

use awaken_session_repository_contract::LifecycleOutbox as _;
use awaken_session_types::SessionId;
mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_credential_vault::InMemorySecretStore;
use awaken_credential_vault::repo::InMemoryCredentialRepo;
use awaken_protocol_managed::tools::AgentTool;
use awaken_protocol_managed::{ManagedState, VaultState, router, vault_router};
use awaken_session_application_contract::{LifecycleFactNotifier, StepOutcome};
use awaken_session_configuration_contract::SessionInit;
use awaken_session_execution_contract::{OutcomeDrive, SessionRuntime, ToolPermissionDecision};
use awaken_session_state_contract::{PersistedSession, SessionExecutionState};
use awaken_session_store::SqliteManagedSessionRepository;
use awaken_session_types::RunError;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use support::{ScheduledConflictRepository, replace_session_fixture};

/// A fake runtime that records every materialized complete projection (and optionally
/// fails it), so a test can assert exactly what a session create provisions.
struct PreparingFake {
    captured: Arc<Mutex<Vec<SessionInit>>>,
    staged: Arc<Mutex<Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>>>,
    observed_durable: Arc<Mutex<Vec<PersistedSession>>>,
    repo: Option<Arc<dyn awaken_session_repository_contract::SessionStore>>,
    fail_with: Option<fn() -> RunError>,
}

#[derive(Default)]
struct CountingLifecycleNotifier(AtomicUsize);

impl LifecycleFactNotifier for CountingLifecycleNotifier {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl CountingLifecycleNotifier {
    fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

async fn lifecycle_test_state() -> (
    ManagedState,
    Arc<SqliteManagedSessionRepository>,
    Arc<CountingLifecycleNotifier>,
) {
    let repository = support::session_repository("open Session repository").await;
    let notifier = Arc::new(CountingLifecycleNotifier::default());
    let state = {
        let session_fixture_runtime = std::sync::Arc::new(PreparingFake {
            captured: Arc::new(Mutex::new(Vec::new())),
            staged: Arc::new(Mutex::new(Vec::new())),
            observed_durable: Arc::new(Mutex::new(Vec::new())),
            repo: None,
            fail_with: None,
        });
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            session_fixture_runtime.clone(),
            Some(session_fixture_runtime),
            repository.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }
    .with_lifecycle_notifier(notifier.clone());
    (state, repository, notifier)
}

#[async_trait::async_trait]
impl SessionRuntime for PreparingFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for PreparingFake);

awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for PreparingFake);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for PreparingFake);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for PreparingFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for PreparingFake);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for PreparingFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for PreparingFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for PreparingFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
        awaken_protocol_managed_testkit::observe_realization_request()
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        if matches!(
            mode,
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch
                | awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                    prepare_session: true,
                    ..
                }
        ) {
            if let Some(repo) = &self.repo {
                match repo.get(&SessionId::from(thread)).await {
                    Ok(session) => self.observed_durable.lock().unwrap().push(session),
                    Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => {}
                    Err(error) => return Err((RunError::internal(error.to_string())).into()),
                }
            }
            self.captured
                .lock()
                .unwrap()
                .push(projection.session_init());
            match self.fail_with {
                Some(failure) => Err((failure()).into()),
                None => Ok(()),
            }
        } else {
            Ok(())
        }
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
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for PreparingFake {
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
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(awaken_protocol_managed_testkit::complete_terminal_cleanup_disposal(&effect))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for PreparingFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        _thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        // This fixture never commits a Run. Explicit absence keeps it honest
        // without weakening the production default for unsupported recovery.
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.session_thread_recovery_snapshot(session_id, thread_id)
                .await?,
            run_id,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for PreparingFake {
    async fn observe_terminal_cleanup_retention(
        &self,
        session_id: &awaken_session_execution_contract::__macro_support::SessionId,
        projection: &awaken_session_execution_contract::__macro_support::FrozenSessionProjection,
        source_prepared: bool,
    ) -> Result<bool, awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::TerminalCleanupRuntime::observe_terminal_cleanup_retention(
            &awaken_session_execution_contract::NeutralRuntime,
            session_id,
            projection,
            source_prepared,
        )
        .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        awaken_protocol_managed_testkit::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for PreparingFake {
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
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
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

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for PreparingFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        self.staged.lock().unwrap().push(request.clone());
        let receipt_fingerprint = request.fingerprint();
        Ok(
            awaken_session_configuration_contract::mcp::McpRealizationReceipt {
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: Some(
                    awaken_credential_contract::CredentialRealizationKind::WorkerRelay,
                ),
                receipt_fingerprint,
            },
        )
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

struct Harness {
    app: Router,
    vaults: Arc<VaultState>,
    captured: Arc<Mutex<Vec<SessionInit>>>,
    staged: Arc<Mutex<Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>>>,
    observed_durable: Arc<Mutex<Vec<PersistedSession>>>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
}

#[derive(Clone, Copy, Debug, Default)]
enum HotStageMode {
    #[default]
    Success,
    FailNext,
    MismatchNext,
}

#[derive(Default)]
struct HotRuntimeState {
    staged: Vec<awaken_session_configuration_contract::mcp::StageMcpAttachment>,
    published: Vec<awaken_session_configuration_contract::mcp::McpGenerationRef>,
    drained: Vec<awaken_session_configuration_contract::mcp::McpGenerationRef>,
    durable_at_stage: Vec<PersistedSession>,
    mode: HotStageMode,
    fail_publish_next: bool,
    fail_drain_next: bool,
    fail_replace_tools_next: bool,
    replaced_tools: Vec<(
        String,
        awaken_session_configuration_contract::SessionToolConfiguration,
    )>,
}

struct HotRuntime {
    state: Arc<Mutex<HotRuntimeState>>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
}

#[async_trait::async_trait]
impl SessionRuntime for HotRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "hot-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for HotRuntime);

awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for HotRuntime);

awaken_session_execution_contract::neutral_role!(BudgetRuntime for HotRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for HotRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for HotRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for HotRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for HotRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for HotRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
        awaken_protocol_managed_testkit::observe_realization_request()
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        let _ = awaken_protocol_managed_testkit::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
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
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for HotRuntime {
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_types::RunError> {
        let mut state = self.state.lock().unwrap();
        state.replaced_tools.push((thread.to_string(), tools));
        if std::mem::take(&mut state.fail_replace_tools_next) {
            return Err(RunError::internal("hot tool replacement failed"));
        }
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
    ) -> Result<(), awaken_session_types::RunError> {
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
        expected_mcp_generations: &[awaken_session_configuration_contract::mcp::McpGenerationRef],
    ) -> Result<awaken_session_state_contract::QuiescenceReceipt, awaken_session_types::RunError>
    {
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
    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<
        Vec<awaken_session_configuration_contract::resource::ResolvedSkillBinding>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resolve_session_skills(workspace_id, skills)
            .await
    }
}

awaken_session_execution_contract::neutral_role!(RealizationEffects for HotRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for HotRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        _thread_id: &str,
    ) -> Result<Option<RunRecoverySnapshot>, RunError> {
        // Hot MCP replacement mutates only the Session aggregate; no fixture
        // Run exists from which an atomic Thread prefix could be recovered.
        Ok(None)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.session_thread_recovery_snapshot(session_id, thread_id)
                .await?,
            run_id,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for HotRuntime {
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
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("unused"))
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

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for HotRuntime {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<awaken_session_configuration_contract::mcp::McpRealizationReceipt, RunError> {
        let durable = self
            .repo
            .get(request.generation.session_id())
            .await
            .expect("stage follows durable claim");
        let mut state = self.state.lock().unwrap();
        state.durable_at_stage.push(durable);
        state.staged.push(request.clone());
        let mode = std::mem::take(&mut state.mode);
        if matches!(mode, HotStageMode::FailNext) {
            return Err(RunError::internal("hot stage failed"));
        }
        let receipt_fingerprint = request.fingerprint();
        let mut generation = request.generation;
        if matches!(mode, HotStageMode::MismatchNext) {
            let mut w = generation.to_wire();
            w.generation = w.generation.at_version(w.generation.version() + 1);
            generation = w.try_into().unwrap();
        }
        Ok(
            awaken_session_configuration_contract::mcp::McpRealizationReceipt {
                generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
                receipt_fingerprint,
            },
        )
    }

    async fn publish_mcp_generation(
        &self,
        generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        let mut state = self.state.lock().unwrap();
        state.published.push(generation);
        if std::mem::take(&mut state.fail_publish_next) {
            return Err(RunError::internal("hot publish failed"));
        }
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        generation: awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        let mut state = self.state.lock().unwrap();
        state.drained.push(generation);
        if std::mem::take(&mut state.fail_drain_next) {
            return Err(RunError::internal("hot drain failed"));
        }
        Ok(())
    }
}

struct HotHarness {
    app: Router,
    managed: Arc<ManagedState>,
    state: Arc<Mutex<HotRuntimeState>>,
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
}

async fn hot_harness() -> HotHarness {
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        support::session_repository("open hot Session repository").await;
    hot_harness_with_repo(repo).await
}

async fn hot_harness_with_repo(
    repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
) -> HotHarness {
    let runtime_state = Arc::new(Mutex::new(HotRuntimeState::default()));
    let managed = Arc::new({
        let session_fixture_runtime = std::sync::Arc::new(HotRuntime {
            state: runtime_state.clone(),
            repo: repo.clone(),
        });
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            session_fixture_runtime.clone(),
            Some(session_fixture_runtime),
            repo.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    });
    HotHarness {
        app: router(managed.clone()),
        managed,
        state: runtime_state,
        repo,
    }
}

/// Sessions + vaults over ONE shared `VaultState`, the way the server mounts
/// them, so a credential entered through the vault routes is bindable at create.
async fn harness(fail_with: Option<fn() -> RunError>) -> Harness {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(support::vault(secrets, credentials));
    let captured = Arc::new(Mutex::new(Vec::new()));
    let staged = Arc::new(Mutex::new(Vec::new()));
    let observed_durable = Arc::new(Mutex::new(Vec::new()));
    let repo: Arc<dyn awaken_session_repository_contract::SessionStore> =
        support::session_repository("open Session repository").await;
    let state = {
        let session_fixture_runtime = std::sync::Arc::new(PreparingFake {
            captured: captured.clone(),
            staged: staged.clone(),
            observed_durable: observed_durable.clone(),
            repo: Some(repo.clone()),
            fail_with,
        });
        let session_fixture_clock = awaken_time::system_clock();
        awaken_protocol_managed_testkit::managed_state_from_parts(
            session_fixture_runtime.clone(),
            Some(session_fixture_runtime),
            repo.clone(),
            awaken_protocol_managed_testkit::environment_fixture(session_fixture_clock.clone())
                .execution,
            session_fixture_clock,
            std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
        )
    }
    .with_vaults(vaults.clone());
    let app = router(Arc::new(state)).merge(vault_router(vaults.clone()));
    Harness {
        app,
        vaults,
        captured,
        staged,
        observed_durable,
        repo,
    }
}

/// Cause/effect decision table for the shared bind-time credential check:
/// O3/R1: unknown vault -> `VaultNotFound` before minting, via the credential role;
/// R2: no vault is referenced -> accept without consulting secret material.
/// The credential source is the only dependency, so these two rules cover every
/// branch and prove that pre-flight validation remains fail-closed after the port split.
#[tokio::test]
async fn check_bind_is_fail_closed_on_unknown_vault() {
    let secrets = Arc::new(InMemorySecretStore::new());
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let vaults = Arc::new(support::vault(secrets, credentials));
    let state = support::state_with_mcp(PreparingFake {
        captured: Arc::new(Mutex::new(Vec::new())),
        staged: Arc::new(Mutex::new(Vec::new())),
        observed_durable: Arc::new(Mutex::new(Vec::new())),
        repo: None,
        fail_with: None,
    })
    .await
    .with_vaults(vaults);

    let bad: awaken_protocol_managed::types::SessionCreateParams = serde_json::from_value(json!({
        "agent": "a",
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        "vault_ids": ["vlt_missing"]
    }))
    .unwrap();
    assert!(matches!(
        state.check_bind("default", &bad).await,
        Err(awaken_protocol_managed::StateError::VaultNotFound(_))
    ));

    // No referenced vault → the bind is legal.
    let ok: awaken_protocol_managed::types::SessionCreateParams = serde_json::from_value(json!({
        "agent": "a",
        "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
    }))
    .unwrap();
    assert!(state.check_bind("default", &ok).await.is_ok());
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> support::Call<serde_json::Value> {
    call_with_headers(app, method, uri, body, &[])
        .await
        .map(|(_headers, body)| body)
}

/// Build the official `agent_with_overrides` MCP union once for these lifecycle
/// tests. MCP declarations and toolsets are a bijection on the SDK wire.
fn session_with_mcp(agent_id: &str, mcp_servers: Vec<Value>, vault_ids: Vec<String>) -> Value {
    let tools = mcp_servers
        .iter()
        .map(|server| {
            json!({
                "type": "mcp_toolset",
                "mcp_server_name": server["name"].as_str().expect("MCP fixture name")
            })
        })
        .collect::<Vec<_>>();
    json!({
        "agent": {
            "id": agent_id,
            "type": "agent_with_overrides",
            "mcp_servers": mcp_servers,
            "tools": tools
        },
        "vault_ids": vault_ids
    })
}

#[tokio::test]
async fn session_mcp_overrides_validate_the_effective_pair_not_field_presence() {
    // Cause/effect matrix for an Agent with no inherited MCP configuration:
    // overriding either half alone, using different names, or duplicating a
    // ToolSet must fail before Session persistence or MCP staging. Only the
    // exact server/ToolSet pair is accepted.
    let h = harness(None).await;
    let server = json!({ "name": "docs", "type": "url", "url": MCP_URL });
    let toolset = json!({ "type": "mcp_toolset", "mcp_server_name": "docs" });
    for agent in [
        json!({
            "id": "agent", "type": "agent_with_overrides",
            "mcp_servers": [server.clone()]
        }),
        json!({
            "id": "agent", "type": "agent_with_overrides",
            "tools": [toolset.clone()]
        }),
        json!({
            "id": "agent", "type": "agent_with_overrides",
            "mcp_servers": [server.clone()],
            "tools": [{ "type": "mcp_toolset", "mcp_server_name": "other" }]
        }),
        json!({
            "id": "agent", "type": "agent_with_overrides",
            "mcp_servers": [server.clone()],
            "tools": [toolset.clone(), toolset.clone()]
        }),
    ] {
        let body = call(
            &h.app,
            "POST",
            "/v1/sessions",
            Some(json!({ "agent": agent })),
        )
        .await
        .expect_for(StatusCode::BAD_REQUEST, "expected status");
        assert!(
            body.to_string().contains("exactly one mcp_toolset"),
            "{body}"
        );
    }
    assert!(h.staged.lock().unwrap().is_empty());
}

async fn call_with_headers(
    app: &Router,
    method: &str,
    uri: &str,
    mut body: Option<Value>,
    headers: &[(&str, &str)],
) -> support::Call<(axum::http::HeaderMap, Value)> {
    // These tests isolate MCP lifecycle decisions. Every Session fixture still
    // sends the SDK-required explicit Environment; this transport helper never
    // changes production behavior or grants a local fallback.
    if method == "POST"
        && uri == "/v1/sessions"
        && let Some(object) = body.as_mut().and_then(Value::as_object_mut)
    {
        object.insert(
            "environment_id".into(),
            json!(awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID),
        );
    }
    let mut b = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        b = b.header(*name, *value);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    // Production startup stamps the trusted Workspace before these resource
    // routers run. This fixture mounts routers directly, so reproduce that edge
    // explicitly instead of relying on the removed unscoped fallback.
    let mut request = b.body(body).unwrap();
    request
        .extensions_mut()
        .insert(awaken_tenancy::WorkspaceScope("default".into()));
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    support::Call::new(status, (headers, value))
}

const MCP_URL: &str = "https://mcp.example.com/sse";

/// Create a vault holding an `mcp_oauth` credential for [`MCP_URL`]; returns
/// `(vault_id, credential_id)`.
async fn vault_with_mcp_oauth(h: &Harness) -> (String, String) {
    let vault = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "mcp" })),
    )
    .await
    .expect(StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let cred = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "mcp_oauth",
            "mcp_server_url": MCP_URL,
            "access_token": "at-secret-token" // awaken-allow(secret):
        })),
    )
    .await
    .expect(StatusCode::OK);
    (vault_id, cred["id"].as_str().unwrap().to_string())
}

async fn create_mcp_credential(
    h: &Harness,
    vault_id: &str,
    kind: &str,
    url: &str,
    secret: &str,
) -> String {
    let auth = match kind {
        "static_bearer" => json!({
            "type": kind,
            "mcp_server_url": url,
            "token": secret
        }),
        "mcp_oauth" => json!({
            "type": kind,
            "mcp_server_url": url,
            "access_token": secret
        }),
        _ => panic!("unsupported test credential kind: {kind}"),
    };
    let credential = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(auth),
    )
    .await
    .expect(StatusCode::OK);
    credential["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn create_binds_mcp_server_to_vault_credential_and_echoes_the_wire_shape() {
    // Construction C1 selected durable Store; C2 one original Runtime Arc
    // supplies both execution and MCP => the existing real bind/stage/root
    // assertions below observe one graph. No late Store selector or second
    // Runtime/MCP fixture is allowed to repair a failed construction.
    let h = harness(None).await;
    let (vault_id, cred_id) = vault_with_mcp_oauth(&h).await;

    let session = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec![vault_id.clone()],
        )),
    )
    .await
    .expect(StatusCode::OK);
    // The agent object echoes the accepted server in the SDK response shape.
    assert_eq!(
        session["agent"]["mcp_servers"],
        json!([{ "name": "calc", "type": "url", "url": MCP_URL }])
    );

    // The exact-generation stage saw the secret-free credential access; the
    // baseline-only prepare call carries no parallel MCP authority.
    let expected = h
        .vaults
        .application()
        .credential_access()
        .credential_source_id("default", &vault_id, &cred_id)
        .await
        .expect("wire credential maps to a domain source");
    {
        let captured = h.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].agent_id, "calc-agent");
    }
    {
        let staged = h.staged.lock().unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].name, "calc");
        assert_eq!(staged[0].target.http_url(), Some(MCP_URL));
        // The binding carries the neutral row-id string (the port speaks no vault vocab).
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.id.as_str()),
            Some(expected.0.as_str())
        );
        assert_eq!(
            staged[0]
                .credential
                .as_ref()
                .map(|access| access.credential.revision),
            Some(1),
            "Session-inline Vault selection is compiled to one exact credential revision"
        );
        // The credential was entered without a refresh object, so the binding
        // carries no refresh configuration.
        assert!(
            staged[0]
                .credential
                .as_ref()
                .and_then(|access| access.refresh.as_ref())
                .is_none()
        );
    }

    // Cause graph: exact generation is Requested -> lease+claim root CAS ->
    // Runtime stage -> success/failure -> terminal activation CAS.
    //
    // | Rule | generation requested | claim durable before I/O | stage result | Effect |
    // |---|---|---|---|---|
    // | A1 | T | T | success | Active + visible |
    // | A2 | T | T | failure | Failed + hidden |
    // | A3 | F | - | success | no attachment effect |
    {
        let observed = h.observed_durable.lock().unwrap();
        assert_eq!(observed.len(), 1, "A1");
        assert_eq!(
            observed[0].revision,
            awaken_session_types::SessionRevision(2),
            "A1 one complete frozen insert -> claim generation 1, all before Runtime I/O"
        );
        assert!(matches!(
            observed[0].baseline,
            awaken_session_configuration_contract::SessionBaselineState::Frozen(_)
        ));
        assert!(observed[0].realization.is_some(), "A1 lease is durable");
        assert_eq!(
            observed[0].mcp.attachments[0].state,
            awaken_session_configuration_contract::mcp::McpAttachmentState::Realizing,
            "A1 claim is durable before Runtime I/O"
        );
    }
    let session_id = session["id"].as_str().expect("created Session id");
    let active = h
        .repo
        .get(&SessionId::from(session_id))
        .await
        .expect("active aggregate");
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "A1"
    );
    assert!(
        active.mcp.attachments[0].publication_acknowledged,
        "A1 publication acknowledgement is durable before success"
    );
    assert_eq!(active.visible_mcp_servers().len(), 1, "A1");
}

#[tokio::test]
async fn session_binding_supports_static_bearer_and_normalized_mcp_urls() {
    // Decision S1: C1 a static-bearer credential and Session MCP URL normalize
    // to the same target -> E1 stage carries that credential's exact source and
    // revision. Constraint: URL normalization selects identity only; plaintext
    // remains under Vault custody and never enters the Session aggregate.
    let h = harness(None).await;
    let vault = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "bearer" })),
    )
    .await
    .expect(StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let credential_id = create_mcp_credential(
        &h,
        &vault_id,
        "static_bearer",
        "HTTPS://MCP.EXAMPLE.COM:443/sse/",
        "static-secret", // awaken-allow(secret):
    )
    .await;

    let _ = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": "https://mcp.example.com/sse" })],
            vec![vault_id.clone()],
        )),
    )
    .await
    .expect(StatusCode::OK);
    let expected = h
        .vaults
        .application()
        .credential_access()
        .credential_source_id("default", &vault_id, &credential_id)
        .await
        .unwrap();
    let staged = h.staged.lock().unwrap();
    assert_eq!(
        staged[0]
            .credential
            .as_ref()
            .map(|access| access.credential.id.as_str()),
        Some(expected.0.as_str())
    );
}

#[tokio::test]
async fn session_binding_honors_vault_order_and_leaves_a_miss_unauthenticated() {
    // Causes/effects: C1 two Vaults match in caller order -> E1 the first named
    // Vault wins; C2 no credential matches the normalized target -> E2 optional
    // MCP auth remains absent. Decision V1=C1=>E1, V2=C2=>E2. Constraint: order
    // is explicit request policy and a miss never fabricates credential access.
    let h = harness(None).await;
    let mut vaults = Vec::new();
    let mut sources = Vec::new();
    for name in ["first-created", "second-created"] {
        let vault = call(
            &h.app,
            "POST",
            "/v1/vaults",
            Some(json!({ "display_name": name })),
        )
        .await
        .expect(StatusCode::OK);
        let vault_id = vault["id"].as_str().unwrap().to_string();
        let credential_id = create_mcp_credential(
            &h,
            &vault_id,
            "mcp_oauth",
            MCP_URL,
            name, // awaken-allow(secret):
        )
        .await;
        sources.push(
            h.vaults
                .application()
                .credential_access()
                .credential_source_id("default", &vault_id, &credential_id)
                .await
                .unwrap(),
        );
        vaults.push(vault_id);
    }

    let _ = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "ordered",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec![vaults[1].clone(), vaults[0].clone()],
        )),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(
        h.staged.lock().unwrap()[0]
            .credential
            .as_ref()
            .map(|access| access.credential.id.as_str()),
        Some(sources[1].0.as_str())
    );

    let _ = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "anonymous",
            vec![json!({ "name": "other", "type": "url", "url": "https://other.example.com/mcp" })],
            vec![vaults[0].clone()],
        )),
    )
    .await
    .expect(StatusCode::OK);
    assert!(
        h.staged.lock().unwrap()[1].credential.is_none(),
        "Anthropic-compatible optional MCP auth leaves a non-match unauthenticated"
    );
}

#[tokio::test]
async fn an_archived_vault_cannot_be_attached_to_a_new_session() {
    // Decision A1: C1 the only referenced Vault is archived -> E1 create returns
    // 404 and E2 no Runtime preparation occurs. Constraint: lifecycle denial is
    // checked before credential selection or any Session/Runtime side effect.
    let h = harness(None).await;
    let (vault_id, _) = vault_with_mcp_oauth(&h).await;
    let _ = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/archive"),
        None,
    )
    .await
    .expect(StatusCode::OK);

    let _ = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec![vault_id],
        )),
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    assert!(h.captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_carries_the_refresh_binding_of_a_refreshable_credential() {
    // Decision F1: C1 matching OAuth access includes a refresh configuration ->
    // E1 stage receives exact endpoint/client/scope plus an opaque sealed-token
    // reference. Constraint: neither access nor refresh plaintext crosses the
    // Vault boundary; the reference remains bound to source revision one.
    let h = harness(None).await;
    let vault = call(
        &h.app,
        "POST",
        "/v1/vaults",
        Some(json!({ "display_name": "mcp" })),
    )
    .await
    .expect(StatusCode::OK);
    let vault_id = vault["id"].as_str().unwrap().to_string();
    let cred = call(
        &h.app,
        "POST",
        &format!("/v1/vaults/{vault_id}/credentials"),
        Some(json!({
            "type": "mcp_oauth",
            "mcp_server_url": MCP_URL,
            "access_token": "at-secret-token", // awaken-allow(secret):
            "refresh": {
                "client_id": "cli_pub",
                "refresh_token": "rt-secret-token", // awaken-allow(secret):
                "token_endpoint": "https://auth.example.com/token",
                "token_endpoint_auth": { "type": "none" },
                "scope": "mcp:read"
            }
        })),
    )
    .await
    .expect(StatusCode::OK);
    let cred_id = cred["id"].as_str().unwrap().to_string();

    let _ = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec![vault_id.clone()],
        )),
    )
    .await
    .expect(StatusCode::OK);

    // The binding carries the stored refresh configuration next to the source
    // id — the sealed refresh token's ref, never the token itself.
    let refresh_token_ref = {
        let staged = h.staged.lock().unwrap();
        let refresh = staged[0]
            .credential
            .as_ref()
            .and_then(|access| access.refresh.as_ref())
            .expect("a refreshable credential's binding carries its refresh config");
        assert_eq!(refresh.token_endpoint, "https://auth.example.com/token");
        assert_eq!(refresh.client_id, "cli_pub");
        assert_eq!(refresh.scope.as_deref(), Some("mcp:read"));
        assert_eq!(refresh.resource, None);
        refresh.refresh_token_ref.clone()
    };
    let source_id = h
        .vaults
        .application()
        .credential_access()
        .credential_source_id("default", &vault_id, &cred_id)
        .await
        .unwrap();
    let logical_prefix = format!(
        "sec:{}:r1:{}:attempt:",
        source_id.0,
        awaken_credential_vault::OAUTH_REFRESH_TOKEN_SLOT
    );
    assert!(refresh_token_ref.starts_with(&logical_prefix));
    assert!(refresh_token_ref.len() > logical_prefix.len());
}

#[tokio::test]
async fn session_without_mcp_servers_echoes_empty_and_prepares_an_empty_init() {
    // Decision A3: C1 create supplies no MCP declaration -> E1 wire MCP list and
    // durable attachment aggregate are empty, E2 baseline preparation runs once,
    // and E3 no MCP stage effect occurs. Constraint: absence is represented by
    // empty canonical state, not a second optional attachment authority.
    let h = harness(None).await;
    let session = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "coder" })),
    )
    .await
    .expect(StatusCode::OK);
    assert_eq!(session["agent"]["mcp_servers"], json!([]));
    {
        let captured = h.captured.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].agent_id, "coder");
    }
    assert!(h.staged.lock().unwrap().is_empty());
    let session_id = session["id"].as_str().expect("A3 Session id");
    let durable = h
        .repo
        .get(&SessionId::from(session_id))
        .await
        .expect("A3 aggregate");
    assert!(durable.mcp.attachments.is_empty(), "A3/E1");
}

/// Fail closed at create: a `vault_ids` entry naming no vault 404s with the
/// standard envelope naming the vault id — no session record is left behind
/// and the runtime is never asked to provision anything.
#[tokio::test]
async fn unknown_vault_id_fails_the_create_with_404_and_provisions_nothing() {
    // Decision U1: C1 the sole referenced Vault is unknown -> E1 standard 404
    // naming that id, E2 no Session row, and E3 no preparation. Constraint: the
    // complete Vault set is admitted before identity minting or Runtime I/O.
    let h = harness(None).await;
    let body = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec!["vlt_missing".into()],
        )),
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("vlt_missing"),
        "the envelope names the offending vault id: {message}"
    );
    assert!(
        h.captured.lock().unwrap().is_empty(),
        "the complete projection port must never run for a refused create"
    );
    let _ = call(&h.app, "GET", "/v1/sessions/sesn_0", None)
        .await
        .expect_for(StatusCode::NOT_FOUND, "no session record was created");
}

/// A create mixing one real vault with one bogus id fails closed too — every
/// named vault must exist, not just some.
#[tokio::test]
async fn known_plus_unknown_vault_id_still_fails_the_create() {
    // Decision U2: C1 one known plus one unknown Vault appears in the same
    // request -> E1 reject the entire create and E2 perform no preparation.
    // Constraint: Vault admission is all-or-nothing; a valid member cannot mask
    // an invalid member or authorize a partial MCP binding.
    let h = harness(None).await;
    let (vault_id, _) = vault_with_mcp_oauth(&h).await;
    let body = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "calc-agent",
            vec![json!({ "name": "calc", "type": "url", "url": MCP_URL })],
            vec![vault_id, "vlt_bogus".into()],
        )),
    )
    .await
    .expect(StatusCode::NOT_FOUND);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("vlt_bogus")
    );
    assert!(h.captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failing_complete_projection_fails_the_create_with_the_mapped_envelope() {
    // Cause/effect decision table: R1 permanent (classified) internal
    // preparation failure maps to 500/api_error; R2 caller-invalid preparation
    // maps to 400/invalid_request_error; R3 transient Environment image
    // readiness maps to 503/api_error while retaining a queryable Preparing
    // intent for fenced retry; R4 an unclassified internal failure maps to
    // 500/api_error and R5 a state conflict to 409/invalid_request_error, both
    // retaining the Preparing intent like R3 (a bounded retry that consumed one
    // attempt of the realization budget). Permanent failures remain queryable activation_failed roots but
    // project as terminated/failed. FMECA: classifying every outage as permanent
    // loses recovery; projecting a permanent failure as live admits Runs without
    // a Runtime; hiding it loses the durable cause and makes exact GET disagree
    // with the Session repository. The error kind and persisted retry budget
    // distinguish those outcomes at the same realization boundary. The Managed
    // Session projects only the official status union; the durable root, not an
    // adapter-private response field, owns the detailed realization cause. The
    // resolved Agent snapshot still echoes accepted MCP config in every state;
    // Runtime activation is a separate fact and cannot erase API configuration.
    // Constraint K0: this preparation-only fixture commits no Run, so its one
    // atomic Thread recovery query returns None; unsupported production runtimes
    // remain fail-closed rather than falling back to split transcript reads.
    /// Rule, the preparation failure, its create status and error type.
    type Row = (&'static str, fn() -> RunError, StatusCode, &'static str);
    let rows: [Row; 5] = [
        (
            "R1",
            || RunError::classified("session_realization_invariant_violated", "prepare blew up"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
        ),
        (
            "R2",
            || RunError::bad_request("prepare refused"),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (
            "R3",
            || RunError::unavailable("environment image is not ready"),
            StatusCode::SERVICE_UNAVAILABLE,
            "api_error",
        ),
        (
            "R4",
            || RunError::internal("prepare blew up for an unknown reason"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "api_error",
        ),
        (
            "R5",
            || RunError::conflict("Session changed concurrently"),
            StatusCode::CONFLICT,
            "invalid_request_error",
        ),
    ];
    for (kind, failure, status, error_type) in rows {
        let h = harness(Some(failure)).await;
        let body = call(
            &h.app,
            "POST",
            "/v1/sessions",
            Some(session_with_mcp(
                "calc-agent",
                vec![json!({"type": "url", "name": "calc", "url": MCP_URL})],
                Vec::new(),
            )),
        )
        .await
        .expect_for(status, "expected status");
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], error_type);
        // Fail closed: no failed durable intent is exposed as a live Session.
        // Exact GET still projects the repository-owned aggregate and its cause.
        // The pre-I/O observation owns the generated opaque identity.
        let failed_id = h
            .observed_durable
            .lock()
            .unwrap()
            .last()
            .expect("durable intent observed before Runtime I/O")
            .session_id
            .clone();
        let read = call(&h.app, "GET", &format!("/v1/sessions/{failed_id}"), None)
            .await
            .expect(StatusCode::OK);
        let failed = h
            .repo
            .get(&failed_id)
            .await
            .expect("recoverable failed intent");
        if matches!(kind, "R3" | "R4" | "R5") {
            assert_eq!(read["status"], "rescheduling", "{kind}");
            assert_eq!(failed.execution, SessionExecutionState::Preparing, "{kind}");
            assert_eq!(failed.realization_progress().attempts(), 1, "{kind}");
            assert_eq!(
                failed.mcp.attachments[0].state,
                awaken_session_configuration_contract::mcp::McpAttachmentState::Realizing,
                "{kind}"
            );
        } else {
            assert_eq!(read["status"], "terminated", "R1/R2: {kind:?}");
            support::assert_current_sdk_session_shape(&read);
            assert_eq!(
                failed.execution,
                SessionExecutionState::ActivationFailed,
                "R1/R2: {kind:?}"
            );
            let expected_cause = format!("run failed: {}", failure().message);
            assert_eq!(
                failed.realization_progress().last_error(),
                Some(expected_cause.as_str()),
                "R1/R2 durable root retains the exact realization cause: {kind:?}"
            );
            assert_eq!(
                failed.mcp.attachments[0].state,
                awaken_session_configuration_contract::mcp::McpAttachmentState::Failed,
                "R1/R2: {kind:?}"
            );
            assert!(failed.visible_mcp_servers().is_empty(), "R1/R2: {kind:?}");
        }
        assert_eq!(
            read["agent"]["mcp_servers"],
            json!([{ "name": "calc", "type": "url", "url": MCP_URL }]),
            "{kind:?} accepted config is independent of Runtime visibility"
        );
    }
}

/// Causal graph for the public full-replacement command:
///
/// ```text
/// parse + normalize desired set
///   -> same recoverable fingerprint? --yes--> no-op
///   -> root-CAS Requested intent
///   -> root-CAS exact lease claim
///   -> stage + validate receipt
///      -> failure/mismatch: Failed; keep previous Active
///      -> success: activation CAS -> publish -> exact old drain -> Removed
/// ```
///
/// The cases below are generated from this decision table, in rule order:
///
/// | Rule | desired | current | stage | receipt | Effect |
/// |---|---|---|---|---|---|
/// | H1 | same | Active | - | - | convergent no-op; no new generation/effect |
/// | H2 | add | absent | success | exact | gen1 Active; publish |
/// | H3 | replace | Active gen1 | success | exact | gen2 Active; gen1 drain+Removed |
/// | H4 | remove | Active | - | - | exact drain+Removed; empty projection |
/// | H5 | replace | Active gen1 | failure | - | gen2 Failed; gen1 remains Active |
/// | H6 | retry same | Failed gen2 + Active gen1 | success | exact | gen3 Active |
/// | H7 | add | absent | success | stale | Failed; hidden; compensating drain |
/// | H8 | malformed | any | - | - | 400; no Runtime effect |
///
/// Constraint K0: this MCP-only fixture commits no Run, so every consistent
/// Thread recovery read is `None`; no rule may assemble a prefix from split reads.
#[tokio::test]
async fn hot_mcp_replacement_tests_are_generated_from_decision_table() {
    let h = hot_harness().await;
    let created = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "hot-agent"})),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    let server_a = json!({"name": "calc", "type": "url", "url": "https://a.example/mcp"});
    let added = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_a.clone()]}})),
    )
    .await
    .expect_for(StatusCode::OK, "H2");
    assert_eq!(
        added["agent"]["mcp_servers"],
        json!([server_a.clone()]),
        "H2"
    );
    {
        let state = h.state.lock().unwrap();
        assert_eq!(state.staged.len(), 1, "H2");
        assert_eq!(state.staged[0].generation.generation().version(), 1, "H2");
        assert_eq!(state.published.len(), 1, "H2");
        assert_eq!(
            state.durable_at_stage[0].mcp.attachments[0].state,
            awaken_session_configuration_contract::mcp::McpAttachmentState::Realizing,
            "H2 claim before I/O"
        );
    }

    let before = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    let replayed = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_a.clone()]}})),
    )
    .await
    .expect_for(StatusCode::OK, "H1");
    assert_eq!(replayed["agent"]["mcp_servers"], json!([server_a]), "H1");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            before,
            "H1"
        );
    }

    let server_b = json!({"name": "calc", "type": "url", "url": "https://b.example/mcp"});
    let replaced = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [server_b.clone()]}})),
    )
    .await
    .expect_for(StatusCode::OK, "H3");
    assert_eq!(replaced["agent"]["mcp_servers"], json!([server_b]), "H3");
    {
        let state = h.state.lock().unwrap();
        let version = |fence: Option<
            &awaken_session_configuration_contract::mcp::McpGenerationRef,
        >| { fence.map(|f| f.generation().version()) };
        let staged = state.staged.last().map(|request| &request.generation);
        assert_eq!(version(staged), Some(2), "H3 staged");
        assert_eq!(version(state.published.last()), Some(2), "H3 published");
        assert_eq!(version(state.drained.last()), Some(1), "H3 drained");
    }
    let durable = h.repo.get(&SessionId::from(id)).await.expect("H3 durable");
    assert_eq!(durable.visible_mcp_servers().len(), 1, "H3");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Removed,
        "H3"
    );
    assert_eq!(
        durable.mcp.attachments[1].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "H3"
    );

    let removed = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await
    .expect_for(StatusCode::OK, "H4");
    assert_eq!(removed["agent"]["mcp_servers"], json!([]), "H4");
    assert_eq!(
        h.state
            .lock()
            .unwrap()
            .drained
            .last()
            .unwrap()
            .generation()
            .version(),
        2,
        "H4"
    );

    let failed = hot_harness().await;
    let created = call(
        &failed.app,
        "POST",
        "/v1/sessions",
        Some(session_with_mcp(
            "hot-agent",
            vec![json!({"type": "url", "name": "calc", "url": "https://old.example/mcp"})],
            Vec::new(),
        )),
    )
    .await
    .expect(StatusCode::OK);
    let failed_id = created["id"].as_str().unwrap();
    failed.state.lock().unwrap().mode = HotStageMode::FailNext;
    let desired = json!({"name": "calc", "type": "url", "url": "https://new.example/mcp"});
    let _ = call(
        &failed.app,
        "POST",
        &format!("/v1/sessions/{failed_id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "H5");
    let durable = failed
        .repo
        .get(&SessionId::from(failed_id))
        .await
        .expect("H5 durable");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "H5"
    );
    assert_eq!(
        durable.mcp.attachments[1].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Failed,
        "H5"
    );
    assert_eq!(
        durable.visible_mcp_servers()[0].target.http_url().unwrap(),
        "https://old.example/mcp",
        "H5"
    );

    let retried = call(
        &failed.app,
        "POST",
        &format!("/v1/sessions/{failed_id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await
    .expect_for(StatusCode::OK, "H6");
    assert_eq!(retried["agent"]["mcp_servers"], json!([desired]), "H6");
    assert_eq!(
        failed
            .state
            .lock()
            .unwrap()
            .staged
            .last()
            .unwrap()
            .generation
            .generation()
            .version(),
        3,
        "H6"
    );

    let mismatch = hot_harness().await;
    let created = call(
        &mismatch.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "hot-agent"})),
    )
    .await
    .expect(StatusCode::OK);
    let mismatch_id = created["id"].as_str().unwrap();
    mismatch.state.lock().unwrap().mode = HotStageMode::MismatchNext;
    let _ = call(
        &mismatch.app,
        "POST",
        &format!("/v1/sessions/{mismatch_id}"),
        Some(
            json!({"agent": {"mcp_servers": [{"type": "url", "name": "bad", "url": "https://bad.example/mcp"}]}}),
        ),
    )
    .await.expect_for(StatusCode::INTERNAL_SERVER_ERROR, "H7");
    let durable = mismatch
        .repo
        .get(&SessionId::from(mismatch_id))
        .await
        .expect("H7 durable");
    assert_eq!(
        durable.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Failed,
        "H7"
    );
    assert!(durable.visible_mcp_servers().is_empty(), "H7");

    let effects = {
        let state = mismatch.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    let _ = call(
        &mismatch.app,
        "POST",
        &format!("/v1/sessions/{mismatch_id}"),
        Some(json!({"agent": {"mcp_servers": [{"type": "url", "name": "missing-url"}]}})),
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "H8");
    let state = mismatch.state.lock().unwrap();
    assert_eq!(
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len()
        ),
        effects,
        "H8"
    );
}

/// Recovery uses the same stage/publish/drain coordinator as create and hot
/// replacement. These cases are generated from the durable-state decision table:
///
/// ```text
/// repository recovery index -> nonterminal state
///   Requested/Realizing/Active -> new lease claim -> stage -> activate if needed -> publish
///   Draining -> exact idempotent drain -> Removed
///   Failed/Removed -> absent from index
/// ```
///
/// | Rule | Durable state | Runtime result | Effect |
/// |---|---|---|---|
/// | R1 | Requested | success | Active + published under new lease |
/// | R2 | Active + expired lease | success | restaged + republished; remains Active |
/// | R3 | Draining | success/unknown | Removed; no stage/publish |
/// | R4 | Removed | - | not selected; no Runtime effect |
/// | R5 | Requested on idle Session | stage failure | terminal Failed; Session remains idle |
/// | R6 | prior R5 Failed | same desired command | generation N+1 becomes Active |
///
/// Constraint K0: MCP realization recovery does not create a Run; the atomic
/// Thread recovery authority therefore reports `None` throughout R1-R6.
#[tokio::test]
async fn mcp_recovery_tests_are_generated_from_decision_table() {
    // Test-execution graph: C1 rules R1-R4 and R5-R6 have independent durable
    // fixtures; C2 bare `tokio::test` uses the default current-thread runtime;
    // C3 every rule calls the canonical Managed-to-Session recovery path. E1
    // both rule groups complete on the default stack. K1 the test-only scheduler
    // boundary owns no domain state: it splits independent decision-table rows
    // instead of adding a second recovery owner or a stack-size override.
    // Decision D1: C1+C2+C3 -> E1.
    tokio::spawn(mcp_recovery_lifecycle_decision_table_case())
        .await
        .expect("MCP recovery lifecycle decision table task");
    tokio::spawn(mcp_recovery_retry_decision_table_case())
        .await
        .expect("MCP recovery retry decision table task");
}

async fn mcp_recovery_lifecycle_decision_table_case() {
    let h = hot_harness().await;
    let created = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "recovery-agent"})),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let mut requested = h.repo.get(&SessionId::from(id)).await.unwrap();
    requested
        .mcp
        .request_full_replacement(
            vec![
                awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                    name: "docs".into(),
                    target: awaken_agent_contract::McpTarget::parse_http(
                        "https://docs.example/mcp",
                    )
                    .unwrap(),
                    credential: None,
                    prompts_as_skills: false,
                    origin:
                        awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
                },
            ],
            None,
        )
        .unwrap();
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        requested,
        "test:recovery-requested",
    )
    .await;

    assert_eq!(h.managed.reconcile_session_realizations().await, 1, "R1");
    let mut active = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "R1"
    );
    let first_epoch = active.realization.as_ref().unwrap().epoch;
    assert_eq!(h.state.lock().unwrap().published.len(), 1, "R1");

    active.realization.as_mut().unwrap().expires_at_unix_ms = 0;
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        active,
        "test:recovery-expired-lease",
    )
    .await;
    assert_eq!(h.managed.reconcile_session_realizations().await, 1, "R2");
    let active = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert!(
        active.realization.as_ref().unwrap().epoch > first_epoch,
        "R2"
    );
    assert_eq!(
        active.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "R2"
    );
    assert_eq!(h.state.lock().unwrap().published.len(), 2, "R2");

    let mut draining = active;
    let generation = draining.mcp.attachments[0].generation.clone();
    draining.mcp.begin_drain(&generation).unwrap();
    replace_session_fixture(
        h.repo.as_ref(),
        "default",
        draining,
        "test:recovery-draining",
    )
    .await;
    let staged_before = h.state.lock().unwrap().staged.len();
    assert_eq!(h.managed.reconcile_session_realizations().await, 1, "R3");
    let removed = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        removed.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Removed,
        "R3"
    );
    assert_eq!(h.state.lock().unwrap().staged.len(), staged_before, "R3");

    let effects = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };
    assert_eq!(h.managed.reconcile_session_realizations().await, 0, "R4");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            effects,
            "R4"
        );
    }
}

async fn mcp_recovery_retry_decision_table_case() {
    let h = hot_harness().await;

    let created = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "retry-agent"})),
    )
    .await
    .expect(StatusCode::OK);
    let retry_id = created["id"].as_str().unwrap();
    let mut retry = h.repo.get(&SessionId::from(retry_id)).await.unwrap();
    retry
        .mcp
        .request_full_replacement(
            vec![
                awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                    name: "retry".into(),
                    target: awaken_agent_contract::McpTarget::parse_http(
                        "https://retry.example/mcp",
                    )
                    .unwrap(),
                    credential: None,
                    prompts_as_skills: false,
                    origin:
                        awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
                },
            ],
            None,
        )
        .unwrap();
    replace_session_fixture(h.repo.as_ref(), "default", retry, "test:recovery-retry").await;
    h.state.lock().unwrap().mode = HotStageMode::FailNext;
    assert_eq!(h.managed.reconcile_session_realizations().await, 0, "R5");
    let failed_retry = h.repo.get(&SessionId::from(retry_id)).await.unwrap();
    assert_eq!(
        failed_retry.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Failed,
        "R5"
    );
    assert_eq!(failed_retry.execution, SessionExecutionState::Idle, "R5");

    let _ = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{retry_id}"),
        Some(json!({"agent": {"mcp_servers": [{
            "type": "url",
            "name": "retry",
            "url": "https://retry.example/mcp"
        }]}})),
    )
    .await
    .expect_for(StatusCode::OK, "R6");
    let retried = h.repo.get(&SessionId::from(retry_id)).await.unwrap();
    assert_eq!(retried.mcp.attachments.len(), 2, "R6");
    assert_eq!(
        retried.mcp.attachments[1].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "R6"
    );
    assert_eq!(retried.mcp.attachments[1].generation.version(), 2, "R6");
}

/// External acknowledgement gaps are derived from this cause graph:
///
/// ```text
/// activation CAS -> publish
///   fail -> Active + unacknowledged -> same desired retry restages/publishes
/// drain command
///   fail -> Draining -> same desired retry drains idempotently -> Removed
/// ```
///
/// | Rule | Durable state | Failed effect | Retry | Effect |
/// |---|---|---|---|---|
/// | P1 | Active/unacknowledged | publish | none | error; no false success |
/// | P2 | Active/unacknowledged | prior publish | same desired | same gen published+acknowledged |
/// | P3 | Draining | drain | none | error; cleanup remains durable |
/// | P4 | Draining | prior drain | same desired | exact drain replay + Removed |
///
/// Constraint K0: publication/drain effects commit no Run, so Managed observes
/// no atomic Thread snapshot and never consults split transcript/ticket reads.
#[tokio::test]
async fn publication_and_drain_gap_tests_are_generated_from_decision_table() {
    let h = hot_harness().await;
    let created = call(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "gap-agent"})),
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let desired = json!({"name": "gap", "type": "url", "url": "https://gap.example/mcp"});

    h.state.lock().unwrap().fail_publish_next = true;
    let _ = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "P1");
    let unacknowledged = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        unacknowledged.mcp.attachments[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "P1"
    );
    assert!(
        !unacknowledged.mcp.attachments[0].publication_acknowledged,
        "P1"
    );

    let recovered = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
    )
    .await
    .expect_for(StatusCode::OK, "P2");
    assert_eq!(recovered["agent"]["mcp_servers"], json!([desired]), "P2");
    let acknowledged = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(acknowledged.mcp.attachments.len(), 1, "P2 same generation");
    assert!(
        acknowledged.mcp.attachments[0].publication_acknowledged,
        "P2"
    );

    h.state.lock().unwrap().fail_drain_next = true;
    let _ = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "P3");
    assert_eq!(
        h.repo
            .get(&SessionId::from(id))
            .await
            .unwrap()
            .mcp
            .attachments[0]
            .state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Draining,
        "P3"
    );

    let removed = call(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
    )
    .await
    .expect_for(StatusCode::OK, "P4");
    assert_eq!(removed["agent"]["mcp_servers"], json!([]), "P4");
    assert_eq!(
        h.repo
            .get(&SessionId::from(id))
            .await
            .unwrap()
            .mcp
            .attachments[0]
            .state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Removed,
        "P4"
    );
}

/// HTTP command reliability is generated from this graph:
///
/// ```text
/// parse headers -> prior durable receipt?
///   same key/hash -> replay; different hash -> conflict
///   absent -> If-Match exact? -> bounded CAS command -> atomic final receipt
/// ```
///
/// | Rule | Key | Hash | If-Match | Effect |
/// |---|---|---|---|---|
/// | I1 | new | same | exact | apply once + ETag + internal durable receipt |
/// | I2 | same | same | omitted | replay; revision/effects unchanged |
/// | I2b | same | same | later root revision | replay receipt + current body/current root ETag |
/// | I2c | pre-upgrade namespace/hash | same | omitted | replay remains compatible |
/// | I3 | same | different | omitted | 409; no effect |
/// | I4 | absent | - | stale | 409 before Runtime effect |
/// | I5 | absent | - | malformed | 400 before Runtime effect |
/// | I6 | absent | - | exact | mutable tools persist in root aggregate |
///
/// Constraint K0: update commands mutate only Session policy/MCP state; the
/// no-Run fixture returns `None` from the sole atomic recovery boundary.
#[tokio::test]
async fn update_precondition_and_idempotency_tests_are_generated_from_decision_table() {
    let h = hot_harness().await;
    let (create_headers, created) = call_with_headers(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "idempotent-agent"})),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let etag = create_headers["etag"].to_str().unwrap().to_string();
    let desired = json!({"name": "idem", "type": "url", "url": "https://idem.example/mcp"});

    let before = h.state.lock().unwrap().staged.len();
    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("if-match", "\"0\"")],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "I4");
    assert_eq!(h.state.lock().unwrap().staged.len(), before, "I4");

    let (applied_headers, applied) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1"), ("if-match", &etag)],
    )
    .await
    .expect_for(StatusCode::OK, "I1");
    assert_eq!(
        applied["agent"]["mcp_servers"],
        json!([desired.clone()]),
        "I1"
    );
    assert!(applied_headers.contains_key("etag"), "I1");
    let legacy_title: Option<Option<String>> = None;
    let legacy_metadata: Option<Option<std::collections::BTreeMap<String, Option<String>>>> = None;
    let legacy_tools: Option<Vec<AgentTool>> = None;
    let legacy_mcp = Some(vec![
        serde_json::from_value::<awaken_protocol_managed::types::agent::AgentMcpServer>(
            desired.clone(),
        )
        .unwrap(),
    ]);
    let legacy_hash = awaken_agent_contract::stable_fingerprint(&(
        &legacy_title,
        &legacy_metadata,
        &legacy_tools,
        &legacy_mcp,
    ));
    let legacy_key = format!(
        "managed:update-command:{id}:{}",
        awaken_agent_contract::stable_fingerprint(&(id, "command-1"))
    );
    let legacy_receipt = h
        .repo
        .idempotency_receipt(&SessionId::from(id), &legacy_key)
        .await
        .expect("I2c receipt read")
        .expect("I2c stable pre-upgrade receipt namespace");
    assert_eq!(legacy_receipt.payload_hash, legacy_hash, "I2c");
    let applied_etag = applied_headers["etag"].clone();
    let effects = {
        let state = h.state.lock().unwrap();
        (
            state.staged.len(),
            state.published.len(),
            state.drained.len(),
        )
    };

    let (replay_headers, replayed) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1")],
    )
    .await
    .expect_for(StatusCode::OK, "I2");
    assert_eq!(
        replayed["agent"]["mcp_servers"],
        json!([desired.clone()]),
        "I2"
    );
    assert_eq!(replay_headers["etag"], applied_etag, "I2");
    {
        let state = h.state.lock().unwrap();
        assert_eq!(
            (
                state.staged.len(),
                state.published.len(),
                state.drained.len()
            ),
            effects,
            "I2"
        );
    }

    // I2b setup reads the current root fence. The command receipt remains the
    // application-layer idempotency authority, while every wire response pairs
    // its current body with the current root revision from one cache snapshot.
    let durable_after_replay = h
        .repo
        .get(&SessionId::from(id))
        .await
        .expect("I2b durable root");
    assert!(
        durable_after_replay.revision > legacy_receipt.committed_revision,
        "I2b realization advances beyond the command receipt"
    );
    assert_eq!(
        applied_headers["etag"].to_str().unwrap(),
        format!("\"{}\"", durable_after_replay.revision.0),
        "I2b response ETag names the current durable root"
    );
    let (current_headers, current) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[])
            .await
            .expect_for(StatusCode::OK, "I2b current root");
    assert_eq!(
        current_headers["etag"].to_str().unwrap(),
        format!("\"{}\"", durable_after_replay.revision.0),
        "I2b GET ETag names current durable root"
    );
    assert_eq!(
        current["agent"]["mcp_servers"],
        json!([desired.clone()]),
        "I2b current body"
    );
    let current_root_etag = current_headers["etag"].to_str().unwrap();
    let (later_headers, later) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "later root fact"})),
        &[("if-match", current_root_etag)],
    )
    .await
    .expect_for(StatusCode::OK, "I2b setup");
    assert_ne!(later_headers["etag"], applied_etag, "I2b setup");
    assert_eq!(later["title"], "later root fact", "I2b setup");
    let (historical_headers, historical) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[("idempotency-key", "command-1")],
    )
    .await
    .expect_for(StatusCode::OK, "I2b");
    assert_eq!(historical_headers["etag"], later_headers["etag"], "I2b");
    assert_eq!(historical["title"], "later root fact", "I2b current body");

    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": []}})),
        &[("idempotency-key", "command-1")],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "I3");

    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "invalid precondition"})),
        &[("if-match", "not-an-etag")],
    )
    .await
    .expect_for(StatusCode::BAD_REQUEST, "I5");

    let current_etag = later_headers["etag"].to_str().unwrap();
    // Cause graph: complete Session tool update -> durable exact projection ->
    // runtime policy/typed-client replacement -> disposable runtime rebuild on
    // the next Run.
    //
    // Decision table:
    // | update field | durable revision | runtime replacement |
    // | omitted      | unchanged        | none                |
    // | exact value with optional type omitted | incremented; canonical type emitted | full config once |
    // | replay       | original receipt | none duplicated     |
    // Constraint: optional discriminants are an input compatibility boundary;
    // durable and response projections use the one typed canonical value.
    let tools = json!([
        {
            "type": "agent_toolset_20260401",
            "configs": [{
                "name": "write",
                "enabled": false,
                "permission_policy": { "type": "always_allow" }
            }],
            "default_config": {
                "enabled": true,
                "permission_policy": { "type": "always_ask" }
            }
        },
        {
            "type": "custom",
            "name": "review_plan",
            "description": "Review the exact plan revision",
            "input_schema": {
                "type": "object",
                "properties": { "revision": { "type": "integer" } },
                "required": ["revision"]
            }
        }
    ]);
    let mut expected_tools = tools.clone();
    expected_tools[0]["configs"][0]["type"] = json!("write");
    let (_, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"tools": tools.clone()}})),
        &[("if-match", current_etag)],
    )
    .await
    .expect_for(StatusCode::OK, "I6");
    assert_eq!(updated["agent"]["tools"], expected_tools, "I6");
    assert_eq!(
        awaken_protocol_managed::project::managed_tools(
            &h.repo.get(&SessionId::from(id)).await.unwrap().tools
        )
        .expect("valid durable tool configuration"),
        serde_json::from_value::<Vec<AgentTool>>(expected_tools).unwrap(),
        "I6"
    );
    let state = h.state.lock().unwrap();
    assert_eq!(state.replaced_tools.len(), 1, "I6 runtime effect");
    assert_eq!(state.replaced_tools[0].0, id, "I6 exact Session");
    let write = state.replaced_tools[0].1.toolsets[0].policy_for("write");
    assert!(!write.enabled, "I6 exact disabled policy reaches runtime");
    assert_eq!(
        state.replaced_tools[0].1.client_tools[0].name,
        "review_plan"
    );
}

#[tokio::test]
async fn create_conflict_after_root_commit_returns_one_recoverable_session() {
    // Create-response cause/effect graph. C1 the public request has no key/a
    // stable key; C2 the root create receipt is committed; C3 every bounded
    // begin-realization CAS observes Conflict; C4 a stable-key retry has the
    // same/different payload. Effects: E1 the original POST returns the exact
    // durable Session instead of 503; E2 its DTO and ETag name one cached root
    // snapshot; E3 exact replay returns the same identity/snapshot without a
    // second root; E4 conflicting payload returns 409. Constraint: the route
    // never retries a non-idempotent POST; the application recovers only by its
    // repository-owned create receipt and leaves pending realization to the
    // canonical lifecycle driver.
    //
    // | Rule | Public key | Root receipt | Realization | Retry payload | Effect |
    // |---|---|---|---|---|---|
    // | P1 | absent | exact internal | conflict | - | E1+E2 |
    // | P2 | stable | exact public | conflict | same | E1+E2+E3 |
    // | P3 | stable | exact public | conflict | changed | E4 |
    for (rule, key) in [("P1", None), ("P2", Some("create-conflict"))] {
        let inner = support::session_repository("open create-conflict repository").await;
        let conflicts = Arc::new(ScheduledConflictRepository::new(inner));
        let h = hot_harness_with_repo(conflicts.clone()).await;
        conflicts.conflicts_on_next(&[1, 2, 3]);
        let headers = key.map_or_else(Vec::new, |key| vec![("idempotency-key", key)]);
        let (response_headers, created) = call_with_headers(
            &h.app,
            "POST",
            "/v1/sessions",
            Some(json!({"agent": "create-conflict-agent", "title": "stable"})),
            &headers,
        )
        .await
        .expect_for(StatusCode::OK, "/E1");
        let id = created["id"].as_str().expect("E1 Session id");
        let durable = h
            .repo
            .get(&SessionId::from(id))
            .await
            .expect("E1 durable root");
        assert_eq!(
            response_headers["etag"].to_str().unwrap(),
            format!("\"{}\"", durable.revision.0),
            "{rule}/E2"
        );
        assert_eq!(
            durable.execution,
            SessionExecutionState::Preparing,
            "{rule}/E1 canonical recovery remains pending"
        );

        if let Some(key) = key {
            let (replay_headers, replayed) = call_with_headers(
                &h.app,
                "POST",
                "/v1/sessions",
                Some(json!({"agent": "create-conflict-agent", "title": "stable"})),
                &[("idempotency-key", key)],
            )
            .await
            .expect_for(StatusCode::OK, "P2/E3");
            assert_eq!(replayed["id"], created["id"], "P2/E3");
            assert_eq!(replay_headers["etag"], response_headers["etag"], "P2/E3");

            let _ = call_with_headers(
                &h.app,
                "POST",
                "/v1/sessions",
                Some(json!({"agent": "create-conflict-agent", "title": "changed"})),
                &[("idempotency-key", key)],
            )
            .await
            .expect_for(StatusCode::CONFLICT, "P3/E4");
            assert_eq!(
                h.repo.get(&SessionId::from(id)).await.unwrap(),
                durable,
                "P3/E4"
            );
        }
    }
}

/// Post-commit Runtime projection FMECA. C1 durable tool policy commits; C2 the
/// first disposable Runtime replacement fails; C3 the client retries the exact
/// idempotency key/hash; C4 the key is reused with another payload. Effects:
/// E1 durable truth and one wire update fact survive C2; E2 C3 reprojects the
/// current durable policy without a second root mutation/event; E3 C4 remains a
/// conflict and cannot project another policy.
///
/// | Rule | Durable commit | Runtime effect | Retry | Payload | Effect |
/// |---|---|---|---|---|---|
/// | T1 | yes | fails | none | original | E1 + retryable error |
/// | T2 | already | succeeds | same key | same | E2 + success |
/// | T3 | already | n/a | same key | different | E3 |
///
/// Constraint K0: neither the durable policy mutation nor its disposable
/// projection creates a Run; the atomic Thread snapshot remains absent.
#[tokio::test]
async fn idempotent_update_repairs_a_failed_post_commit_runtime_projection() {
    let h = hot_harness().await;
    let (_, created) = call_with_headers(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "tool-repair-agent"})),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();
    let tools = json!([{
        "type": "agent_toolset_20260401",
        "configs": [],
        "default_config": {
            "enabled": true,
            "permission_policy": { "type": "always_ask" }
        }
    }]);
    h.state.lock().unwrap().fail_replace_tools_next = true;

    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"tools": tools.clone()}})),
        &[("idempotency-key", "tool-repair")],
    )
    .await
    .expect_for(StatusCode::INTERNAL_SERVER_ERROR, "T1");
    assert_eq!(h.state.lock().unwrap().replaced_tools.len(), 1, "T1");
    assert_eq!(
        awaken_protocol_managed::project::managed_tools(
            &h.repo.get(&SessionId::from(id)).await.unwrap().tools
        )
        .expect("valid durable tool configuration"),
        serde_json::from_value::<Vec<AgentTool>>(tools.clone()).unwrap(),
        "T1 durable truth"
    );

    let (_, retry) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"tools": tools.clone()}})),
        &[("idempotency-key", "tool-repair")],
    )
    .await
    .expect_for(StatusCode::OK, "T2");
    assert_eq!(retry["agent"]["tools"], tools, "T2");
    assert_eq!(h.state.lock().unwrap().replaced_tools.len(), 2, "T2");

    let events = call(&h.app, "GET", &format!("/v1/sessions/{id}/events"), None)
        .await
        .expect(StatusCode::OK);
    assert_eq!(
        events["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "session.updated")
            .count(),
        1,
        "T1/T2 project exactly one committed fact"
    );

    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "different-payload"})),
        &[("idempotency-key", "tool-repair")],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "T3");
    assert_eq!(h.state.lock().unwrap().replaced_tools.len(), 2, "T3");
}

/// CAS retry decisions are generated from `conflict phase × command fence`.
/// Causes are C1 the command root CAS conflicts or Applies, C2 `If-Match` is
/// absent or exact at request start, C3 a later realization CAS conflicts, and
/// C4 an idempotency key is present or absent. Effects are E1 bounded command
/// retry, E2 pre-commit 409, E3 post-commit 500 with current durable desired
/// truth, E4 exact receipt repair when C4 is present, and E5 compensation while
/// the public wire projection keeps the prior canonical visible/Active generation.
///
/// | Rule | Command CAS | If-Match | Later phase | Effect |
/// |---|---|---|---|---|
/// | C1 | conflict | absent | - | re-read, reapply same command, one generation/effect |
/// | C2 | conflict | exact at request start | - | return 409; do not weaken caller fence |
/// | C3 | applied + receipt | absent | publication-ack conflict | committed 500; exact receipt replay repairs same generation to 200 |
/// | C4 | applied | exact at request start | activation conflict | committed 500; retain desired/Realizing + drain once; GET keeps prior Active |
///
/// Constraints: `If-Match` fences only the command root CAS; a later projection
/// failure cannot roll back Applied truth or masquerade as a precondition 409.
/// Every rule is Session/MCP-only and commits no Run, so the authoritative
/// Thread recovery query returns `None` without a split fallback.
#[tokio::test]
async fn update_cas_retry_tests_are_generated_from_decision_table() {
    let inner = support::session_repository("open conflict repository").await;
    let conflicts = Arc::new(ScheduledConflictRepository::new(inner));
    let h = hot_harness_with_repo(conflicts.clone()).await;
    let (_, created) = call_with_headers(
        &h.app,
        "POST",
        "/v1/sessions",
        Some(json!({"agent": "cas-agent"})),
        &[],
    )
    .await
    .expect(StatusCode::OK);
    let id = created["id"].as_str().unwrap();

    conflicts.conflict_on_next(1);
    let desired = json!({"name": "cas", "type": "url", "url": "https://cas.example/mcp"});
    let (_, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [desired.clone()]}})),
        &[],
    )
    .await
    .expect_for(StatusCode::OK, "C1");
    assert_eq!(updated["agent"]["mcp_servers"], json!([desired]), "C1");
    assert_eq!(h.state.lock().unwrap().staged.len(), 1, "C1");
    assert_eq!(
        h.repo
            .get(&SessionId::from(id))
            .await
            .unwrap()
            .mcp
            .attachments
            .len(),
        1,
        "C1"
    );

    let (get_headers, _) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[])
            .await
            .expect(StatusCode::OK);
    let etag = get_headers["etag"].to_str().unwrap();
    let effects = h.state.lock().unwrap().staged.len();
    conflicts.conflict_on_next(1);
    let _ = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"title": "must-not-apply"})),
        &[("if-match", etag)],
    )
    .await
    .expect_for(StatusCode::CONFLICT, "C2");
    assert_eq!(h.state.lock().unwrap().staged.len(), effects, "C2");
    assert_ne!(
        h.repo
            .get(&SessionId::from(id))
            .await
            .unwrap()
            .title
            .as_deref(),
        Some("must-not-apply"),
        "C2"
    );

    let replacement = json!({"name": "cas", "type": "url", "url": "https://cas-2.example/mcp"});
    let c3_idempotency_key = "cas-publication-ack-repair";
    let c3_receipt_key = format!(
        "managed:update-command:{id}:{}",
        awaken_session_application::SessionUpdateCommands::update_operation_id(
            id,
            c3_idempotency_key
        )
    );
    let durable_before_c3 = h
        .repo
        .get(&SessionId::from(id))
        .await
        .expect("C3 root before command");
    conflicts.conflict_on_next(4);
    let staged_before = h.state.lock().unwrap().staged.len();
    let (failed_headers, failed) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [replacement.clone()]}})),
        &[("idempotency-key", c3_idempotency_key)],
    )
    .await
    .expect_for(
        StatusCode::INTERNAL_SERVER_ERROR,
        "C3 post-commit projection failure",
    );
    assert!(!failed_headers.contains_key("etag"), "C3 error has no ETag");
    assert_eq!(failed["type"], "error", "C3 error envelope");
    assert_eq!(failed["error"]["type"], "api_error", "C3 error class");
    assert_eq!(
        failed["error"]["message"], "Session changed concurrently",
        "C3 exact post-commit cause"
    );
    let committed_receipt = h
        .repo
        .idempotency_receipt(&SessionId::from(id), &c3_receipt_key)
        .await
        .expect("C3 receipt read")
        .expect("C3 command receipt committed before realization");
    assert_eq!(
        committed_receipt.committed_revision.0,
        durable_before_c3.revision.0 + 1,
        "C3 one command root CAS"
    );
    let durable_after_conflict = h
        .repo
        .get(&SessionId::from(id))
        .await
        .expect("C3 committed durable root");
    assert_eq!(
        durable_after_conflict.revision.0,
        committed_receipt.committed_revision.0 + 2,
        "C3 begin and activation precede the injected acknowledgement conflict"
    );
    assert_eq!(
        durable_after_conflict.mcp.attachments.len(),
        2,
        "C3 desired generation committed once"
    );
    assert_eq!(
        durable_after_conflict.mcp.attachments[1].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active,
        "C3 publication occurred before acknowledgement"
    );
    assert!(
        !durable_after_conflict.mcp.attachments[1].publication_acknowledged,
        "C3 acknowledgement remains recoverable"
    );
    assert_eq!(
        h.state.lock().unwrap().staged.len(),
        staged_before + 1,
        "C3 first attempt stages generation 2 once"
    );

    let (replay_headers, updated) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [replacement.clone()]}})),
        &[("idempotency-key", c3_idempotency_key)],
    )
    .await
    .expect_for(StatusCode::OK, "C3 exact receipt replay repairs");
    assert_eq!(updated["agent"]["mcp_servers"], json!([replacement]), "C3");
    let durable = h.repo.get(&SessionId::from(id)).await.unwrap();
    assert_eq!(
        replay_headers["etag"].to_str().unwrap(),
        format!("\"{}\"", durable.revision.0),
        "C3 replay pairs the current body with its current root revision"
    );
    assert_eq!(
        durable.mcp.attachments.len(),
        2,
        "C3 no duplicate generation"
    );
    assert!(durable.mcp.attachments[1].publication_acknowledged, "C3");
    assert_eq!(
        h.state.lock().unwrap().staged.len(),
        staged_before + 2,
        "C3 restages exact gen2"
    );

    let (get_headers, _) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[])
            .await
            .expect(StatusCode::OK);
    let etag = get_headers["etag"].to_str().unwrap();
    let durable_before_c4 = h
        .repo
        .get(&SessionId::from(id))
        .await
        .expect("C4 root before command");
    assert_eq!(
        etag,
        format!("\"{}\"", durable_before_c4.revision.0),
        "C4 exact If-Match names current command root"
    );
    conflicts.conflict_on_next(3);
    let staged_before_c4 = h.state.lock().unwrap().staged.len();
    let drain_before = h.state.lock().unwrap().drained.len();
    let c4_replacement = json!({"type": "url", "name": "cas", "url": "https://cas-3.example/mcp"});
    let (failed_headers, failed) = call_with_headers(
        &h.app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({"agent": {"mcp_servers": [c4_replacement.clone()]}})),
        &[("if-match", etag)],
    )
    .await
    .expect_for(
        StatusCode::INTERNAL_SERVER_ERROR,
        "C4 post-commit projection failure",
    );
    assert!(!failed_headers.contains_key("etag"), "C4 error has no ETag");
    assert_eq!(failed["type"], "error", "C4 error envelope");
    assert_eq!(failed["error"]["type"], "api_error", "C4 error class");
    assert_eq!(
        failed["error"]["message"], "Session changed concurrently",
        "C4 exact post-commit cause"
    );
    let durable = h.repo.get(&SessionId::from(id)).await.unwrap();
    let desired_after_c4 = durable.mcp.desired_attachments();
    assert_eq!(
        desired_after_c4.len(),
        1,
        "C4 one durable desired attachment"
    );
    assert_eq!(
        desired_after_c4[0].state,
        awaken_session_configuration_contract::mcp::McpAttachmentState::Realizing,
        "C4 desired generation remains recoverable"
    );
    let awaken_agent_contract::McpTarget::Http(desired_target) = &desired_after_c4[0].target else {
        panic!("C4 desired target must remain HTTP")
    };
    assert_eq!(
        desired_target.url, "https://cas-3.example/mcp",
        "C4 durable desired target is the committed replacement"
    );
    assert_eq!(
        durable.revision.0,
        durable_before_c4.revision.0 + 2,
        "C4 command and begin roots committed before activation conflict"
    );
    assert_eq!(
        h.state.lock().unwrap().staged.len(),
        staged_before_c4 + 1,
        "C4 staged the committed desired generation once"
    );
    assert_eq!(
        h.state.lock().unwrap().drained.len(),
        drain_before + 1,
        "C4 compensation"
    );
    let (current_headers, current) =
        call_with_headers(&h.app, "GET", &format!("/v1/sessions/{id}"), None, &[])
            .await
            .expect_for(StatusCode::OK, "C4 GET current truth");
    assert_eq!(
        current_headers["etag"].to_str().unwrap(),
        format!("\"{}\"", durable.revision.0),
        "C4 GET ETag names current durable root"
    );
    assert_eq!(
        current["agent"]["mcp_servers"],
        json!([{"type": "url", "name": "cas", "url": "https://cas-3.example/mcp"}]),
        "C4 GET returns the committed desired config while activation recovers"
    );
}

/// A fresh process restarts the session sequence at 0, but the store may hold
/// committed truth from a previous process. Minting must skip such ids: a NEW
/// session must never graft onto an old thread's transcript (rehydration by
/// explicit id stays the only reattach path).
#[tokio::test]
async fn minting_namespace_cannot_alias_committed_truth() {
    // Causes: the fixtures below establish `minting namespace` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `cannot alias committed truth` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `minting namespace` is one independent branch selecting `cannot alias
    // committed truth`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    use awaken_agent_contract::agent::content::ContentBlock;

    struct HauntedRuntime;
    #[async_trait::async_trait]
    impl SessionRuntime for HauntedRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("no Run in this test")
        }
        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: ToolPermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<ContentBlock>,
            _is_error: bool,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        fn model(&self) -> String {
            "haunted".into()
        }
    }

    awaken_session_execution_contract::neutral_role!(CoordinationRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(BudgetRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for HauntedRuntime);

    awaken_session_execution_contract::neutral_role!(CapabilityRuntime for HauntedRuntime);

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::RealizationRuntime for HauntedRuntime {
        fn observe_realization_request(
            &self,
        ) -> Result<
            awaken_lease_contract::authority::AuthorityProofStarted,
            awaken_session_types::RunError,
        > {
            awaken_protocol_managed_testkit::observe_realization_request()
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
            thread: &str,
            projection: awaken_session_realization_contract::FrozenSessionProjection,
            mode: awaken_session_realization_contract::SessionProjectionInstallMode,
        ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure>
        {
            awaken_protocol_managed_testkit::complete_session_projection_init(
                thread,
                &projection,
                &mode,
            )?;
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
            lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        ) -> Result<(), awaken_session_types::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .invalidate_session_realization_lease(thread, lease)
                .await
        }
    }

    awaken_session_execution_contract::neutral_role!(RealizationEffects for HauntedRuntime);

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::ThreadReadRuntime for HauntedRuntime {
        async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
            // A previous process persisted threads sesn_0 and sesn_1.
            Ok(thread == "sesn_0" || thread == "sesn_1")
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// The committed transcript for `thread`, in commit order. Used to rehydrate a
        /// session whose in-memory record was lost (e.g. after a process restart) from
        /// durable truth: a non-empty result means the thread exists in the store. The
        /// default reports nothing, so an ephemeral host never rehydrates.
        async fn committed_messages(
            &self,
            thread: &str,
        ) -> Result<
            Vec<awaken_agent_contract::agent::message::Message>,
            awaken_session_types::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .committed_messages(thread)
                .await
        }

        async fn session_thread_recovery_snapshot(
            &self,
            _session_id: &SessionId,
            _thread_id: &str,
        ) -> Result<Option<RunRecoverySnapshot>, RunError> {
            // `owns_thread` is the only durable evidence in this minting case;
            // it deliberately exposes no committed Run prefix.
            Ok(None)
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Read one exact committed Run through the same internally consistent
        /// logical-Thread prefix. Settlement already owns the immutable Run id from
        /// its guarded dispatch and must not rediscover it through a process-local
        /// "latest Run" projection.
        ///
        /// The compatibility default filters the existing Thread snapshot. Shared
        /// durable adapters override this method so a cold replica can query its
        /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
        /// directly by `run_id`.
        async fn session_thread_run_recovery_snapshot(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
            run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            awaken_session_types::RunError,
        > {
            awaken_session_execution_contract::select_recovery_snapshot_run(
                self.session_thread_recovery_snapshot(session_id, thread_id)
                    .await?,
                run_id,
            )
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Read the durable disposition of one logical child Thread through its
        /// parent Session's physical commit partition. Legacy Threads without the
        /// state cell remain Active.
        async fn session_thread_disposition(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
        ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_types::RunError>
        {
            awaken_session_execution_contract::NeutralRuntime
                .session_thread_disposition(session_id, thread_id)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Read one exact terminal Outcome projection from committed Thread truth.
        /// `None` means the id is absent or still active. Implementations must not
        /// resume the Outcome or reconstruct a terminal from an in-process
        /// [`awaken_session_application_contract::StepOutcome`].
        async fn committed_outcome_projection(
            &self,
            thread: &str,
            outcome_id: &str,
        ) -> Result<
            Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
            awaken_session_types::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .committed_outcome_projection(thread, outcome_id)
                .await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::OutcomeRuntime for HauntedRuntime {
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
        ) -> Result<OutcomeDrive, RunError> {
            unreachable!()
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
            awaken_session_types::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .continue_outcome(thread)
                .await
        }
    }

    let state = support::state(HauntedRuntime).await;
    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                budget: None,
                initial_events: Vec::new(),
                environment_id: "env_local".into(),
                title: None,
                metadata: Default::default(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            awaken_tenancy::DEFAULT_WORKSPACE_ID,
        )
        .await
        .expect("create skips haunted ids");
    assert!(session.id.starts_with("sesn_fnv1a64:"));
    assert_ne!(session.id, "sesn_0");
    assert_ne!(session.id, "sesn_1");
}

/// ADR-0048 / S10: Session creation commits the lifecycle fact and Session in
/// one repository transaction, then emits only a payload-free replay hint.
#[tokio::test]
async fn create_session_commits_the_owned_fact_then_notifies_once() {
    // Decision rule L1: C1 creation reaches idle and C2 repository commit
    // succeeds -> E1 one stable owned fact exists and E2 one payload-free wake
    // occurs. The repository, never the notifier, owns fact content.
    let (state, repository, notifier) = lifecycle_test_state().await;

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                budget: None,
                initial_events: Vec::new(),
                environment_id: "env_local".into(),
                title: None,
                metadata: Default::default(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            "wrkspc_acme",
        )
        .await
        .expect("create session");

    let facts = repository.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), 1, "L1/E1");
    assert_eq!(facts[0].object_id, session.id, "L1/E1");
    assert_eq!(
        facts[0].workspace_id.as_deref(),
        Some("wrkspc_acme"),
        "L1/E1"
    );
    assert_eq!(facts[0].event_type, "session.status_idled", "L1/E1");
    assert_eq!(notifier.count(), 1, "L1/E2");
}

/// Archive is idempotent at the aggregate/outbox transaction boundary.
// Test design: archive_session_commits_the_terminated_fact_once
// Cause/effect graph: archive commits one terminal fact and retires only owned runtime resources.
// Decision table: eligible=archive once; archived=replay; running=conflict; deleted/missing=404.
#[tokio::test]
async fn archive_session_commits_the_terminated_fact_once() {
    // Decision rules: L2 C1 first archive -> E1 one terminated fact and one
    // wake; L3 C2 repeat archive of the terminated aggregate -> E2 no second
    // fact and no second wake. Create's idled fact remains independently stable.
    // Constraint K0: this lifecycle/outbox fixture has no Run, so archive sees
    // no atomic Thread recovery snapshot and cannot invent split recovery truth.
    let (state, repository, notifier) = lifecycle_test_state().await;

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                budget: None,
                initial_events: Vec::new(),
                environment_id: "env_local".into(),
                title: None,
                metadata: Default::default(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            "wrkspc_acme",
        )
        .await
        .expect("create session");

    // First archive → the terminated fact; second archive → nothing new.
    state.archive_session(&session.id).await.expect("archive");
    state
        .archive_session(&session.id)
        .await
        .expect("re-archive is idempotent");

    let facts = repository.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), 2, "L2/E1 + L3/E2");
    let terminated = facts
        .iter()
        .filter(|fact| fact.event_type == "session.status_terminated")
        .collect::<Vec<_>>();
    assert_eq!(terminated.len(), 1, "L3/E2");
    assert_eq!(terminated[0].object_id, session.id, "L2/E1");
    assert_eq!(
        terminated[0].workspace_id.as_deref(),
        Some("wrkspc_acme"),
        "L2/E1"
    );
    assert_eq!(
        notifier.count(),
        2,
        "one wake for create and one for archive"
    );
}

/// Delete commits the terminal visibility fact before the aggregate disappears.
// Test design: delete_session_commits_the_deleted_fact_with_the_owner
// Cause/effect graph: owner fencing commits deletion before removal and prevents later resurrection.
// Decision table: correct owner+eligible=delete; wrong/stale=conflict; repeated/missing=404.
#[tokio::test]
async fn delete_session_commits_the_deleted_fact_with_the_owner() {
    // Decision rule L4: C1 an owned active Session is deleted -> E1 its stable
    // deleted fact remains in the transactionally durable outbox after the row
    // becomes unavailable and E2 one post-commit wake is added to create's wake.
    // Constraint: repository transaction owns the fact and deletion; notifier
    // remains a payload-free post-commit hint and cannot recreate the aggregate.
    let (state, repository, notifier) = lifecycle_test_state().await;

    let session = state
        .create_session(
            awaken_protocol_managed::types::SessionCreateParams {
                agent: awaken_protocol_managed::types::AgentRef::Id("assistant".into()),
                budget: None,
                initial_events: Vec::new(),
                environment_id: "env_local".into(),
                title: None,
                metadata: Default::default(),
                vault_ids: Vec::new(),
                resources: Vec::new(),
            },
            "wrkspc_acme",
        )
        .await
        .expect("create session");

    state.delete_session(&session.id).await.expect("delete");

    let facts = repository.pending_lifecycle().await.unwrap();
    assert_eq!(facts.len(), 2, "L4/E1");
    let deleted = facts
        .iter()
        .find(|fact| fact.event_type == "session.deleted")
        .expect("deleted fact remains durable");
    assert_eq!(deleted.object_id, session.id, "L4/E1");
    assert_eq!(
        deleted.workspace_id.as_deref(),
        Some("wrkspc_acme"),
        "L4/E1"
    );
    assert_eq!(notifier.count(), 2, "L4/E2");
}
