use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, PersistedSession, RunError,
    SandboxProvisioning, SessionEnvironmentBindingSink, SessionRuntime, SessionRuntimePlacement,
};

use super::*;

const COMPOSED_ASYNC_TEST_STACK_BYTES: usize = 32 * 1024 * 1024;

fn run_composed_async_test<F, Fut>(case: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    // One test-only executor owns the larger stack required by the two deeply
    // composed Event-batch recovery futures. The cases remain ordinary async
    // functions, so this changes neither their authority nor their oracle.
    let test = std::thread::Builder::new()
        .name("session-application-composed-test".into())
        .stack_size(COMPOSED_ASYNC_TEST_STACK_BYTES)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("composed test runtime")
                .block_on(case());
        })
        .expect("spawn composed test thread");
    if let Err(panic) = test.join() {
        std::panic::resume_unwind(panic);
    }
}

/// Execute the canonical complete-projection effects for a Session Application
/// test double. Domain mode decisions come from `SessionProjectionInstallMode`;
/// individual fakes override only the effect they need to observe.
async fn install_complete_test_projection<R: SessionRuntime + ?Sized>(
    runtime: &R,
    thread: &str,
    projection: awaken_session_contract::FrozenSessionProjection,
    mode: awaken_session_contract::SessionProjectionInstallMode,
) -> Result<(), RunError> {
    if thread.is_empty()
        || projection.workspace_id.is_empty()
        || projection.baseline.agent_id.is_empty()
    {
        return Err(RunError::bad_request(
            "test Session projection must carry complete frozen coordinates",
        ));
    }
    runtime
        .apply_session_inputs(
            thread,
            &projection.workspace_id,
            projection.resource_revision,
            &projection.resources,
        )
        .await?;
    if mode.prepares_session() {
        runtime
            .prepare_session(thread, projection.session_init())
            .await?;
    }
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

#[derive(Default)]
struct ToggleProjectionRefresh {
    calls: AtomicUsize,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_contract::ExecutableProjectionRefresh for ToggleProjectionRefresh {
    async fn refresh(&self) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            Err("projection unavailable".into())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
enum ReplyRuntimeOutcome {
    Accepted,
    BadRequest,
    Unavailable,
}

struct RecordingReplyRuntime {
    outcome: ReplyRuntimeOutcome,
    deliveries: Mutex<Vec<awaken_session_contract::SessionThreadToolReplyDelivery>>,
    boundaries: RecordingBoundaries,
}

struct RecordingBoundaryBudgetRuntime {
    root_usage: Mutex<awaken_session_contract::SessionUsage>,
    child_usage: Mutex<awaken_session_contract::SessionUsage>,
    child_target: awaken_session_contract::CoordinatedThreadTarget,
    continuations: AtomicUsize,
    boundaries: RecordingBoundaries,
    budget_resume_tickets:
        Mutex<HashMap<String, Vec<awaken_session_contract::SessionBudgetResumeTicket>>>,
    budget_resume_deliveries: Mutex<Vec<awaken_session_contract::SessionBudgetResumeDelivery>>,
    budget_resume_dispositions:
        Mutex<VecDeque<awaken_session_contract::SessionBudgetResumeDisposition>>,
}

#[derive(Clone, Copy)]
enum AgentAdmissionOutcome {
    Accepted,
    BadRequest,
    Unavailable,
    MismatchedReceipt,
}

struct RecordingAgentAdmissionRuntime {
    outcomes: Mutex<VecDeque<AgentAdmissionOutcome>>,
    admissions: Mutex<Vec<awaken_session_contract::CoordinatedRunCommand>>,
    committed_links: Mutex<Vec<awaken_session_contract::CoordinatedThreadLink>>,
    archived_threads: Mutex<BTreeSet<(String, String)>>,
    continuations: Mutex<Vec<awaken_session_contract::SessionAgentReportContinuation>>,
    interruptions: Mutex<Vec<(String, awaken_agent_contract::agent::thread::Id)>>,
    boundaries: RecordingBoundaries,
    generic_recovery_available: AtomicBool,
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
        self.set_recovery_snapshot(
            session_id,
            thread_id,
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
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
                events: Vec::new(),
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

    fn lifecycle(&self, session_id: &str) -> Vec<awaken_agent_contract::RunLifecycleEvent> {
        let mut boundaries = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|((candidate_session, _), _)| candidate_session == session_id)
            .flat_map(|((_, thread_id), snapshot)| {
                let commit_count = snapshot.runs.len();
                snapshot.runs.iter().enumerate().map(move |(index, run)| {
                    let source_commit_cursor = if commit_count == 1 {
                        snapshot.store_cursor.max(1)
                    } else {
                        (index as u64 + 1).min(snapshot.store_cursor.max(1))
                    };
                    (
                        source_commit_cursor,
                        awaken_agent_contract::agent::thread::Id(thread_id.clone()),
                        run.id.clone(),
                        run.state.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        boundaries.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.0.cmp(&right.1.0))
                .then_with(|| left.2.0.cmp(&right.2.0))
        });
        let mut offsets = HashMap::<u64, usize>::new();
        boundaries
            .into_iter()
            .map(|(source_commit_cursor, thread_id, run_id, state)| {
                let offset = offsets.entry(source_commit_cursor).or_default();
                let cursor = awaken_agent_contract::encode_run_lifecycle_cursor(
                    source_commit_cursor,
                    *offset,
                )
                .expect("test lifecycle cursor");
                *offset += 1;
                awaken_agent_contract::RunLifecycleEvent {
                    cursor,
                    source_commit_cursor,
                    kind: awaken_agent_contract::classify_run_lifecycle_event(&state, None),
                    thread_id,
                    run_id,
                    state,
                    await_reason: None,
                }
            })
            .collect()
    }

    fn lifecycle_page(
        &self,
        session_id: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> awaken_agent_contract::RunLifecyclePage {
        let events = self
            .lifecycle(session_id)
            .into_iter()
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .collect::<Vec<_>>();
        let next_cursor = events.last().map_or(cursor, |event| event.cursor);
        awaken_agent_contract::RunLifecyclePage {
            events,
            next_cursor,
        }
    }
}

impl RecordingAgentAdmissionRuntime {
    fn new(outcomes: impl IntoIterator<Item = AgentAdmissionOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            admissions: Mutex::new(Vec::new()),
            committed_links: Mutex::new(Vec::new()),
            archived_threads: Mutex::new(BTreeSet::new()),
            continuations: Mutex::new(Vec::new()),
            interruptions: Mutex::new(Vec::new()),
            boundaries: RecordingBoundaries::default(),
            generic_recovery_available: AtomicBool::new(true),
        }
    }

    fn commit_link(&self, link: awaken_session_contract::CoordinatedThreadLink) {
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
    fn new(child_usage: awaken_session_contract::SessionUsage) -> Self {
        Self {
            root_usage: Mutex::new(Default::default()),
            child_usage: Mutex::new(child_usage),
            child_target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                agent_id: "coord-child".into(),
            },
            continuations: AtomicUsize::new(0),
            boundaries: RecordingBoundaries::default(),
            budget_resume_tickets: Mutex::new(HashMap::new()),
            budget_resume_deliveries: Mutex::new(Vec::new()),
            budget_resume_dispositions: Mutex::new(VecDeque::new()),
        }
    }

    fn with_child_target(
        mut self,
        child_target: awaken_session_contract::CoordinatedThreadTarget,
    ) -> Self {
        self.child_target = child_target;
        self
    }

    fn set_root_usage(&self, usage: awaken_session_contract::SessionUsage) {
        *self.root_usage.lock().unwrap() = usage;
    }

    fn set_child_usage(&self, usage: awaken_session_contract::SessionUsage) {
        *self.child_usage.lock().unwrap() = usage;
    }

    fn set_budget_resume_tickets(
        &self,
        session_id: &str,
        tickets: Vec<awaken_session_contract::SessionBudgetResumeTicket>,
    ) {
        self.budget_resume_tickets
            .lock()
            .unwrap()
            .insert(session_id.to_string(), tickets);
    }

    fn set_budget_resume_dispositions(
        &self,
        dispositions: impl IntoIterator<Item = awaken_session_contract::SessionBudgetResumeDisposition>,
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
            outcome,
            deliveries: Mutex::new(Vec::new()),
            boundaries: RecordingBoundaries::default(),
        }
    }
}

#[derive(Default)]
struct RecordingCleanupRuntime {
    fail_quiesce_once: AtomicBool,
    fail_before_effect_once: AtomicBool,
    fail_once: AtomicBool,
    intents: Mutex<Vec<awaken_session_contract::SessionCleanupCommand>>,
    effective_ids: Mutex<BTreeSet<String>>,
    block_quiesce: AtomicBool,
    quiesce_entered: tokio::sync::Notify,
    quiesce_release: tokio::sync::Notify,
    delegated_snapshot: Mutex<awaken_session_contract::DelegatedRunSnapshot>,
    publication_intents: Mutex<Vec<awaken_session_contract::SessionRepositoryPublicationCommand>>,
    terminal_effect_order: Mutex<Vec<String>>,
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
            return Err(awaken_resource_contract::ResourcePurgeError::Storage(
                "injected reference projection failure".into(),
            ));
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
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn session_budget_resume_tickets(
        &self,
        _session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionBudgetResumeTicket>, RunError> {
        Ok(Vec::new())
    }

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &str,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(None)
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingReplyRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
        Ok(vec![awaken_session_contract::CoordinatedThreadLink {
            session_id: session_id.to_string(),
            thread_id: awaken_agent_contract::agent::thread::Id("reply-child".into()),
            target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                agent_id: "reply-agent".into(),
            },
            created_by_operation_id: "reply-spawn".into(),
            latest_run_id: Some(awaken_agent_contract::agent::run::Id("reply-run".into())),
        }])
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        if command.tool_use_id != "reply-tool"
            || command.expected_run_id.0 != "reply-run"
            || command.expected_correlation_id != "reply-correlation"
        {
            return Err(RunError::bad_request(
                "reply does not match the committed Awaiting ticket",
            ));
        }
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            prior_session_activity_epoch: Some(1),
            already_applied: false,
        })
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if self.boundaries.snapshot(session_id, thread_id).is_none() {
            self.boundaries.commit(
                session_id,
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                &awaken_agent_contract::agent::run::Id("reply-run".into()),
                awaken_agent_contract::agent::run::RunState::Awaiting,
                "",
            );
        }
        let mut snapshot = self.boundaries.snapshot(session_id, thread_id);
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
                        },
                    ),
                },
            ];
        }
        Ok(snapshot)
    }

    async fn committed_run_lifecycle(
        &self,
        session_id: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        Ok(self.boundaries.lifecycle_page(session_id, cursor, limit))
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
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
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        self.reply_session_thread_tool(delivery).await?;
        Ok(awaken_session_contract::StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        Err(RunError::internal("unused reply test runtime"))
    }

    fn model(&self) -> String {
        "unused-reply-model".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingAgentAdmissionRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
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
                .map(|command| awaken_session_contract::CoordinatedThreadLink {
                    session_id: session_id.to_string(),
                    thread_id: command.thread_id.clone(),
                    target: awaken_session_contract::CoordinatedThreadTarget::Agent {
                        agent_id: command.snapshot.root_agent_id.0.clone(),
                    },
                    created_by_operation_id: command.operation_id.clone(),
                    latest_run_id: Some(command.run_id.clone()),
                })
                .collect::<Vec<_>>(),
        );
        links.sort_by(|left, right| left.thread_id.0.cmp(&right.thread_id.0));
        links.dedup_by(|left, right| left.thread_id == right.thread_id);
        Ok(links)
    }

    async fn session_thread_disposition(
        &self,
        session_id: &str,
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

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if !self.generic_recovery_available.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(self.boundaries.snapshot(session_id, thread_id))
    }

    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let Some(mut snapshot) = self.boundaries.snapshot(session_id, thread_id) else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    async fn committed_run_lifecycle(
        &self,
        session_id: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        Ok(self.boundaries.lifecycle_page(session_id, cursor, limit))
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_contract::SessionAgentMessageReceipt, RunError> {
        let accepted_thread = command.thread_id.clone();
        self.admissions.lock().unwrap().push(command);
        match self
            .outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(AgentAdmissionOutcome::Accepted)
        {
            AgentAdmissionOutcome::Accepted => {
                Ok(awaken_session_contract::SessionAgentMessageReceipt {
                    thread_id: accepted_thread,
                })
            }
            AgentAdmissionOutcome::BadRequest => {
                Err(RunError::bad_request("rejected Agent admission"))
            }
            AgentAdmissionOutcome::Unavailable => {
                Err(RunError::unavailable("ambiguous Agent admission"))
            }
            AgentAdmissionOutcome::MismatchedReceipt => {
                Ok(awaken_session_contract::SessionAgentMessageReceipt {
                    thread_id: awaken_agent_contract::agent::thread::Id("mismatched-child".into()),
                })
            }
        }
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.continuations.lock().unwrap().push(command);
        Ok(())
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.interruptions
            .lock()
            .unwrap()
            .push((session_id.to_string(), child_thread_id.clone()));
        Ok(())
    }

    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime.run(agent, thread, content).await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime.resume(thread, tool_use_id, decision).await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        NoopRuntime
            .resume_custom(thread, tool_use_id, content, is_error)
            .await
    }

    async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        NoopRuntime
            .define_outcome(thread, description, rubric, max_iterations)
            .await
    }

    fn model(&self) -> String {
        "unused-admission-model".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingBoundaryBudgetRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::SessionBudgetResumeTicket>, RunError> {
        Ok(self
            .budget_resume_tickets
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_contract::SessionBudgetResumeDisposition, RunError> {
        self.budget_resume_deliveries.lock().unwrap().push(delivery);
        Ok(self
            .budget_resume_dispositions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(awaken_session_contract::SessionBudgetResumeDisposition::Dispatched))
    }

    async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
        Ok(self
            .boundaries
            .lifecycle(session_id)
            .into_iter()
            .map(|event| awaken_session_contract::CoordinatedThreadLink {
                session_id: session_id.to_string(),
                thread_id: event.thread_id,
                target: self.child_target.clone(),
                created_by_operation_id: "budget-spawn".into(),
                latest_run_id: Some(event.run_id),
            })
            .collect())
    }

    async fn session_usage(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        Ok(self.root_usage.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        Ok(self.boundaries.snapshot(session_id, thread_id))
    }

    async fn session_thread_usage(
        &self,
        session_id: &str,
        child_thread_id: &str,
    ) -> Result<awaken_session_contract::SessionUsage, RunError> {
        if session_id == child_thread_id {
            Ok(self.root_usage.lock().unwrap().clone())
        } else {
            Ok(self.child_usage.lock().unwrap().clone())
        }
    }

    async fn committed_run_lifecycle(
        &self,
        session_id: &str,
        cursor: awaken_agent_contract::RunLifecycleCursor,
        limit: usize,
    ) -> Result<awaken_agent_contract::RunLifecyclePage, RunError> {
        Ok(self.boundaries.lifecycle_page(session_id, cursor, limit))
    }

    async fn continue_session_agent_report(
        &self,
        _command: awaken_session_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.continuations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        Err(RunError::internal("unused boundary budget test runtime"))
    }

    fn model(&self) -> String {
        "model".into()
    }
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingCleanupRuntime {
    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
    }

    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, RunError> {
        if self.fail_quiesce_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-quiescence crash window"));
        }
        if self.block_quiesce.load(Ordering::SeqCst) {
            self.quiesce_entered.notify_one();
            self.quiesce_release.notified().await;
        }
        Ok(self.delegated_snapshot.lock().unwrap().clone())
    }

    async fn execute_terminal_cleanup(
        &self,
        intent: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        if self.fail_before_effect_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected pre-effect crash window"));
        }
        self.intents.lock().unwrap().push(intent.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push(format!("cleanup:{}", intent.thread_id));
        self.effective_ids
            .lock()
            .unwrap()
            .insert(intent.effect_id.clone());
        if self.fail_once.swap(false, Ordering::SeqCst) {
            return Err(RunError::internal("injected cleanup crash window"));
        }
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &intent,
            Vec::new(),
        ))
    }

    async fn execute_terminal_repository_publication(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
    ) -> Result<awaken_session_contract::SessionRepositoryPublicationReceipt, RunError> {
        self.publication_intents
            .lock()
            .unwrap()
            .push(command.clone());
        self.terminal_effect_order
            .lock()
            .unwrap()
            .push("publication".into());
        let awaken_session_contract::ResolvedInputSource::Repository {
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
            awaken_session_contract::SessionRepositoryPublicationReceipt::new(
                &command,
                awaken_provisioning_contract::RepositoryPublicationReceipt {
                    repository_id: repository_id.to_string(),
                    source_remote_url: config.remote_url.clone(),
                    branch: command.intent.expectation.branch.clone(),
                    commit: command.intent.expectation.commit.clone(),
                },
            ),
        )
    }

    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_contract::ToolPermissionDecision,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<awaken_session_contract::StepOutcome, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
    ) -> Result<awaken_session_contract::OutcomeDrive, RunError> {
        Err(RunError::internal("unused test runtime"))
    }

    fn model(&self) -> String {
        "unused".into()
    }
}

struct FaultingSessionRepository {
    inner: Arc<dyn ManagedSessionRepository>,
    applied_create_roots: Mutex<Vec<PersistedSession>>,
    fail_operation_once: Mutex<Option<String>>,
    conflict_operation_once: Mutex<Option<String>>,
    running_conflict_operation_once: Mutex<Option<String>>,
    tombstone_after_operation_once: Mutex<Option<String>>,
    get_not_found_once: AtomicBool,
    fail_recovery_scan_once: AtomicBool,
    recovery_scan_count: AtomicUsize,
}

fn report_committed_mutation_as_conflict(
    result: awaken_session_contract::SessionMutationResult,
) -> awaken_session_contract::SessionMutationResult {
    match result {
        awaken_session_contract::SessionMutationResult::Applied { new_revision }
        | awaken_session_contract::SessionMutationResult::Replayed { new_revision }
        | awaken_session_contract::SessionMutationResult::Conflict {
            current_revision: new_revision,
        } => awaken_session_contract::SessionMutationResult::Conflict {
            current_revision: new_revision,
        },
        awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
            awaken_session_contract::SessionMutationResult::IdempotencyMismatch
        }
    }
}

impl FaultingSessionRepository {
    fn new(inner: Arc<dyn ManagedSessionRepository>) -> Self {
        Self {
            inner,
            applied_create_roots: Mutex::new(Vec::new()),
            fail_operation_once: Mutex::new(None),
            conflict_operation_once: Mutex::new(None),
            running_conflict_operation_once: Mutex::new(None),
            tombstone_after_operation_once: Mutex::new(None),
            get_not_found_once: AtomicBool::new(false),
            fail_recovery_scan_once: AtomicBool::new(false),
            recovery_scan_count: AtomicUsize::new(0),
        }
    }

    fn fail_once(&self, operation: &str) {
        *self.fail_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn applied_create_roots(&self) -> Vec<PersistedSession> {
        self.applied_create_roots.lock().unwrap().clone()
    }

    fn commit_then_conflict_once(&self, operation: &str) {
        *self.conflict_operation_once.lock().unwrap() = Some(operation.to_string());
    }

    fn commit_running_activity_then_conflict_once(&self, operation: &str) {
        *self.running_conflict_operation_once.lock().unwrap() = Some(operation.to_string());
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
}

#[async_trait::async_trait]
impl ManagedSessionRepository for FaultingSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: awaken_session_contract::IdempotencyRecord,
        lifecycle_facts: Vec<awaken_session_contract::ManagedLifecycleFact>,
    ) -> Result<
        awaken_session_contract::SessionCreateResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        let result = self
            .inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await?;
        if let awaken_session_contract::SessionCreateResult::Applied(session) = &result {
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
        session_id: &str,
        idempotency: &awaken_session_contract::IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, awaken_session_contract::SessionRepositoryError> {
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: awaken_session_contract::SessionMutation,
    ) -> Result<
        awaken_session_contract::SessionMutationResult,
        awaken_session_contract::SessionRepositoryError,
    > {
        let should_fail = self
            .fail_operation_once
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|operation| mutation.idempotency.key.contains(operation));
        if should_fail {
            self.fail_operation_once.lock().unwrap().take();
            return Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
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
            let result = self.inner.commit_mutation(owner_scope, mutation).await?;
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
            let mut concurrent = self.inner.get(&session_id).await?;
            let expected_revision = concurrent.revision;
            concurrent.begin_activity_epoch().ok_or_else(|| {
                awaken_session_contract::SessionRepositoryError::InvalidMutation(
                    "injected concurrent activity exhausted its epoch".into(),
                )
            })?;
            concurrent
                .transition_execution(awaken_session_contract::SessionExecutionState::Running)
                .map_err(|error| {
                    awaken_session_contract::SessionRepositoryError::InvalidMutation(
                        error.to_string(),
                    )
                })?;
            if !concurrent.begin_runtime_interval(1) {
                return Err(
                    awaken_session_contract::SessionRepositoryError::InvalidMutation(
                        "injected concurrent activity could not open its interval".into(),
                    ),
                );
            }
            let payload = awaken_session_contract::SessionMutationPayload::Replace(concurrent);
            let payload_hash = payload.stable_hash();
            let result = self
                .inner
                .commit_mutation(
                    owner_scope,
                    awaken_session_contract::SessionMutation {
                        expected_revision,
                        idempotency: awaken_session_contract::IdempotencyRecord {
                            key: format!(
                                "test:concurrent-activity:{session_id}:{}",
                                expected_revision.0
                            ),
                            payload_hash,
                        },
                        payload,
                        lifecycle_facts: Vec::new(),
                    },
                )
                .await?;
            return Ok(report_committed_mutation_as_conflict(result));
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
            let committed_revision = match self.inner.commit_mutation(owner_scope, mutation).await?
            {
                awaken_session_contract::SessionMutationResult::Applied { new_revision }
                | awaken_session_contract::SessionMutationResult::Replayed { new_revision } => {
                    new_revision
                }
                conflict @ awaken_session_contract::SessionMutationResult::Conflict { .. }
                | conflict @ awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                    return Ok(conflict);
                }
            };
            let deleted_revision = awaken_session_contract::SessionRevision(
                committed_revision.0.checked_add(1).expect("test revision"),
            );
            let payload = awaken_session_contract::SessionMutationPayload::Delete(
                awaken_session_contract::SessionTombstone {
                    session_id: session_id.clone(),
                    deleted_revision,
                    deleted_at: "2026-08-21T00:00:00Z".into(),
                },
            );
            let payload_hash = payload.stable_hash();
            let result = self
                .inner
                .commit_mutation(
                    owner_scope,
                    awaken_session_contract::SessionMutation {
                        expected_revision: committed_revision,
                        idempotency: awaken_session_contract::IdempotencyRecord {
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
                awaken_session_contract::SessionMutationResult::Applied { .. }
                    | awaken_session_contract::SessionMutationResult::Replayed { .. }
            ));
            return Err(awaken_session_contract::SessionRepositoryError::NotFound);
        }
        self.inner.commit_mutation(owner_scope, mutation).await
    }

    async fn append_lifecycle(
        &self,
        fact: awaken_session_contract::ManagedLifecycleFact,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(
        &self,
    ) -> Result<
        Vec<awaken_session_contract::ManagedLifecycleFact>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(
        &self,
        fact_id: &str,
    ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }

    async fn get(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
        if self.get_not_found_once.swap(false, Ordering::SeqCst) {
            return Err(awaken_session_contract::SessionRepositoryError::NotFound);
        }
        self.inner.get(session_id).await
    }

    async fn reconcilable_sessions(
        &self,
    ) -> Result<
        awaken_session_contract::SessionRecoveryScan,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.recovery_scan_count.fetch_add(1, Ordering::SeqCst);
        if self.fail_recovery_scan_once.swap(false, Ordering::SeqCst) {
            return Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "injected recovery scan outage".into(),
                ),
            );
        }
        self.inner.reconcilable_sessions().await
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, awaken_session_contract::SessionRepositoryError> {
        self.inner
            .sessions_referencing_credential_source(workspace_id, source_id)
            .await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<
        Option<awaken_session_contract::SessionIdempotencyReceipt>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn owner(
        &self,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
        self.inner.owner(session_id).await
    }
}

struct NoopMcpRealizer;

#[async_trait::async_trait]
impl McpAttachmentRealizer for NoopMcpRealizer {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        // Test-realizer rule: admitted exact generation -> matching idempotent
        // receipt. Production's unsupported default remains fail-closed and is
        // covered in the contract suite; application happy-path fixtures must
        // model the successful dependency effect they assert.
        Ok(awaken_session_contract::McpRealizationReceipt {
            receipt_fingerprint: request.fingerprint(),
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_contract::McpGenerationRef,
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

impl RecordingEnvironmentSource {
    fn fail_for(&self, session_id: &str) {
        self.failures.lock().unwrap().insert(session_id.into());
    }

    fn recover_for(&self, session_id: &str) {
        self.failures.lock().unwrap().remove(session_id);
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentSource for RecordingEnvironmentSource {
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

    async fn enqueue_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        if self.failures.lock().unwrap().contains(session_id) {
            return Err(
                awaken_session_contract::work_queue::WorkQueueError::Storage(format!(
                    "injected failure for {session_id}"
                )),
            );
        }
        self.dispatched
            .lock()
            .unwrap()
            .insert(session_id.to_string());
        self.dispatch_calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("work:{session_id}"))
    }

    async fn wake_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
        self.awakened.lock().unwrap().insert(session_id.to_string());
        Ok(format!("work:{session_id}"))
    }

    async fn retire_session_work(
        &self,
        _environment_id: &str,
        session_id: &str,
    ) -> Result<
        Option<awaken_session_contract::work_queue::WorkItem>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        self.retired.lock().unwrap().push(session_id.to_string());
        Ok(None)
    }

    async fn acquire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<
        Option<awaken_session_contract::work_queue::SessionWorkLease>,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        Ok(self.awakened.lock().unwrap().contains(session_id).then(|| {
            awaken_session_contract::work_queue::SessionWorkLease {
                work_id: format!("work:{session_id}"),
                environment_id: environment_id.to_string(),
                session_id: session_id.to_string(),
                owner: worker_owner.to_string(),
                epoch: 1,
                expires_at_unix_ms: now_ms.saturating_add(30_000),
            }
        }))
    }
}

fn persisted(id: &str, self_hosted: bool, status: &str) -> PersistedSession {
    let environment = awaken_session_contract::EnvironmentSnapshot {
        environment_id: "env-worker".into(),
        revision: awaken_environment_contract::EnvironmentRevision(7),
        self_hosted,
        config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-7".into()),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    PersistedSession {
        session_id: id.into(),
        revision: Default::default(),
        baseline: awaken_session_contract::SessionBaselineState::Frozen(
            awaken_session_contract::SessionBaseline::compile(
                awaken_session_contract::SessionBaselineInputs {
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
        ),
        title: None,
        metadata: Default::default(),
        tools: Default::default(),
        event_batches: Vec::new(),
        activity_epoch: 0,
        active_activity_epochs: Default::default(),
        running_interval: None,
        closed_runtime_intervals: Vec::new(),
        runtime_active_millis: 0,
        usage_cursor: Default::default(),
        budget: Default::default(),
        environment: Default::default(),
        mcp: Default::default(),
        resources: Default::default(),
        realization: None,
        realization_progress: Default::default(),
        execution: status.parse().expect("fixture execution state"),
        disposition: Default::default(),
        terminal_cleanup: Default::default(),
    }
}

async fn create(repo: &dyn ManagedSessionRepository, session: PersistedSession) {
    let id = session.session_id.clone();
    repo.create(
        "workspace",
        session,
        awaken_session_contract::IdempotencyRecord {
            key: format!("create:{id}"),
            payload_hash: format!("payload:{id}"),
        },
        Vec::new(),
    )
    .await
    .expect("persist fixture");
}

fn application(
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
) -> SessionApplication {
    application_with_configuration(
        repo,
        environments,
        SessionApplicationConfiguration::default(),
    )
}

fn application_with_runtime(
    runtime: Arc<dyn SessionRuntime>,
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
) -> SessionApplication {
    SessionApplication::new(runtime, Arc::new(NoopMcpRealizer), repo, environments)
}

fn application_with_configuration(
    repo: Arc<dyn ManagedSessionRepository>,
    environments: Arc<dyn SessionEnvironmentSource>,
    configuration: SessionApplicationConfiguration,
) -> SessionApplication {
    SessionApplication::new_with_configuration(
        Arc::new(NoopRuntime),
        Arc::new(NoopMcpRealizer),
        repo,
        environments,
        configuration,
    )
}

fn skill_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources::try_new(
        Vec::new(),
        vec![awaken_session_contract::ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: id.into(),
            version: 1,
            bundle_sha256: format!("sha-{id}"),
        }],
    )
    .unwrap()
}

fn file_resources(id: &str) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("input"),
            source: awaken_session_contract::ResolvedInputSource::File {
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
) -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from(binding_id),
            source: awaken_session_contract::ResolvedInputSource::Repository {
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

fn repository_publication_intent(
    resources: &awaken_session_contract::ResolvedSessionResources,
) -> awaken_session_contract::SessionRepositoryPublicationIntent {
    awaken_session_contract::SessionRepositoryPublicationIntent {
        input: resources.inputs()[0].clone(),
        expectation: awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/issue-coding".into(),
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
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
mod continuation;
mod creation;
mod event_batch_cutover_validation;
mod event_batches;
mod realization;
mod run_admission;
