// @formal evidence session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// awaken-allow(file-limit-ceiling): 5239 (legacy oversized test file: split, never grow)
use awaken_session_execution_contract::{
    SessionCoordinationReads as _, StreamingRunRuntime as _, ThreadReadRuntime as _,
};
use awaken_session_types::SessionId;
#[macro_use]
mod no_committed_threads;
mod application_fixture;
use application_fixture::{
    application, application_with_configuration, application_with_persistence,
    application_with_runtime, configured_admission_application, configured_application,
};
mod observation_refinement;
mod observed_commit;
mod recording_cleanup_terminal;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use awaken_executable_environment_contract::EnvironmentImageBuildError;
use awaken_service_lifecycle::run_composed_async_test;
use awaken_session_repository_contract::SessionRepository;
use awaken_session_execution_contract::{McpAttachmentRealizer, SessionRuntime};
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::RunError;
use awaken_time::system_clock;
use {
    awaken_session_configuration_contract::SandboxProvisioning,
    awaken_session_types::SessionRuntimePlacement,
};

use super::*;
use awaken_session_repository_contract::SessionRepositoryReads;

/// Execute the canonical complete-projection effects for a Session Application
/// test double. Domain mode decisions come from `SessionProjectionInstallMode`;
/// individual fakes override only the effect they need to observe.
async fn install_complete_test_projection<
    R: awaken_session_execution_contract::EnvironmentRuntime
        + awaken_session_execution_contract::RealizationEffects
        + ?Sized,
>(
    runtime: &R,
    thread: &str,
    projection: awaken_session_realization_contract::FrozenSessionProjection,
    mode: awaken_session_realization_contract::SessionProjectionInstallMode,
) -> Result<(), RunError> {
    if thread.is_empty()
        || projection.workspace_id.is_empty()
        || projection.baseline.agent_id.is_empty()
    {
        return Err(RunError::bad_request(
            "test Session projection must carry complete frozen coordinates",
        ));
    }
    // Cause/effect rule P1: a complete frozen projection carries the aggregate's
    // previous and desired Resource generations; the test Runtime must observe
    // exactly that canonical transition through the production two-argument
    // port, with no test-only reconstruction or legacy compatibility path.
    let transition = projection.resource_transition(
        awaken_session_realization_contract::FrozenResourceTransitionUse::ApplyEffects,
    )?;
    runtime.apply_session_inputs(thread, &transition).await?;
    if mode.adopts_resident_environment()
        && let Some(binding) = projection.environment.binding()
    {
        runtime
            .adopt_session_environment(&projection.baseline.agent_id, thread, binding)
            .await?;
    }
    Ok(())
}

struct NoopRuntime;

/// Application fakes model protocol ordering, not the platform suspend clock.
/// One explicit process-local clock supplies advancing request observations;
/// production Runtime's unsupported default must remain fail-closed.
pub(crate) fn test_realization_request_started()
-> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
    static ORIGIN: std::sync::OnceLock<awaken_time::mono::Instant> = std::sync::OnceLock::new();
    let origin = *ORIGIN.get_or_init(awaken_time::mono::now);
    let now = awaken_time::mono::now();
    Ok(
        awaken_lease_contract::authority::AuthorityProofStarted::from_clock_sample(
            now,
            now.duration_since(origin),
        ),
    )
}

#[derive(Default)]
struct ToggleProjectionRefresh {
    calls: AtomicUsize,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::ExecutableAgentProjectionRefresh
    for ToggleProjectionRefresh
{
    async fn refresh_agent_projection(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err("projection unavailable".into())
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::ExecutableEnvironmentProjectionRefresh
    for ToggleProjectionRefresh
{
    async fn refresh_environment_projection(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err("projection unavailable".into())
        } else {
            Ok(())
        }
    }
}

fn paired_projection_refresh(
    refresh: Arc<ToggleProjectionRefresh>,
) -> Arc<awaken_session_application_contract::ExecutableProjectionRefresh> {
    Arc::new(
        awaken_session_application_contract::ExecutableProjectionRefresh::new(
            refresh.clone(),
            refresh,
        ),
    )
}

#[derive(Clone, Copy)]
enum ReplyRuntimeOutcome {
    Accepted,
    BadRequest,
    Unavailable,
}

struct RecordingReplyRuntime {
    activity: awaken_session_execution_contract::SessionThreadToolReplyActivity,
    outcome: ReplyRuntimeOutcome,
    observed_sinks: AtomicUsize,
    deliveries: Mutex<Vec<awaken_session_execution_contract::SessionThreadToolReplyDelivery>>,
    boundaries: RecordingBoundaries,
}

struct RecordingBoundaryBudgetRuntime {
    admission_observations: Mutex<
        VecDeque<
            Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError>,
        >,
    >,
    admission_reads: AtomicUsize,
    root_usage: Mutex<awaken_session_budget_contract::SessionUsage>,
    child_usage: Mutex<awaken_session_budget_contract::SessionUsage>,
    child_target: awaken_session_execution_contract::CoordinatedThreadTarget,
    continuations: AtomicUsize,
    usage_relationship_reads: AtomicUsize,
    fail_usage_relationship_read: AtomicBool,
    usage_thread_reads: Mutex<Vec<String>>,
    boundaries: RecordingBoundaries,
    budget_resume_tickets:
        Mutex<HashMap<String, Vec<awaken_session_application_contract::SessionBudgetResumeTicket>>>,
    budget_resume_deliveries:
        Mutex<Vec<awaken_session_application_contract::SessionBudgetResumeDelivery>>,
    budget_resume_dispositions:
        Mutex<VecDeque<awaken_session_application_contract::SessionBudgetResumeDisposition>>,
}

#[derive(Clone, Copy)]
enum AgentAdmissionOutcome {
    Accepted,
    BadRequest,
    Unavailable,
    MismatchedReceipt,
}

struct RecordingAgentAdmissionRuntime {
    admission_observations: Mutex<
        VecDeque<
            Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError>,
        >,
    >,
    admission_reads: AtomicUsize,
    outcomes: Mutex<VecDeque<AgentAdmissionOutcome>>,
    admissions: Mutex<Vec<awaken_session_execution_contract::CoordinatedRunCommand>>,
    committed_links: Mutex<Vec<awaken_session_execution_contract::CoordinatedThreadLink>>,
    archived_threads: Mutex<BTreeSet<(String, String)>>,
    continuations: Mutex<Vec<awaken_session_execution_contract::SessionAgentReportContinuation>>,
    interruptions: Mutex<Vec<(String, awaken_agent_contract::agent::thread::Id)>>,
    boundaries: RecordingBoundaries,
    generic_recovery_available: AtomicBool,
    lifecycle_visible: AtomicBool,
    source_valid: AtomicBool,
    installed_projections: Mutex<Vec<awaken_session_realization_contract::FrozenSessionProjection>>,
    projection_failure: AtomicBool,
}

type RecordedBoundaryKey = (String, String);
#[derive(Default)]
struct RecordingBoundaries(
    Mutex<
        HashMap<
            RecordedBoundaryKey,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        >,
    >,
);

impl RecordingBoundaries {
    fn commit(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        state: awaken_agent_contract::agent::run::RunState,
        report: impl Into<String>,
    ) {
        let commit_cursor = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|((candidate_session, _), _)| candidate_session == session_id)
            .map(|(_, snapshot)| snapshot.store_cursor)
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        let report = report.into();
        let messages: Vec<awaken_agent_contract::agent::message::Message> = (!report.is_empty())
            .then(|| {
                awaken_agent_contract::agent::message::Message::text(
                    awaken_agent_contract::agent::message::Id::assistant(run_id, 0),
                    awaken_agent_contract::agent::message::Role::Assistant,
                    report,
                )
            })
            .into_iter()
            .collect();
        let message_commit_cursors = (!messages.is_empty())
            .then_some(commit_cursor)
            .into_iter()
            .collect();
        // The fixture commit carries its terminal audit alongside Run state and
        // Messages. A separately synthesized feed is not another fact authority.
        let draft: awaken_agent_contract::audit::draft::Draft =
            awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                state: state.clone(),
                await_reason: None,
                await_target: None,
            }
            .into();
        let events = vec![awaken_agent_contract::audit::record::Record {
            sequence: awaken_agent_contract::encode_run_lifecycle_cursor(commit_cursor, 0)
                .unwrap()
                .0,
            run_id: run_id.clone(),
            kind: draft.kind,
            payload: draft.payload,
        }];
        self.set_recovery_snapshot(
            session_id,
            thread_id,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: thread_id.clone(),
                claimed_run_id: run_id.clone(),
                runs: vec![awaken_agent_contract::agent::run::Record {
                    id: run_id.clone(),
                    thread_id: thread_id.clone(),
                    state,
                }],
                latest_run_id: Some(run_id.clone()),
                messages,
                message_commit_cursors,
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events,
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: commit_cursor,
                next_commit_ordinal: 1,
            },
        );
    }

    fn set_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        snapshot: awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) {
        assert_eq!(snapshot.thread_id, *thread_id, "test recovery Thread");
        self.0
            .lock()
            .unwrap()
            .insert((session_id.to_string(), thread_id.0.clone()), snapshot);
    }

    fn snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot> {
        self.0
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            .cloned()
    }
}

impl RecordingAgentAdmissionRuntime {
    fn set_admission_observations(
        &self,
        observations: impl IntoIterator<
            Item = Result<
                Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
                RunError,
            >,
        >,
    ) {
        // Explicit observer answers are independent of attempted admissions:
        // an unavailable acknowledgement cannot establish accepted or Missing.
        *self.admission_observations.lock().unwrap() = observations.into_iter().collect();
    }

    fn new(outcomes: impl IntoIterator<Item = AgentAdmissionOutcome>) -> Self {
        Self {
            admission_observations: Mutex::new(VecDeque::new()),
            admission_reads: AtomicUsize::new(0),
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            admissions: Mutex::new(Vec::new()),
            committed_links: Mutex::new(Vec::new()),
            archived_threads: Mutex::new(BTreeSet::new()),
            continuations: Mutex::new(Vec::new()),
            interruptions: Mutex::new(Vec::new()),
            boundaries: RecordingBoundaries::default(),
            generic_recovery_available: AtomicBool::new(true),
            lifecycle_visible: AtomicBool::new(true),
            source_valid: AtomicBool::new(true),
            installed_projections: Mutex::new(Vec::new()),
            projection_failure: AtomicBool::new(false),
        }
    }

    fn commit_link(&self, link: awaken_session_execution_contract::CoordinatedThreadLink) {
        self.committed_links.lock().unwrap().push(link);
    }

    fn archive_thread(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
    ) {
        self.archived_threads
            .lock()
            .unwrap()
            .insert((session_id.to_string(), thread_id.0.clone()));
    }

    fn set_lifecycle_visible(&self, visible: bool) {
        self.lifecycle_visible.store(visible, Ordering::SeqCst);
    }

    fn observed_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot> {
        let mut snapshot = self.boundaries.snapshot(session_id, thread_id)?;
        if !self.lifecycle_visible.load(Ordering::SeqCst) {
            snapshot.events.clear();
        }
        Some(snapshot)
    }
}

struct CoordinatedAgentSource;

impl awaken_executable_agent_contract::ExecutableAgentProfileSource for CoordinatedAgentSource {
    fn session_profile_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        let revision = match agent_id {
            "coord-root" => 1,
            "coord-child" => 2,
            _ => return None,
        };
        self.session_profile_at_revision_in(workspace_id, agent_id, revision)
    }

    fn session_profile_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        if workspace_id != "workspace" {
            return None;
        }
        match (agent_id, source_revision) {
            ("coord-root", 1) => Some(
                awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                    name: Some("Coordinator".into()),
                    source_revision,
                    delegates: vec![awaken_executable_agent_contract::ExecutableAgentDelegate {
                        agent_id: "coord-child".into(),
                        source_revision: Some(2),
                    }],
                    ..Default::default()
                },
            ),
            ("coord-child", 2) => Some(
                awaken_executable_agent_contract::ExecutableAgentSessionProfile {
                    name: Some("Researcher".into()),
                    source_revision,
                    ..Default::default()
                },
            ),
            _ => None,
        }
    }

    fn executable_snapshot_at_revision_in(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.session_profile_at_revision_in(workspace_id, agent_id, source_revision)?;
        let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder(agent_id)
            .model(awaken_runtime_contract::resolved::ModelBinding::new(
                "test", "model", "native",
            ))
            .fingerprint(format!("{agent_id}-revision-{source_revision}"))
            .build();
        snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
        snapshot.metadata.source.revision = source_revision;
        Some(snapshot)
    }
}

impl RecordingBoundaryBudgetRuntime {
    fn set_admission_observations(
        &self,
        observations: impl IntoIterator<
            Item = Result<
                Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
                RunError,
            >,
        >,
    ) {
        // Explicit observer answers are independent of attempted admissions:
        // an unavailable acknowledgement cannot establish accepted or Missing.
        *self.admission_observations.lock().unwrap() = observations.into_iter().collect();
    }

    fn new(child_usage: awaken_session_budget_contract::SessionUsage) -> Self {
        Self {
            admission_observations: Mutex::new(VecDeque::new()),
            admission_reads: AtomicUsize::new(0),
            root_usage: Mutex::new(Default::default()),
            child_usage: Mutex::new(child_usage),
            child_target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
                agent_id: "coord-child".into(),
            },
            continuations: AtomicUsize::new(0),
            usage_relationship_reads: AtomicUsize::new(0),
            fail_usage_relationship_read: AtomicBool::new(false),
            usage_thread_reads: Mutex::new(Vec::new()),
            boundaries: RecordingBoundaries::default(),
            budget_resume_tickets: Mutex::new(HashMap::new()),
            budget_resume_deliveries: Mutex::new(Vec::new()),
            budget_resume_dispositions: Mutex::new(VecDeque::new()),
        }
    }

    fn with_child_target(
        mut self,
        child_target: awaken_session_execution_contract::CoordinatedThreadTarget,
    ) -> Self {
        self.child_target = child_target;
        self
    }

    fn set_root_usage(&self, usage: awaken_session_budget_contract::SessionUsage) {
        *self.root_usage.lock().unwrap() = usage;
    }

    fn set_child_usage(&self, usage: awaken_session_budget_contract::SessionUsage) {
        *self.child_usage.lock().unwrap() = usage;
    }

    fn set_budget_resume_tickets(
        &self,
        session_id: &str,
        tickets: Vec<awaken_session_application_contract::SessionBudgetResumeTicket>,
    ) {
        self.budget_resume_tickets
            .lock()
            .unwrap()
            .insert(session_id.to_string(), tickets);
    }

    fn set_budget_resume_dispositions(
        &self,
        dispositions: impl IntoIterator<
            Item = awaken_session_application_contract::SessionBudgetResumeDisposition,
        >,
    ) {
        *self.budget_resume_dispositions.lock().unwrap() = dispositions.into_iter().collect();
    }
}

impl RecordingAgentAdmissionRuntime {
    fn commit_boundary(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        state: awaken_agent_contract::agent::run::RunState,
        report: impl Into<String>,
    ) {
        self.boundaries
            .commit(session_id, thread_id, run_id, state, report);
    }

    fn set_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        snapshot: awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    ) {
        self.boundaries
            .set_recovery_snapshot(session_id, thread_id, snapshot);
    }

    fn set_generic_recovery_available(&self, available: bool) {
        self.generic_recovery_available
            .store(available, Ordering::SeqCst);
    }

    fn reject_agent_message_source(&self) {
        self.source_valid.store(false, Ordering::SeqCst);
    }
}

impl RecordingBoundaryBudgetRuntime {
    fn commit_boundary(
        &self,
        session_id: &str,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        run_id: &awaken_agent_contract::agent::run::Id,
        state: awaken_agent_contract::agent::run::RunState,
        report: impl Into<String>,
    ) {
        self.boundaries
            .commit(session_id, thread_id, run_id, state, report);
    }
}

impl RecordingReplyRuntime {
    fn new(outcome: ReplyRuntimeOutcome) -> Self {
        Self {
            activity: awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
                prior_epoch: Some(1),
            },
            outcome,
            observed_sinks: AtomicUsize::new(0),
            deliveries: Mutex::new(Vec::new()),
            boundaries: RecordingBoundaries::default(),
        }
    }
}

#[derive(Default)]
struct RecordingCleanupRuntime {
    source_observation: Mutex<awaken_session_execution_contract::SessionSourceDispatchObservation>,
    fail_quiesce_once: AtomicBool,
    fail_before_effect_once: AtomicBool,
    fail_once: AtomicBool,
    fail_publication_once: AtomicBool,
    intents: Mutex<Vec<awaken_session_state_contract::SessionCleanupCommand>>,
    effective_ids: Mutex<BTreeSet<String>>,
    block_quiesce: AtomicBool,
    quiesce_entered: tokio::sync::Notify,
    quiesce_release: tokio::sync::Notify,
    delegated_snapshot: Mutex<awaken_session_application_contract::DelegatedRunSnapshot>,
    publication_intents:
        Mutex<Vec<awaken_session_state_contract::SessionRepositoryPublicationCommand>>,
    reject_publication: AtomicBool,
    close_publication_source: AtomicBool,
    terminal_effect_order: Mutex<Vec<String>>,
    terminal_assignments:
        Mutex<Vec<awaken_session_realization_contract::SessionTerminalCleanupAssignment>>,
    acknowledged_effects: Mutex<Vec<awaken_session_state_contract::SessionTerminalCleanupEffect>>,
    acknowledged_completed: Mutex<Vec<(String, crate::session_resource::SessionRealizationLease)>>,
    ordinary_resource_apply_calls: AtomicUsize,
}

#[derive(Default)]
struct RecordingReferenceIndex {
    fail_replace: AtomicBool,
    records: Mutex<BTreeSet<awaken_resource_contract::ResourceReferenceRecord>>,
}

#[async_trait::async_trait]
impl awaken_resource_contract::ResourceReferenceIndex for RecordingReferenceIndex {
    async fn add_reference(
        &self,
        record: awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.records.lock().unwrap().insert(record))
    }

    async fn remove_reference(
        &self,
        record: &awaken_resource_contract::ResourceReferenceRecord,
    ) -> Result<bool, awaken_resource_contract::ResourcePurgeError> {
        Ok(self.records.lock().unwrap().remove(record))
    }

    async fn replace_references(
        &self,
        kind: awaken_resource_contract::ResourceReferenceKind,
        reference_id: &str,
        records: Vec<awaken_resource_contract::ResourceReferenceRecord>,
    ) -> Result<(), awaken_resource_contract::ResourcePurgeError> {
        if self.fail_replace.load(Ordering::SeqCst) {
            return Err(
                awaken_resource_contract::ResourcePurgeError::Indeterminate {
                    detail: "injected reference projection failure".into(),
                },
            );
        }
        let mut current = self.records.lock().unwrap();
        current.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        current.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &awaken_resource_contract::ResourceTarget,
    ) -> Result<
        Vec<awaken_resource_contract::ResourceReference>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| &record.target == target)
            .map(|record| record.reference.clone())
            .collect())
    }

    async fn references_for_resource(
        &self,
        kind: awaken_resource_contract::ResourceKind,
        resource_id: &str,
    ) -> Result<
        Vec<awaken_resource_contract::ResourceReferenceRecord>,
        awaken_resource_contract::ResourcePurgeError,
    > {
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

struct UnusedFileCatalog;

#[async_trait::async_trait]
impl awaken_resource_contract::FileCatalog for UnusedFileCatalog {
    async fn artifact_capture(
        &self,
        _workspace_id: &str,
        _session_id: &str,
    ) -> Result<
        Option<awaken_resource_contract::ArtifactCapture>,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never captures Files")
    }

    async fn replace_artifact_capture(
        &self,
        _request: awaken_resource_contract::ReplaceArtifactCapture,
    ) -> Result<
        awaken_resource_contract::ReplaceArtifactCaptureOutcome,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never replaces File captures")
    }

    async fn create_file(
        &self,
        _record: awaken_resource_contract::FileRecord,
    ) -> Result<
        awaken_resource_contract::CreateFileRecordOutcome,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never creates Files")
    }

    async fn get_file(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _include_deleted: bool,
    ) -> Result<
        Option<awaken_resource_contract::FileRecord>,
        awaken_resource_contract::FileCatalogError,
    > {
        unreachable!("Skill-only projection never resolves Files")
    }

    async fn list_files(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<awaken_resource_contract::FileRecord>, awaken_resource_contract::FileCatalogError>
    {
        Ok(Vec::new())
    }

    async fn list_files_including_deleted(
        &self,
        _workspace_id: &str,
        _scope_id: Option<&str>,
    ) -> Result<Vec<awaken_resource_contract::FileRecord>, awaken_resource_contract::FileCatalogError>
    {
        Ok(Vec::new())
    }

    async fn mark_file_deleted(
        &self,
        _workspace_id: &str,
        _file_id: &str,
    ) -> Result<
        Option<awaken_resource_contract::FileRecord>,
        awaken_resource_contract::FileCatalogError,
    > {
        Ok(None)
    }

    async fn active_size_bytes(
        &self,
        _workspace_id: &str,
    ) -> Result<u64, awaken_resource_contract::FileCatalogError> {
        Ok(0)
    }
}

#[async_trait::async_trait]
impl SessionRuntime for NoopRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

awaken_session_execution_contract::neutral_role!(CoordinationRuntime for NoopRuntime);

awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for NoopRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for NoopRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for NoopRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for NoopRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for NoopRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for NoopRuntime {
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
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
impl awaken_session_execution_contract::RealizationEffects for NoopRuntime {
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
        lease: &crate::session_resource::SessionRealizationLease,
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
        Ok(awaken_session_state_contract::SessionCleanupDisposalReceipt::new(&effect.command))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for NoopRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<
        awaken_session_application_contract::SessionBudgetResumeDisposition,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resume_budget_reached(delivery)
            .await
    }

    async fn session_budget_resume_tickets(
        &self,
        _session_id: &SessionId,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, RunError> {
        Ok(Vec::new())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The Session's accumulated token usage across all Runs, surfaced on the
    /// session's `usage` field. The default is empty — a runtime that reports no usage
    /// (the deterministic in-process models).
    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .session_usage(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read usage for one logical Thread from its parent Session partition.
    /// Global stores may use `thread_id` directly; filesystem/SQLite adapters
    /// must retain `session_id` as the physical read owner.
    async fn session_thread_usage(
        &self,
        _session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, awaken_session_types::RunError> {
        self.session_usage(thread_id).await
    }
}

no_committed_threads!(NoopRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for NoopRuntime {
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        authorization
            .verify_for(&effect)
            .map_err(|error| RunError::internal(error.to_string()))?;
        let provider_prepared_effect_fence = effect
            .sandbox_effect_fence()
            .map_err(|error| RunError::internal(error.to_string()))?;
        awaken_session_state_contract::SessionCleanupPreparation::try_new(
            &effect,
            provider_prepared_effect_fence,
            Vec::new(),
        )
        .map_err(|error| RunError::internal(error.to_string()))
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
        lease: &crate::session_resource::SessionRealizationLease,
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
impl awaken_session_execution_contract::OutcomeRuntime for NoopRuntime {
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
        Err(RunError::internal("unused test runtime"))
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
impl SessionRuntime for RecordingReplyRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    fn model(&self) -> String {
        "unused-reply-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingReplyRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingReplyRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingReplyRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingReplyRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingReplyRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingReplyRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingReplyRuntime {
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
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

awaken_session_execution_contract::neutral_role!(RealizationEffects for RecordingReplyRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for RecordingReplyRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.deliveries.lock().unwrap().push(delivery);
        match self.outcome {
            ReplyRuntimeOutcome::Accepted => Ok(()),
            ReplyRuntimeOutcome::BadRequest => Err(RunError::bad_request("rejected reply")),
            ReplyRuntimeOutcome::Unavailable => Err(RunError::unavailable("ambiguous reply")),
        }
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        if stream_sink.is_some() {
            self.observed_sinks.fetch_add(1, Ordering::SeqCst);
        }
        self.reply_session_thread_tool(delivery).await?;
        Ok(awaken_session_application_contract::StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_types::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for RecordingReplyRuntime {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        Default::default()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_admission_receipt(command)
            .await
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        Ok(vec![
            awaken_session_execution_contract::CoordinatedThreadLink {
                session_id: session_id.clone(),
                thread_id: awaken_agent_contract::agent::thread::Id("reply-child".into()),
                target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
                    agent_id: "reply-agent".into(),
                },
                created_by_operation_id: "reply-spawn".into(),
                latest_run_id: Some(awaken_agent_contract::agent::run::Id("reply-run".into())),
            },
        ])
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        if command.tool_use_id != "reply-tool"
            || command.expected_run_id.0 != "reply-run"
            || command.expected_correlation_id != "reply-correlation"
        {
            return Err(RunError::bad_request(
                "reply does not match the committed Awaiting ticket",
            ));
        }
        Ok(
            awaken_session_execution_contract::SessionThreadToolReplyFence {
                activity: self.activity.clone(),
                already_applied: false,
            },
        )
    }

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RecordingReplyRuntime {
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
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if self
            .boundaries
            .snapshot(session_id.as_str(), thread_id)
            .is_none()
        {
            self.boundaries.commit(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                &awaken_agent_contract::agent::run::Id("reply-run".into()),
                awaken_agent_contract::agent::run::RunState::Awaiting,
                "",
            );
        }
        let mut snapshot = self.boundaries.snapshot(session_id.as_str(), thread_id);
        if session_id == thread_id
            && let Some(snapshot) = snapshot.as_mut()
        {
            use awaken_agent_contract::agent::awaiting::{
                AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason,
            };
            snapshot.resume_tickets = vec![
                awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: awaken_agent_contract::agent::run::Id("reply-run".into()),
                    ticket: ResumeTicket::new(
                        "reply-correlation",
                        awaken_agent_contract::agent::run::Id("reply-run".into()),
                        awaken_agent_contract::agent::thread::Id(session_id.to_string()),
                        "reply-snapshot",
                        "reply-catalog",
                        AwaitTarget::ToolCall {
                            reason: ToolAwaitReason::Permission,
                            call_id: "reply-tool".into(),
                            tool: PendingTool {
                                tool_id: "write".into(),
                                arguments: serde_json::json!({"path": "answer.txt"}),
                            },
                            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                        },
                    ),
                },
            ];
        }
        Ok(snapshot)
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
impl awaken_session_execution_contract::OutcomeRuntime for RecordingReplyRuntime {
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
        Err(RunError::internal("unused reply test runtime"))
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
impl SessionRuntime for RecordingAgentAdmissionRuntime {
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
        "unused-admission-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingAgentAdmissionRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingAgentAdmissionRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingAgentAdmissionRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingAgentAdmissionRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingAgentAdmissionRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingAgentAdmissionRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingAgentAdmissionRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
    }

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
        self.installed_projections
            .lock()
            .unwrap()
            .push(projection.clone());
        if self.projection_failure.load(Ordering::SeqCst) {
            return Err((RunError::unavailable("fixture projection unavailable")).into());
        }
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
    }

    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

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

awaken_session_execution_contract::neutral_role!(RealizationEffects for RecordingAgentAdmissionRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for RecordingAgentAdmissionRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageReceipt, RunError> {
        assert!(
            !self.installed_projections.lock().unwrap().is_empty(),
            "coordination must install its complete projection before admission"
        );
        let accepted_thread = command.thread_id.clone();
        self.admissions.lock().unwrap().push(command);
        match self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(AgentAdmissionOutcome::Accepted)
        {
            AgentAdmissionOutcome::Accepted => Ok(
                awaken_session_execution_contract::SessionAgentMessageReceipt {
                    thread_id: accepted_thread,
                },
            ),
            AgentAdmissionOutcome::BadRequest => {
                Err(RunError::bad_request("rejected Agent admission"))
            }
            AgentAdmissionOutcome::Unavailable => {
                Err(RunError::unavailable("ambiguous Agent admission"))
            }
            AgentAdmissionOutcome::MismatchedReceipt => Ok(
                awaken_session_execution_contract::SessionAgentMessageReceipt {
                    thread_id: awaken_agent_contract::agent::thread::Id("mismatched-child".into()),
                },
            ),
        }
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        assert!(
            !self.installed_projections.lock().unwrap().is_empty(),
            "cold report continuation requires complete projection before admission"
        );
        self.continuations.lock().unwrap().push(command);
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.interruptions
            .lock()
            .unwrap()
            .push((session_id.to_string(), child_thread_id.clone()));
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_types::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for RecordingAgentAdmissionRuntime {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        Default::default()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_session_types::RunError,
    > {
        let _ = command;
        self.admission_reads.fetch_add(1, Ordering::SeqCst);
        self.admission_observations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(RunError::unavailable(
                    "admission observation was not scripted",
                ))
            })
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        let mut links = self
            .committed_links
            .lock()
            .unwrap()
            .iter()
            .filter(|link| link.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        links.extend(
            self.admissions
                .lock()
                .unwrap()
                .iter()
                .filter(|command| command.session_id == session_id)
                .map(
                    |command| awaken_session_execution_contract::CoordinatedThreadLink {
                        session_id: session_id.clone(),
                        thread_id: command.thread_id.clone(),
                        target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
                            agent_id: command.snapshot.root_agent_id.0.clone(),
                        },
                        created_by_operation_id: command.operation_id.clone(),
                        latest_run_id: Some(command.run_id.clone()),
                    },
                )
                .collect::<Vec<_>>(),
        );
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        links.dedup_by(|left, right| left.thread_id == right.thread_id);
        Ok(links)
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageCommand, RunError> {
        if self.source_valid.load(Ordering::SeqCst) {
            // These application tests exercise policy after the Runtime-owned
            // provenance boundary. ManagedHost owns the production refinement.
            Ok(command.clone())
        } else {
            Err(RunError::bad_request(
                "test source does not match committed ActiveToolBatch",
            ))
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RecordingAgentAdmissionRuntime {
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
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if !self.generic_recovery_available.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self.observed_snapshot(session_id.as_str(), thread_id))
    }

    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.observed_snapshot(session_id.as_str(), thread_id),
            run_id,
        )
    }

    async fn session_thread_disposition(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        Ok(
            if self
                .archived_threads
                .lock()
                .unwrap()
                .contains(&(session_id.to_string(), thread_id.to_string()))
            {
                awaken_agent_contract::ThreadDisposition::Archived
            } else {
                awaken_agent_contract::ThreadDisposition::Active
            },
        )
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
impl awaken_session_execution_contract::OutcomeRuntime for RecordingAgentAdmissionRuntime {
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
    ) -> Result<Option<awaken_session_execution_contract::OutcomeDrive>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingBoundaryBudgetRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    fn model(&self) -> String {
        "model".into()
    }
}

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingBoundaryBudgetRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for RecordingBoundaryBudgetRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for RecordingBoundaryBudgetRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingBoundaryBudgetRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingBoundaryBudgetRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingBoundaryBudgetRuntime {
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
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

awaken_session_execution_contract::neutral_role!(RealizationEffects for RecordingBoundaryBudgetRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for RecordingBoundaryBudgetRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    async fn continue_session_agent_report(
        &self,
        _command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.continuations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_session_types::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for RecordingBoundaryBudgetRuntime {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        Default::default()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_session_types::RunError,
    > {
        let _ = command;
        self.admission_reads.fetch_add(1, Ordering::SeqCst);
        self.admission_observations
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                Err(RunError::unavailable(
                    "admission observation was not scripted",
                ))
            })
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.usage_relationship_reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_usage_relationship_read.load(Ordering::SeqCst) {
            return Err(RunError::unavailable("relationship query unavailable"));
        }
        Ok(self
            .boundaries
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|((owner, _), _)| owner == session_id.as_str())
            .flat_map(|(_, snapshot)| snapshot.runs.iter())
            .map(
                |run| awaken_session_execution_contract::CoordinatedThreadLink {
                    session_id: session_id.clone(),
                    thread_id: run.thread_id.clone(),
                    target: self.child_target.clone(),
                    created_by_operation_id: "budget-spawn".into(),
                    latest_run_id: Some(run.id.clone()),
                },
            )
            .collect())
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for RecordingBoundaryBudgetRuntime {
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_application_contract::SessionBudgetResumeDisposition, RunError> {
        self.budget_resume_deliveries.lock().unwrap().push(delivery);
        Ok(self
            .budget_resume_dispositions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(
                awaken_session_application_contract::SessionBudgetResumeDisposition::Dispatched,
            ))
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, RunError> {
        Ok(self
            .budget_resume_tickets
            .lock()
            .unwrap()
            .get(session_id.as_str())
            .cloned()
            .unwrap_or_default())
    }

    async fn session_usage(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        Ok(self.root_usage.lock().unwrap().clone())
    }

    async fn session_thread_usage(
        &self,
        session_id: &SessionId,
        child_thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.usage_thread_reads
            .lock()
            .unwrap()
            .push(child_thread_id.to_string());
        if session_id == child_thread_id {
            Ok(self.root_usage.lock().unwrap().clone())
        } else {
            Ok(self.child_usage.lock().unwrap().clone())
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RecordingBoundaryBudgetRuntime {
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
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(self.boundaries.snapshot(session_id.as_str(), thread_id))
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
impl awaken_session_execution_contract::OutcomeRuntime for RecordingBoundaryBudgetRuntime {
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
        Err(RunError::internal("unused boundary budget test runtime"))
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
impl SessionRuntime for RecordingCleanupRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_application_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for RecordingCleanupRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for RecordingCleanupRuntime);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for RecordingCleanupRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RecordingCleanupRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for RecordingCleanupRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RecordingCleanupRuntime {
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
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
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
impl awaken_session_execution_contract::CoordinationRuntime for RecordingCleanupRuntime {
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        if self.fail_quiesce_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-quiescence crash window"));
        }
        if self.block_quiesce.load(Ordering::SeqCst) {
            self.quiesce_entered.notify_one();
            self.quiesce_release.notified().await;
        }
        Ok(self.delegated_snapshot.lock().unwrap().clone())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for RecordingCleanupRuntime {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        self.source_observation.lock().unwrap().clone()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_admission_receipt(command)
            .await
    }

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for RecordingCleanupRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .replace_session_tools(thread, tools)
            .await
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
        expected_mcp_generations: &[crate::session_mcp::McpGenerationRef],
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
    ) -> Result<Vec<crate::session_resource::ResolvedSkillBinding>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .resolve_session_skills(workspace_id, skills)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for RecordingCleanupRuntime {
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_state_contract::CheckpointReceipt, awaken_session_types::RunError>
    {
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
        awaken_session_types::RunError,
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
    ) -> Result<awaken_session_state_contract::SourceDisposedReceipt, awaken_session_types::RunError>
    {
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
    ) -> Result<awaken_session_state_contract::RestoreReceipt, awaken_session_types::RunError> {
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
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        _thread: &str,
        _transition: &crate::session_resource::SessionResourceTransition,
    ) -> Result<(), RunError> {
        self.ordinary_resource_apply_calls
            .fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<awaken_session_state_contract::SessionRepositoryPublicationEffect, RunError> {
        let exact = self
            .terminal_assignments
            .lock()
            .unwrap()
            .last()
            .is_some_and(|assignment| assignment.authority.lease == *lease);
        if !exact {
            return Err(RunError::internal(
                "terminal Repository effect does not match its installed assignment",
            ));
        }
        self.publication_intents
            .lock()
            .unwrap()
            .push(command.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("publication".into());
        if self.fail_publication_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::unavailable_classified(
                "repository_publication_transport_unavailable",
                "injected publication transport loss",
            ));
        }
        if self.close_publication_source.load(Ordering::SeqCst) {
            // Control conformance only: derive identity from the installed
            // assignment, while the Runtime Host integration owns the physical
            // observation. No independent source store or forged Git ref.
            let binding = self
                .terminal_assignments
                .lock()
                .unwrap()
                .last()
                .and_then(|assignment| {
                    assignment
                        .projection
                        .environment
                        .terminal_repository_publication_binding()
                })
                .map(str::to_owned)
                .ok_or_else(|| RunError::internal("closure fixture has no exact source"))?;
            let rejection = awaken_session_state_contract::SessionRepositoryPublicationRejection::source_closed(
                &command,
                &binding,
                awaken_provisioning_contract::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: None,
                },
            ).map_err(|error| RunError::internal(error.to_string()))?;
            return Ok(
                awaken_session_state_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ),
            );
        }
        if self.reject_publication.load(Ordering::SeqCst) {
            let rejection =
                awaken_session_state_contract::SessionRepositoryPublicationRejection::new(
                    &command,
                    awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent,
                )
                .map_err(|error| RunError::internal(error.to_string()))?;
            return Ok(
                awaken_session_state_contract::SessionRepositoryPublicationEffect::Rejected(
                    rejection,
                ),
            );
        }
        let crate::session_resource::ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } = &command.intent.input.source
        else {
            return Err(RunError::internal(
                "test publication command did not contain a Repository",
            ));
        };
        Ok(
            awaken_session_state_contract::SessionRepositoryPublicationEffect::Published(
                awaken_session_state_contract::SessionRepositoryPublicationReceipt::new(
                    &command,
                    awaken_provisioning_contract::RepositoryPublicationReceipt {
                        repository_id: repository_id.to_string(),
                        source_remote_url: config.remote_url.clone(),
                        branch: command.intent.expectation.branch.clone(),
                        commit: command.intent.expectation.commit.clone(),
                    },
                ),
            ),
        )
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("dispose".into());
        Ok(awaken_session_state_contract::SessionCleanupDisposalReceipt::new(&effect.command))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for RecordingCleanupRuntime {
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
        Err(RunError::internal("unused test runtime"))
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

#[derive(Default)]
struct PausedSessionMutation {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    candidate: Mutex<Option<awaken_session_state_contract::SessionMutation>>,
    result: Mutex<Option<awaken_session_state_contract::SessionMutationResult>>,
}

struct FaultingSessionRepository {
    pause_mutation_once: Mutex<Option<(String, Arc<PausedSessionMutation>)>>,
    create_crash_boundary: Mutex<Option<(bool, std::path::PathBuf)>>,
    inner: Arc<dyn awaken_session_repository_contract::SessionStore>,
    applied_create_roots: Mutex<Vec<PersistedSession>>,
    get_override_once: Mutex<Option<PersistedSession>>,
    paused_gets: Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>,
    failed_gets: Mutex<BTreeSet<String>>,
    observed_gets: Mutex<Vec<String>>,
    completed_gets: Mutex<Vec<String>>,
    get_entered: tokio::sync::Notify,
    fail_operation_once: Mutex<Option<String>>,
    conflict_operation_once: Mutex<Option<String>>,
    running_conflict_operation_once: Mutex<Option<String>>,
    realization_conflict_operation_once:
        Mutex<Option<(String, crate::session_resource::SessionRealizationLease)>>,
    tombstone_after_operation_once: Mutex<Option<String>>,
    get_not_found_once: AtomicBool,
    fail_idempotency_receipt_once: AtomicBool,
    fail_recovery_scan_once: AtomicBool,
    fail_environment_phase_count_once: AtomicBool,
    corrupt_environment_phase_count_once: AtomicBool,
    recovery_scan_count: AtomicUsize,
    fail_recovery_scan_on_call: AtomicUsize,
    quarantine_recovery_scan_on_call: AtomicUsize,
    stall_recovery_cursor_on_call: AtomicUsize,
    hold_recovery_scan_on_call: AtomicUsize,
    recovery_scan_entered: tokio::sync::Notify,
    recovery_scan_release: tokio::sync::Notify,
}

fn report_committed_mutation_as_conflict(
    result: awaken_session_state_contract::SessionMutationResult,
) -> awaken_session_state_contract::SessionMutationResult {
    match result {
        awaken_session_state_contract::SessionMutationResult::Applied { new_revision }
        | awaken_session_state_contract::SessionMutationResult::Replayed { new_revision }
        | awaken_session_state_contract::SessionMutationResult::Conflict {
            current_revision: new_revision,
        } => awaken_session_state_contract::SessionMutationResult::Conflict {
            current_revision: new_revision,
        },
        awaken_session_state_contract::SessionMutationResult::IdempotencyMismatch => {
            awaken_session_state_contract::SessionMutationResult::IdempotencyMismatch
        }
    }
}

impl FaultingSessionRepository {
    async fn pause_at_create_boundary(&self, after_commit: bool) {
        let marker = self.create_crash_boundary.lock().unwrap().clone();
        if let Some((after, marker)) = marker.filter(|(after, _)| *after == after_commit) {
            std::fs::write(marker, if after { "after" } else { "before" }).unwrap();
            std::future::pending::<()>().await;
        }
    }

    fn new(inner: Arc<dyn awaken_session_repository_contract::SessionStore>) -> Self {
        Self {
            inner,
            pause_mutation_once: Mutex::new(None),
            create_crash_boundary: Mutex::new(None),
            applied_create_roots: Mutex::new(Vec::new()),
            get_override_once: Mutex::new(None),
            paused_gets: Mutex::new(HashMap::new()),
            failed_gets: Mutex::new(BTreeSet::new()),
            observed_gets: Mutex::new(Vec::new()),
            completed_gets: Mutex::new(Vec::new()),
            get_entered: tokio::sync::Notify::new(),
            fail_operation_once: Mutex::new(None),
            conflict_operation_once: Mutex::new(None),
            running_conflict_operation_once: Mutex::new(None),
            realization_conflict_operation_once: Mutex::new(None),
            tombstone_after_operation_once: Mutex::new(None),
            get_not_found_once: AtomicBool::new(false),
            fail_idempotency_receipt_once: AtomicBool::new(false),
            fail_recovery_scan_once: AtomicBool::new(false),
            fail_environment_phase_count_once: AtomicBool::new(false),
            corrupt_environment_phase_count_once: AtomicBool::new(false),
            recovery_scan_count: AtomicUsize::new(0),
            fail_recovery_scan_on_call: AtomicUsize::new(0),
            quarantine_recovery_scan_on_call: AtomicUsize::new(0),
            stall_recovery_cursor_on_call: AtomicUsize::new(0),
            hold_recovery_scan_on_call: AtomicUsize::new(0),
            recovery_scan_entered: tokio::sync::Notify::new(),
            recovery_scan_release: tokio::sync::Notify::new(),
        }
    }

    fn pause_next_mutation(&self, operation: &str) -> Arc<PausedSessionMutation> {
        let pause = Arc::new(PausedSessionMutation::default());
        *self.pause_mutation_once.lock().unwrap() = Some((operation.into(), pause.clone()));
        pause
    }

    fn fail_once(&self, operation: &str) {
        *self.fail_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn applied_create_roots(&self) -> Vec<PersistedSession> {
        self.applied_create_roots.lock().unwrap().clone()
    }

    fn return_get_once(&self, session: PersistedSession) {
        *self.get_override_once.lock().unwrap() = Some(session);
    }

    fn commit_then_conflict_once(&self, operation: &str) {
        *self.conflict_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn commit_running_activity_then_conflict_once(&self, operation: &str) {
        *self.running_conflict_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn commit_concurrent_realization_then_conflict_once(
        &self,
        operation: &str,
        realization: crate::session_resource::SessionRealizationLease,
    ) {
        *self.realization_conflict_operation_once.lock().unwrap() =
            Some((operation.to_string(), realization));
    }

    fn tombstone_after_operation_once(&self, operation: &str) {
        *self.tombstone_after_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn get_not_found_once(&self) {
        self.get_not_found_once.store(true, Ordering::SeqCst);
    }

    fn fail_recovery_scan_once(&self) {
        self.fail_recovery_scan_once.store(true, Ordering::SeqCst);
    }

    fn recovery_scan_count(&self) -> usize {
        self.recovery_scan_count.load(Ordering::SeqCst)
    }

    fn fail_recovery_scan_on_call(&self, call: usize) {
        self.fail_recovery_scan_on_call
            .store(call, Ordering::SeqCst);
    }

    fn quarantine_recovery_scan_on_call(&self, call: usize) {
        self.quarantine_recovery_scan_on_call
            .store(call, Ordering::SeqCst);
    }

    fn stall_recovery_cursor_on_call(&self, call: usize) {
        self.stall_recovery_cursor_on_call
            .store(call, Ordering::SeqCst);
    }

    async fn commit_concurrent_root_as_conflict(
        &self,
        owner_scope: &str,
        concurrent: PersistedSession,
        operation: &str,
    ) -> Result<
        awaken_session_state_contract::SessionMutationResult,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        let session_id = concurrent.session_id.clone();
        let expected_revision = concurrent.revision;
        let payload = awaken_session_state_contract::SessionMutationPayload::Replace(concurrent);
        let payload_hash = payload.stable_hash();
        let result = self
            .inner
            .commit(
                owner_scope,
                awaken_session_state_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_state_contract::IdempotencyRecord {
                        key: format!("test:{operation}:{session_id}:{}", expected_revision.0),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await?;
        Ok(report_committed_mutation_as_conflict(result))
    }
}

#[async_trait::async_trait]
impl awaken_session_repository_contract::SessionRepositoryReads for FaultingSessionRepository {
    async fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, awaken_session_state_contract::SessionRepositoryError> {
        self.observed_gets
            .lock()
            .unwrap()
            .push(session_id.to_string());
        self.get_entered.notify_one();
        let gate = self
            .paused_gets
            .lock()
            .unwrap()
            .get(session_id.as_str())
            .cloned();
        if let Some(gate) = gate {
            gate.acquire()
                .await
                .expect("test read gate remains open")
                .forget();
        }
        if self
            .failed_gets
            .lock()
            .unwrap()
            .contains(session_id.as_str())
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected Session read failure".into(),
                ),
            );
        }
        if let Some(session) = self.get_override_once.lock().unwrap().take() {
            return Ok(session);
        }
        if self.get_not_found_once.swap(false, Ordering::SeqCst) {
            return Err(awaken_session_state_contract::SessionRepositoryError::NotFound);
        }
        let result = self.inner.get(session_id).await;
        self.completed_gets
            .lock()
            .unwrap()
            .push(session_id.to_string());
        self.get_entered.notify_one();
        result
    }

    async fn owner(
        &self,
        session_id: &SessionId,
    ) -> Result<String, awaken_session_state_contract::SessionRepositoryError> {
        self.inner.owner(session_id).await
    }

    async fn scan(
        &self,
        query: awaken_session_state_contract::SessionScanQuery<'_>,
    ) -> Result<
        awaken_session_state_contract::SessionScan,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.scan(query).await
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<
        Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }
}

#[async_trait::async_trait]
impl SessionRepository for FaultingSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: awaken_session_state_contract::IdempotencyRecord,
        lifecycle_facts: Vec<awaken_session_state_contract::ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_state_contract::SessionCreateResult,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.pause_at_create_boundary(false).await;
        let result = self
            .inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await?;
        self.pause_at_create_boundary(true).await;
        if let awaken_session_state_contract::SessionCreateResult::Applied(session) = &result {
            self.applied_create_roots
                .lock()
                .unwrap()
                .push(session.clone());
        }
        Ok(result)
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &awaken_session_state_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, awaken_session_state_contract::SessionRepositoryError>
    {
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: awaken_session_state_contract::SessionMutation,
    ) -> Result<
        awaken_session_state_contract::SessionMutationResult,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        // One test scheduling cut before the real repository CAS. Consume the
        // cut and capture the detached request under the lock, then release the
        // lock before await. Other writers therefore reach the same real store.
        let pause = {
            let mut configured = self.pause_mutation_once.lock().unwrap();
            if configured
                .as_ref()
                .is_some_and(|(operation, _)| mutation.idempotency.key.contains(operation))
            {
                configured.take().map(|(_, pause)| pause)
            } else {
                None
            }
        };
        if let Some(pause) = &pause {
            *pause.candidate.lock().unwrap() = Some(mutation.clone());
            pause.entered.notify_one();
            pause.release.notified().await;
        }
        let should_fail = self
            .fail_operation_once
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|operation| mutation.idempotency.key.contains(operation));
        if should_fail {
            self.fail_operation_once.lock().unwrap().take();
            return Err(
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected root-CAS outage".into(),
                ),
            );
        }
        let should_report_conflict = self
            .conflict_operation_once
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|operation| mutation.idempotency.key.contains(operation));
        if should_report_conflict {
            self.conflict_operation_once.lock().unwrap().take();
            let result = self.inner.commit(owner_scope, mutation).await?;
            return Ok(report_committed_mutation_as_conflict(result));
        }
        let should_commit_running = {
            let mut operation = self.running_conflict_operation_once.lock().unwrap();
            if operation
                .as_ref()
                .is_some_and(|operation| mutation.idempotency.key.contains(operation))
            {
                operation.take();
                true
            } else {
                false
            }
        };
        if should_commit_running {
            let session_id = mutation.payload.session_id().to_string();
            let mut concurrent = self.inner.get(&SessionId::from(session_id)).await?;
            concurrent.begin_activity_epoch().ok_or_else(|| {
                awaken_session_state_contract::SessionRepositoryError::InvalidMutation(
                    "injected concurrent activity exhausted its epoch".into(),
                )
            })?;
            concurrent
                .transition_execution(awaken_session_state_contract::SessionExecutionState::Running)
                .map_err(|error| {
                    awaken_session_state_contract::SessionRepositoryError::InvalidMutation(
                        error.to_string(),
                    )
                })?;
            if !concurrent.begin_runtime_interval(1, 0) {
                return Err(
                    awaken_session_state_contract::SessionRepositoryError::InvalidMutation(
                        "injected concurrent activity could not open its interval".into(),
                    ),
                );
            }
            return self
                .commit_concurrent_root_as_conflict(owner_scope, concurrent, "concurrent-activity")
                .await;
        }
        let concurrent_realization = {
            let mut configured = self.realization_conflict_operation_once.lock().unwrap();
            if configured
                .as_ref()
                .is_some_and(|(operation, _)| mutation.idempotency.key.contains(operation))
            {
                configured.take().map(|(_, realization)| realization)
            } else {
                None
            }
        };
        if let Some(realization) = concurrent_realization {
            let session_id = mutation.payload.session_id().to_string();
            let mut concurrent = self.inner.get(&SessionId::from(session_id)).await?;
            concurrent.realization = Some(realization);
            return self
                .commit_concurrent_root_as_conflict(
                    owner_scope,
                    concurrent,
                    "concurrent-realization",
                )
                .await;
        }
        let should_tombstone = {
            let mut operation = self.tombstone_after_operation_once.lock().unwrap();
            if operation
                .as_ref()
                .is_some_and(|operation| mutation.idempotency.key.contains(operation))
            {
                operation.take();
                true
            } else {
                false
            }
        };
        if should_tombstone {
            let session_id = mutation.payload.session_id().to_string();
            let committed_revision = match self.inner.commit(owner_scope, mutation).await? {
                awaken_session_state_contract::SessionMutationResult::Applied { new_revision }
                | awaken_session_state_contract::SessionMutationResult::Replayed { new_revision } => {
                    new_revision
                }
                conflict @ awaken_session_state_contract::SessionMutationResult::Conflict { .. }
                | conflict @ awaken_session_state_contract::SessionMutationResult::IdempotencyMismatch => {
                    return Ok(conflict);
                }
            };
            let deleted_revision = awaken_session_types::SessionRevision(
                committed_revision.0.checked_add(1).expect("test revision"),
            );
            let payload = awaken_session_state_contract::SessionMutationPayload::Delete(
                awaken_session_state_contract::SessionTombstone {
                    session_id: SessionId::from(&session_id),
                    deleted_revision,
                    deleted_at: "2026-08-21T00:00:00Z".into(),
                },
            );
            let payload_hash = payload.stable_hash();
            let result = self
                .inner
                .commit(
                    owner_scope,
                    awaken_session_state_contract::SessionMutation {
                        expected_revision: committed_revision,
                        idempotency: awaken_session_state_contract::IdempotencyRecord {
                            key: format!("test:concurrent-tombstone:{session_id}"),
                            payload_hash,
                        },
                        payload,
                        lifecycle_facts: Vec::new(),
                    },
                )
                .await?;
            assert!(matches!(
                result,
                awaken_session_state_contract::SessionMutationResult::Applied { .. }
                    | awaken_session_state_contract::SessionMutationResult::Replayed { .. }
            ));
            return Err(awaken_session_state_contract::SessionRepositoryError::NotFound);
        }
        let result = self.inner.commit(owner_scope, mutation).await;
        if let (Some(pause), Ok(result)) = (pause, &result) {
            *pause.result.lock().unwrap() = Some(result.clone());
        }
        result
    }

    async fn count_environment_phase(
        &self,
        phase: awaken_session_state_contract::SessionEnvironmentPhase,
    ) -> Result<u64, awaken_session_state_contract::SessionRepositoryError> {
        if self
            .corrupt_environment_phase_count_once
            .swap(false, Ordering::SeqCst)
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::Corrupt(
                    "injected corrupt Session root during Environment phase count".into(),
                ),
            );
        }
        if self
            .fail_environment_phase_count_once
            .swap(false, Ordering::SeqCst)
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected Environment phase count outage".into(),
                ),
            );
        }
        self.inner.count_environment_phase(phase).await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<
        Option<awaken_session_state_contract::SessionIdempotencyReceipt>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        if self
            .fail_idempotency_receipt_once
            .swap(false, Ordering::SeqCst)
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected command receipt read outage".into(),
                ),
            );
        }
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn reconciliation_scan(
        &self,
        after: Option<&awaken_session_state_contract::SessionScanCursor>,
    ) -> Result<
        awaken_session_state_contract::SessionScan,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        let call = self.recovery_scan_count.fetch_add(1, Ordering::SeqCst) + 1;
        if self.hold_recovery_scan_on_call.load(Ordering::SeqCst) == call {
            self.recovery_scan_entered.notify_one();
            self.recovery_scan_release.notified().await;
        }
        if self.fail_recovery_scan_once.swap(false, Ordering::SeqCst)
            || self.fail_recovery_scan_on_call.load(Ordering::SeqCst) == call
        {
            return Err(
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    "injected recovery scan outage".into(),
                ),
            );
        }
        let mut scan = self.inner.reconciliation_scan(after).await?;
        if self.quarantine_recovery_scan_on_call.load(Ordering::SeqCst) == call {
            scan.quarantined
                .push(awaken_session_types::SessionRecoveryQuarantine {
                    session_id: "injected-quarantine".into(),
                    reason: "injected recovery quarantine".into(),
                });
        }
        if self.stall_recovery_cursor_on_call.load(Ordering::SeqCst) == call {
            scan.next_cursor = after.cloned();
        }
        Ok(scan)
    }
}

#[async_trait::async_trait]
impl awaken_session_repository_contract::LifecycleOutbox for FaultingSessionRepository {
    async fn append_lifecycle(
        &self,
        fact: awaken_session_state_contract::ManagedLifecycleFact,
    ) -> Result<(), awaken_session_state_contract::SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(
        &self,
    ) -> Result<
        Vec<awaken_session_state_contract::ManagedLifecycleFact>,
        awaken_session_state_contract::SessionRepositoryError,
    > {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_state_contract::SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }
}

struct NoopMcpRealizer;

#[async_trait::async_trait]
impl McpAttachmentRealizer for NoopMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        // Test-realizer rule: admitted exact generation -> matching idempotent
        // receipt. Production's unsupported default remains fail-closed and is
        // covered in the contract suite; application happy-path fixtures must
        // model the successful dependency effect they assert.
        Ok(crate::session_mcp::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[derive(Default)]
struct RecordingEnvironmentSource {
    dispatched: Mutex<BTreeSet<String>>,
    dispatch_calls: AtomicUsize,
    awakened: Mutex<BTreeSet<String>>,
    retired: Mutex<Vec<String>>,
    failures: Mutex<BTreeSet<String>>,
}

#[derive(Default)]
struct RecordingWorkerObservations {
    workers: Mutex<Vec<awaken_worker_contract::RegisteredWorker>>,
    scripted: Mutex<VecDeque<WorkerObservationStep>>,
    list_calls: AtomicUsize,
    fail: AtomicBool,
    fail_incarnation: AtomicBool,
}

enum WorkerObservationStep {
    Workers(Vec<awaken_worker_contract::RegisteredWorker>),
    Fail,
}

impl RecordingWorkerObservations {
    fn replace(&self, workers: Vec<awaken_worker_contract::RegisteredWorker>) {
        *self.workers.lock().unwrap() = workers;
    }

    fn script(&self, steps: impl IntoIterator<Item = WorkerObservationStep>) {
        self.scripted.lock().unwrap().extend(steps);
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerObservationSource for RecordingWorkerObservations {
    async fn list(
        &self,
    ) -> Result<Vec<awaken_worker_contract::RegisteredWorker>, awaken_worker_contract::RegistryError>
    {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(step) = self.scripted.lock().unwrap().pop_front() {
            return match step {
                WorkerObservationStep::Workers(workers) => Ok(workers),
                WorkerObservationStep::Fail => {
                    Err(awaken_worker_contract::RegistryError::Persistence(
                        "injected Worker observation failure".into(),
                    ))
                }
            };
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(awaken_worker_contract::RegistryError::Persistence(
                "injected Worker observation failure".into(),
            ));
        }
        Ok(self.workers.lock().unwrap().clone())
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for RecordingWorkerObservations {
    async fn incarnation(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        if self.fail.load(Ordering::SeqCst) || self.fail_incarnation.load(Ordering::SeqCst) {
            return Err(awaken_worker_contract::RegistryError::Persistence(
                "injected exact-incarnation read failure".into(),
            ));
        }
        // This fixture owns only its supplied records; it never invents a
        // missing historical identity from a newer incarnation or its manifest.
        let records = self.workers.lock().unwrap();
        let mut matching = records
            .iter()
            .filter(|record| record.snapshot.identity == *identity);
        let record = matching.next().cloned();
        if matching.next().is_some() {
            return Err(awaken_worker_contract::RegistryError::Persistence(
                "duplicate exact fixture incarnation".into(),
            ));
        }
        Ok(record)
    }
}

fn terminal_cleanup_worker(
    target: &awaken_session_realization_contract::SessionRealizationTarget,
    manifest: awaken_worker_contract::WorkerManifest,
) -> awaken_worker_contract::RegisteredWorker {
    let identity = awaken_worker_contract::WorkerIdentity::from_lease_owner(
        &target.owner,
        &target.runtime_incarnation,
    )
    .unwrap_or_else(|| {
        // A malformed target stays mismatched; never normalize it into an
        // accepted incarnation. Exact targets share the production parser.
        awaken_worker_contract::WorkerIdentity::new(&target.owner, &target.runtime_incarnation, 1)
    });
    let capability_fingerprint = manifest
        .fingerprint()
        .expect("test Worker manifest fingerprint");
    awaken_worker_contract::RegisteredWorker {
        snapshot: awaken_worker_contract::WorkerSnapshot {
            identity,
            state: awaken_worker_contract::WorkerState::Ready,
            manifest,
            capability_fingerprint,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: target.lease_expires_at_unix_ms,
        },
        heartbeat_sequence: 1,
        observation_sequence: 0,
        registered_at_ms: 1,
        heartbeat_at_ms: 1,
        drain_deadline_ms: None,
        process_exit: None,
        create_ledger: None,
    }
}

fn terminal_cleanup_manifest() -> awaken_worker_contract::WorkerManifest {
    awaken_worker_contract::WorkerManifest {
        capabilities: BTreeSet::from([
            awaken_worker_contract::SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY.to_owned(),
        ]),
        runtime_protocol: awaken_worker_contract::VersionRange {
            min: 1,
            max: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        },
        dispatch_contract: awaken_worker_contract::VersionRange::exact(1),
        ..Default::default()
    }
}

impl RecordingEnvironmentSource {
    fn fail_for(&self, session_id: &str) {
        self.failures.lock().unwrap().insert(session_id.into());
    }

    fn recover_for(&self, session_id: &str) {
        self.failures.lock().unwrap().remove(session_id);
    }
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::SessionEnvironmentProjectionSource
    for RecordingEnvironmentSource
{
    type Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot;

    async fn get(
        &self,
        _environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        unreachable!("work dispatch does not reopen the Environment catalog")
    }

    async fn resolve_current_for_session(
        &self,
        _environment_id: &str,
        _runtime: Option<&str>,
        _mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        unreachable!("work dispatch consumes the frozen Session baseline")
    }

    async fn resolve_exact_for_session(
        &self,
        _environment_id: &str,
        _revision: u64,
        _runtime: Option<&str>,
        _mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
        unreachable!("work dispatch consumes the frozen Session baseline")
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerAssignment for RecordingEnvironmentSource {
    async fn enqueue_session(
        &self,
        _environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        if self.failures.lock().unwrap().contains(session_id.as_str()) {
            return Err(awaken_session_types::RunError::unavailable(format!(
                "injected failure for {session_id}"
            )));
        }
        self.dispatched
            .lock()
            .unwrap()
            .insert(session_id.to_string());
        self.dispatch_calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("work:{session_id}"))
    }

    async fn wake_session(
        &self,
        _environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        self.awakened.lock().unwrap().insert(session_id.to_string());
        Ok(format!("work:{session_id}"))
    }

    async fn retire_session(
        &self,
        _environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), awaken_session_types::RunError> {
        self.retired.lock().unwrap().push(session_id.to_string());
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeases for RecordingEnvironmentSource {
    async fn acquire_session(
        &self,
        environment_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<awaken_session_types::SessionWorkLease>, awaken_session_types::RunError>
    {
        Ok(self
            .awakened
            .lock()
            .unwrap()
            .contains(session_id.as_str())
            .then(|| {
                awaken_session_types::SessionWorkLease::from_persisted(
                    format!("work:{session_id}"),
                    environment_id,
                    session_id.clone(),
                    worker_owner,
                    (awaken_lease_contract::fence::Epoch::FIRST).get(),
                    now_ms.saturating_add(30_000),
                )
                .expect("canonical grant has a nonzero epoch")
            }))
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

fn persisted(id: &str, self_hosted: bool, status: &str) -> PersistedSession {
    let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env-worker".into(),
        revision: awaken_environment_contract::EnvironmentRevision(7),
        self_hosted,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "env-7".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let mut legacy = PersistedSession::frozen_with_budget(
        id,
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment,
                runtime_placement: if self_hosted {
                    SessionRuntimePlacement::Worker
                } else {
                    SessionRuntimePlacement::Local
                },
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        ),
        Default::default(),
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    legacy.execution = status.parse().expect("fixture execution state");
    // This fixture models the pre-cutover missing field, not a fresh creation.
    let mut wire = serde_json::to_value(legacy).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    wire.as_object_mut().unwrap().remove("run_activities");
    serde_json::from_value(wire).unwrap()
}

async fn create(
    repo: &dyn awaken_session_repository_contract::SessionStore,
    session: PersistedSession,
) {
    let id = session.session_id.clone();
    repo.create(
        "workspace",
        session,
        awaken_session_state_contract::IdempotencyRecord {
            key: format!("create:{id}"),
            payload_hash: format!("payload:{id}"),
        },
        Vec::new(),
    )
    .await
    .expect("persist fixture");
}

#[tokio::test]
async fn complete_runtime_composition_does_not_require_optional_role_discovery() {
    // Causes: C1 all required role traits; C2 explicit nonoptional role binding;
    // C3 a selected Store/Clock pair is prepared before Runtime composition.
    // R1 C1+C2+C3 => construction uses required roles and the same cached sink.
    // R2 repeated sink reads => pointer identity, no new writer or clock choice.
    // Missing C1 is excluded by CompleteSessionRuntime at compile time.
    let runtime = Arc::new(NoopRuntime);
    let bindings = crate::SessionExecutor::from(runtime.clone());
    assert_eq!(bindings.core().model(), runtime.model());
    let repository = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .await
            .unwrap(),
    );
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let persistence = crate::SessionPersistence::new(repository, clock.clone());
    let sink = persistence.environment_binding_sink();
    assert!(
        Arc::ptr_eq(&sink, &persistence.environment_binding_sink()),
        "R2"
    );
    let app = SessionApplicationBuilder::fixture(
        runtime,
        Arc::new(NoopMcpRealizer),
        persistence,
        Arc::new(RecordingEnvironmentSource::default()),
        std::sync::Arc::new(awaken_ext_builtin_tools::WebToolConfigurationCompiler),
    )
    .build()
    .expect("valid fixture Session application");
    assert!(Arc::ptr_eq(&sink, &app.environment_binding_sink()), "R1/R2");
    let selected_clock: Arc<dyn awaken_time::WallClock> = clock;
    assert!(Arc::ptr_eq(&selected_clock, &app.clock()), "R1");
}

fn skill_resources(id: &str) -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        Vec::new(),
        vec![crate::session_resource::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: id.into(),
            version: 1,
            bundle_sha256: format!("sha-{id}"),
        }],
    )
    .unwrap()
}

fn file_resources(id: &str) -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from("input"),
            source: crate::session_resource::ResolvedInputSource::File {
                file_id: awaken_resource_contract::FileId::from(id),
            },
            mount_path: "/workspace/input".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }],
        Vec::new(),
    )
    .unwrap()
}

fn repository_resources(
    binding_id: &str,
    repository_id: &str,
) -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![crate::session_resource::ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: awaken_resource_contract::BindingId::from(binding_id),
            source: crate::session_resource::ResolvedInputSource::Repository {
                repository_id: repository_id.into(),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: repository_id.into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: "https://example.test/repository.git".into(),
                    credential_binding: None,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential: None,
            },
            mount_path: "/workspace/source".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }],
        Vec::new(),
    )
    .expect("valid writable Repository fixture")
}

fn credentialed_repository_resources(
    binding_id: &str,
    repository_id: &str,
) -> crate::session_resource::ResolvedSessionResources {
    let resources = repository_resources(binding_id, repository_id);
    let mut inputs = resources.inputs().to_vec();
    let crate::session_resource::ResolvedInputSource::Repository { config, .. } =
        &mut inputs[0].source
    else {
        unreachable!("credential fixture is a Repository")
    };
    config.credential_binding = Some("repository-credential".into());
    crate::session_resource::ResolvedSessionResources::try_new(inputs, resources.skills().to_vec())
        .expect("credentialed Repository fixture")
}

fn repository_publication_intent(
    resources: &crate::session_resource::ResolvedSessionResources,
) -> awaken_session_state_contract::SessionRepositoryPublicationIntent {
    awaken_session_state_contract::SessionRepositoryPublicationIntent {
        input: resources.inputs()[0].clone(),
        expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/issue-coding".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            expected_prior_commit: None,
        },
    }
}

/// Local-restart ownership FMECA and cause/effect decision table. C1 the
/// configured logical owner is unchanged or different; C2 the Runtime
/// incarnation is the original process or a replacement. Effects are E1 an
/// exact replay, E2 immediate epoch-fenced takeover, E3 stale-owner
/// rejection, or E4 claim-authorized topology reassignment. C3 renewal and
/// reassignment are mutually exclusive so one command cannot obscure its
/// authority class.
///
/// | Rule | Logical owner | Incarnation | Reassign | Renew | Effect |
/// |---|---|---|---|---|---|
/// | L1 | same | same | false | false | E1 replay |
/// | L2 | same | replacement | false | false | E2 epoch advances immediately |
/// | L3 | different | replacement | false | false | E3 stale ownership |
/// | L4 | different | replacement | true | false | E4 epoch advances immediately |
/// | L5 | any | any | true | true | invalid, no mutation |
mod authority;
mod construction;
mod continuation;
mod creation;
mod event_batch_cutover_validation;
mod event_batches;
mod execution_observation;
mod realization;
mod recovery_scan;
mod recovery_sweep;
mod resource_resume;
mod run_admission;
