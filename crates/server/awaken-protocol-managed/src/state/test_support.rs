// @formal surface exclude=test_only
// @formal owner crates/server/awaken-protocol-managed/src/state/tests/mod.rs
// @formal reason: Managed-state fixtures and fake runtime compiled only through the test-gated parent
// @formal reason: module.
//! Shared Managed-state test fixtures.
//!
//! These fixtures serve rehydration, deployment-session, and activity tests.
//! Keeping the single fake Runtime here avoids parallel fake implementations in
//! those behavior modules while leaving each test beside the behavior it covers.

mod read_control;
#[cfg(test)]
mod session_repository {
    //! Existing real-SQLite conflict fixture, shared with controlled refresh reads.
    //! Every operation still delegates to SQLite except the explicitly armed CAS
    //! conflict or read failure. This is the sole repository decorator for both.

    use super::ReadControl;
    use async_trait::async_trait;
    use awaken_session_repository_contract::SessionRepository;
    use awaken_session_state_contract::PersistedSession;
    use awaken_session_store::SqliteManagedSessionRepository;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    pub(crate) struct ConflictInjectingRepo {
        inner: Arc<SqliteManagedSessionRepository>,
        conflicts: AtomicU64,
        reads: ReadControl,
    }

    impl ConflictInjectingRepo {
        pub(crate) fn new(
            inner: Arc<SqliteManagedSessionRepository>,
            conflicts: u64,
            reads: ReadControl,
        ) -> Self {
            Self {
                inner,
                conflicts: AtomicU64::new(conflicts),
                reads,
            }
        }
    }

    #[async_trait]
    impl SessionRepository for ConflictInjectingRepo {
        async fn create(
            &self,
            owner_scope: &str,
            session: PersistedSession,
            idempotency: awaken_session_state_contract::IdempotencyRecord,
            facts: Vec<awaken_session_state_contract::ManagedLifecycleFact>,
        ) -> Result<
            awaken_session_state_contract::SessionCreateResult,
            awaken_session_state_contract::SessionRepositoryError,
        > {
            self.inner
                .create(owner_scope, session, idempotency, facts)
                .await
        }

        async fn replay_create(
            &self,
            owner_scope: &str,
            session_id: &awaken_session_types::SessionId,
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
            if self
                .conflicts
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Ok(
                    awaken_session_state_contract::SessionMutationResult::Conflict {
                        current_revision: mutation.expected_revision,
                    },
                );
            }
            self.inner.commit(owner_scope, mutation).await
        }

        async fn get(
            &self,
            session_id: &awaken_session_types::SessionId,
        ) -> Result<PersistedSession, awaken_session_state_contract::SessionRepositoryError>
        {
            self.reads.on_read("session").await.map_err(|port| {
                awaken_session_state_contract::SessionRepositoryError::AuthorityUnavailable(
                    format!("scripted {port} read failure"),
                )
            })?;
            self.inner.get(session_id).await
        }

        async fn idempotency_receipt(
            &self,
            session_id: &awaken_session_types::SessionId,
            key: &str,
        ) -> Result<
            Option<awaken_session_state_contract::SessionIdempotencyReceipt>,
            awaken_session_state_contract::SessionRepositoryError,
        > {
            self.inner.idempotency_receipt(session_id, key).await
        }

        async fn owner(
            &self,
            session_id: &awaken_session_types::SessionId,
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
            session_id: &awaken_session_types::SessionId,
        ) -> Result<
            Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
            awaken_session_state_contract::SessionRepositoryError,
        > {
            self.inner
                .deleted_event_admissions(owner_scope, session_id)
                .await
        }

        async fn count_environment_phase(
            &self,
            phase: awaken_session_state_contract::SessionEnvironmentPhase,
        ) -> Result<u64, awaken_session_state_contract::SessionRepositoryError> {
            self.inner.count_environment_phase(phase).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_repository_contract::LifecycleOutbox for ConflictInjectingRepo {
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
}

pub(crate) use read_control::ReadControl;
pub(crate) use session_repository::ConflictInjectingRepo;

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::state::StateKey as _;
use awaken_session_execution_contract::ToolPermissionDecision;
use awaken_session_state_contract::PersistedSession;
use awaken_session_store::SqliteManagedSessionRepository;

use super::{DelegatedRun, OutcomeDrive, RunError, SessionRuntime};
use awaken_session_application_contract::StepOutcome;

fn scripted_read_failure(port: &'static str) -> RunError {
    RunError::unavailable(format!("scripted {port} read failure"))
}

pub(super) fn ephemeral_session_repo() -> SqliteManagedSessionRepository {
    SqliteManagedSessionRepository::open_in_memory()
        .expect("open ephemeral managed Session repository")
}

pub(super) async fn create_session_fixture(
    repo: &dyn awaken_session_repository_contract::SessionStore,
    owner: &str,
    mut session: PersistedSession,
) {
    session.revision = awaken_session_types::SessionRevision(0);
    let payload = awaken_session_state_contract::SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    repo.create(
        owner,
        session.clone(),
        awaken_session_state_contract::IdempotencyRecord {
            key: format!("test:create:{}:{payload_hash}", session.session_id),
            payload_hash,
        },
        Vec::new(),
    )
    .await
    .expect("create Session fixture");
}

pub(super) type RestoredRuntime = (
    String,
    Option<String>,
    usize,
    awaken_session_configuration_contract::SessionNetworkPolicy,
    awaken_provisioning_contract::SandboxOverride,
);

pub(super) type ChildCommitAfterHistorySnapshot = (
    String,
    Vec<Message>,
    Vec<awaken_agent_contract::RunLifecycleEvent>,
);

/// A runtime that reports committed Thread facts after the durable Session root
/// has authorized rehydration. The transcript is projection input only and can
/// never establish Session existence by itself.
#[derive(Clone, Default)]
pub(crate) struct RehydrateFake {
    pub(crate) reads: ReadControl,
    pub(super) commit_times_by_thread: Arc<
        std::sync::Mutex<std::collections::HashMap<String, std::collections::BTreeMap<u64, u64>>>,
    >,
    pub(super) restored: Arc<
        std::sync::Mutex<
            Vec<(
                String,
                String,
                awaken_session_resource_contract::ResolvedSessionResources,
            )>,
        >,
    >,
    pub(super) restored_environments: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
    pub(super) restored_runtimes: Arc<std::sync::Mutex<Vec<RestoredRuntime>>>,
    pub(super) delegated: Arc<std::sync::Mutex<Vec<DelegatedRun>>>,
    pub(super) coordinated:
        Arc<std::sync::Mutex<Vec<awaken_session_execution_contract::CoordinatedThreadLink>>>,
    pub(super) lifecycle: Arc<std::sync::Mutex<Vec<awaken_agent_contract::RunLifecycleEvent>>>,
    /// Test-only interleaving control for lifecycle projection: when set, a
    /// root recovery read exposes this older backend watermark while the
    /// lifecycle feed may already contain newer commits.
    pub(super) root_store_cursor_override: Arc<std::sync::Mutex<Option<u64>>>,
    pub(super) pending_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_application_contract::Pending>,
        >,
    >,
    pub(super) disposition_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<(String, String), awaken_agent_contract::ThreadDisposition>,
        >,
    >,
    pub(super) archive_commits: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    pub(super) thread_tool_replies:
        Arc<std::sync::Mutex<Vec<awaken_session_execution_contract::SessionThreadToolReplyCommand>>>,
    pub(super) delegate_ids: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) custom_tools:
        Arc<std::sync::Mutex<Vec<awaken_session_configuration_contract::CustomTool>>>,
    pub(super) order: Arc<std::sync::Mutex<Vec<&'static str>>>,
    pub(super) committed: Arc<std::sync::Mutex<Option<Vec<Message>>>>,
    pub(super) committed_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<Message>>>>,
    /// Explicit Run records used by canonical coordination fixtures whose
    /// accepted root transcript is committed before any root lifecycle edge.
    pub(super) run_ids_by_thread: Arc<
        std::sync::Mutex<std::collections::HashMap<String, awaken_agent_contract::agent::run::Id>>,
    >,
    /// Exact fake commit coordinates aligned with `committed_by_thread`.
    /// Absent entries mean the fixture committed its configured messages in
    /// one atomic prefix, not that production may omit the coordinates.
    pub(super) message_cursors_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    pub(super) state_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, Vec<awaken_agent_contract::agent::state::Command>>,
        >,
    >,
    pub(super) state_cursors_by_thread:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u64>>>>,
    pub(super) outcome_projections: Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                (String, String),
                awaken_session_execution_contract::CommittedOutcomeProjection,
            >,
        >,
    >,
    pub(super) usage_by_thread: Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, awaken_session_budget_contract::SessionUsage>,
        >,
    >,
    /// Test-only commit interleaving: return the current child transcript, then
    /// atomically expose the supplied newer transcript/lifecycle facts. This
    /// models a terminal commit racing a projector read without a second fake.
    pub(super) child_commit_after_history_snapshot:
        Arc<std::sync::Mutex<Option<ChildCommitAfterHistorySnapshot>>>,
    pub(super) pending: Arc<std::sync::Mutex<Option<awaken_session_application_contract::Pending>>>,
    pub(super) ended: Arc<std::sync::Mutex<Vec<String>>>,
    pub(super) reject_mcp_publication: Arc<std::sync::atomic::AtomicBool>,
    pub(super) reject_environment_adoption: Arc<std::sync::atomic::AtomicBool>,
}

impl RehydrateFake {
    /// Install the one Runtime-owned accepted `send_message` prefix that
    /// authorizes an ordinary coordinated link. Tests must not fabricate a
    /// Managed-only link anchor: the root ToolUse/ToolResult messages, their
    /// exact commit coordinate, the root Run, and the relationship are one
    /// recovery fixture.
    pub(crate) fn install_agent_coordination_prefix(
        &self,
        session_id: &str,
        root_run_id: awaken_agent_contract::agent::run::Id,
        step: usize,
        call_id: &str,
        link: awaken_session_execution_contract::CoordinatedThreadLink,
    ) {
        assert!(matches!(
            &link.target,
            awaken_session_execution_contract::CoordinatedThreadTarget::Agent { .. }
        ));
        assert_eq!(
            link.created_by_operation_id,
            awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                &root_run_id,
                step,
                call_id,
            )
        );
        let agent_id = link
            .target
            .agent_id()
            .expect("ordinary coordination fixture has an Agent target")
            .to_string();
        let mut messages = self
            .committed_by_thread
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .or_else(|| self.committed.lock().unwrap().clone())
            .unwrap_or_default();
        let mut cursors = self
            .message_cursors_by_thread
            .lock()
            .unwrap()
            .get(session_id)
            .filter(|cursors| cursors.len() == messages.len())
            .cloned()
            .unwrap_or_else(|| {
                (1..=messages.len())
                    .map(|cursor| u64::try_from(cursor).expect("fixture message cursor"))
                    .collect()
            });
        let source_commit_cursor = cursors
            .iter()
            .copied()
            .max()
            .unwrap_or_default()
            .checked_add(1)
            .expect("fixture message cursor exhausted");
        messages.extend([
            Message::new(
                awaken_agent_contract::agent::message::Id::assistant(&root_run_id, step),
                awaken_agent_contract::agent::message::Role::Assistant,
                vec![ContentBlock::tool_use(
                    call_id,
                    awaken_managed_session_events::SEND_MESSAGE_TOOL_ID,
                    serde_json::json!({
                        "agent_id": agent_id,
                        "message": "fixture coordination"
                    }),
                )],
            ),
            Message::new(
                awaken_agent_contract::agent::message::Id::tool_result(call_id),
                awaken_agent_contract::agent::message::Role::Tool,
                vec![ContentBlock::tool_result(
                    call_id,
                    vec![ContentBlock::text(
                        serde_json::json!({
                            "accepted": true,
                            "session_thread_id": link.thread_id.0.clone()
                        })
                        .to_string(),
                    )],
                )],
            ),
        ]);
        cursors.extend([source_commit_cursor, source_commit_cursor]);
        self.message_cursors_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), cursors);
        self.committed_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), messages);
        self.run_ids_by_thread
            .lock()
            .unwrap()
            .insert(session_id.to_string(), root_run_id);
        self.coordinated.lock().unwrap().push(link);
    }

    fn commit_staged_child_prefix_after_snapshot(&self, thread_id: &str) {
        let staged = self
            .child_commit_after_history_snapshot
            .lock()
            .unwrap()
            .take();
        if let Some((target_thread, messages, lifecycle)) = staged {
            if target_thread == thread_id {
                let message_cursor = lifecycle
                    .iter()
                    .filter(|event| event.thread_id.0 == target_thread)
                    .map(|event| event.source_commit_cursor)
                    .max()
                    .expect("staged child message has its atomic lifecycle commit");
                self.message_cursors_by_thread
                    .lock()
                    .unwrap()
                    .insert(target_thread.clone(), vec![message_cursor; messages.len()]);
                self.committed_by_thread
                    .lock()
                    .unwrap()
                    .insert(target_thread, messages);
                self.lifecycle.lock().unwrap().extend(lifecycle);
            } else {
                *self.child_commit_after_history_snapshot.lock().unwrap() =
                    Some((target_thread, messages, lifecycle));
            }
        }
    }
}

#[async_trait]
impl SessionRuntime for RehydrateFake {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
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
        "host-default-model".to_string()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
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

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
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

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for RehydrateFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RehydrateFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_error_contract::RunError,
    > {
        crate::test_support::observe_realization_request()
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
        let adopts_environment = mode.adopts_resident_environment();
        if let Some(init) =
            crate::test_support::complete_session_projection_init(thread, &projection, &mode)?
        {
            self.order.lock().unwrap().push("runtime");
            self.restored_runtimes.lock().unwrap().push((
                thread.to_string(),
                init.runtime,
                0,
                init.environment.network,
                init.environment.sandbox,
            ));
        }
        if adopts_environment && let Some(binding) = projection.environment.binding() {
            self.environment()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "EnvironmentRuntime is unsupported by this runtime",
                    )
                })?
                .adopt_session_environment(&projection.baseline.agent_id, thread, binding)
                .await?;
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
impl awaken_session_execution_contract::CoordinationRuntime for RehydrateFake {
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
        awaken_session_error_contract::RunError,
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
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivation,
        awaken_session_error_contract::RunError,
    > {
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
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.order.lock().unwrap().push("delegations");
        Ok(self.delegated.lock().unwrap().clone())
    }

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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    async fn coordinated_threads(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.reads
            .on_read("coordination")
            .await
            .map_err(scripted_read_failure)?;
        self.order.lock().unwrap().push("coordination");
        Ok(self.coordinated.lock().unwrap().clone())
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
        awaken_session_error_contract::RunError,
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        let thread_id = command.target.thread_id(&command.session_id);
        let pending = self
            .pending_by_thread
            .lock()
            .unwrap()
            .get(&thread_id.0)
            .cloned()
            .filter(|pending| pending.tool_use_id == command.tool_use_id)
            .ok_or_else(|| RunError::bad_request("reply does not match the pending ticket"))?;
        let run_id = match &command.target {
            awaken_session_execution_contract::SessionThreadTarget::Primary => {
                command.expected_run_id.clone()
            }
            awaken_session_execution_contract::SessionThreadTarget::Child(_) => self
                .coordinated
                .lock()
                .unwrap()
                .iter()
                .find(|link| link.thread_id == thread_id)
                .and_then(|link| link.latest_run_id.clone())
                .ok_or_else(|| RunError::bad_request("coordinated child has no committed Run"))?,
        };
        if run_id != command.expected_run_id
            || command.expected_correlation_id != format!("ticket:{}", pending.tool_use_id)
        {
            return Err(RunError::bad_request("reply ticket fence is stale"));
        }
        Ok(
            awaken_session_execution_contract::SessionThreadToolReplyFence {
                activity:
                    awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
                        prior_epoch: Some(1),
                    },
                already_applied: false,
            },
        )
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let command = delivery.command;
        let thread_id = command.target.thread_id(&command.session_id);
        let expected_fence = self.session_thread_tool_reply_fence(&command).await?;
        if delivery.fence != expected_fence {
            return Err(RunError::bad_request("reply fence is stale"));
        }
        let removed = self.pending_by_thread.lock().unwrap().remove(&thread_id.0);
        if removed.as_ref().map(|pending| pending.tool_use_id.as_str())
            != Some(command.tool_use_id.as_str())
        {
            return Err(RunError::bad_request(
                "reply does not match the coordinated child pending ticket",
            ));
        }
        self.thread_tool_replies.lock().unwrap().push(command);
        Ok(())
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
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
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
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .thread_reads()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "ThreadReadRuntime is unsupported by this runtime",
                    )
                })?
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for RehydrateFake {
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resume_budget_reached(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Vec<awaken_session_application_contract::SessionBudgetResumeTicket>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_budget_resume_tickets(session_id)
            .await
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.reads
            .on_read("usage")
            .await
            .map_err(scripted_read_failure)?;
        Ok(self
            .usage_by_thread
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
            .unwrap_or_default())
    }

    async fn session_thread_usage(
        &self,
        _session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        self.reads
            .on_read("child_usage")
            .await
            .map_err(scripted_read_failure)?;
        Ok(self
            .usage_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        self.run_streaming(agent, thread, content, sink).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
    }

    async fn pending_tool(
        &self,
        _thread: &str,
    ) -> Result<Option<awaken_session_application_contract::Pending>, RunError> {
        Ok(self.pending.lock().unwrap().clone())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for RehydrateFake {
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .replace_session_tools(thread, tools)
            .await
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        if self
            .reject_environment_adoption
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::internal("sandbox is owned by another runtime"));
        }
        self.order.lock().unwrap().push("environment");
        self.restored_environments.lock().unwrap().push((
            agent.to_string(),
            thread.to_string(),
            binding.to_string(),
        ));
        Ok(())
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
        self.order.lock().unwrap().push("resources");
        self.restored.lock().unwrap().push((
            thread.to_string(),
            transition.desired().workspace_id.clone(),
            transition.desired().resources.clone(),
        ));
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.order.lock().unwrap().push("history");
        if let Some(messages) = self
            .committed_by_thread
            .lock()
            .unwrap()
            .get(thread)
            .cloned()
        {
            return Ok(messages);
        }
        if let Some(messages) = self.committed.lock().unwrap().clone() {
            return Ok(messages);
        }
        Ok(vec![Message::text(
            awaken_agent_contract::agent::message::Id(format!("{thread}-m0")),
            awaken_agent_contract::agent::message::Role::User,
            "hello",
        )])
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.reads
            .on_read(if thread_id == session_id {
                "root_snapshot"
            } else {
                "child_snapshot"
            })
            .await
            .map_err(scripted_read_failure)?;
        self.order.lock().unwrap().push(if thread_id == session_id {
            "root_snapshot"
        } else {
            "child_snapshot"
        });
        let lifecycle = self.lifecycle.lock().unwrap().clone();
        let has_outcome_projection = self
            .outcome_projections
            .lock()
            .unwrap()
            .keys()
            .any(|(owner, _)| owner == thread_id);
        let explicit_run_id = self
            .run_ids_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned();
        let latest_run_id = if thread_id == session_id {
            lifecycle
                .iter()
                .rev()
                .find(|event| event.thread_id.0 == thread_id)
                .map(|event| event.run_id.clone())
                .or(explicit_run_id.clone())
                .or_else(|| {
                    (self.committed.lock().unwrap().is_some()
                        || self.pending.lock().unwrap().is_some()
                        || has_outcome_projection)
                        .then(|| {
                            awaken_agent_contract::agent::run::Id(format!(
                                "test-root-run:{thread_id}"
                            ))
                        })
                })
        } else {
            self.coordinated
                .lock()
                .unwrap()
                .iter()
                .find(|link| link.thread_id.0 == thread_id)
                .and_then(|link| link.latest_run_id.clone())
        };
        let Some(latest_run_id) = latest_run_id else {
            return Ok(None);
        };
        let messages = if thread_id == session_id {
            self.committed_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .or_else(|| self.committed.lock().unwrap().clone())
                .unwrap_or_default()
        } else {
            self.committed_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .unwrap_or_default()
        };
        let pending = if thread_id == session_id {
            self.pending.lock().unwrap().clone()
        } else {
            self.pending_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
        };
        let resume_tickets: Vec<awaken_agent_contract::thread::read::recovery::RunResumeTicket> = pending
            .map(|pending| {
                let reason = if pending.client_executed {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution
                } else {
                    awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission
                };
                awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: latest_run_id.clone(),
                    ticket: awaken_agent_contract::agent::awaiting::ResumeTicket::new(
                        format!("ticket:{}", pending.tool_use_id),
                        latest_run_id.clone(),
                        awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                        "test-snapshot",
                        "test-catalog",
                        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                            reason,
                            call_id: pending.tool_use_id,
                            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                                tool_id: pending.name,
                                arguments: pending.input,
                            },
                            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                        },
                    ),
                }
            })
            .into_iter()
            .collect();
        let mut state = self
            .state_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_default();
        if let Some(usage) = self.usage_by_thread.lock().unwrap().get(thread_id).cloned() {
            // Production derives accounting from this exact committed state
            // prefix. Keep the shared fake structurally faithful instead of
            // maintaining a test-only second usage read path.
            let mut attributed = awaken_runtime_contract::llm::ThreadUsage::default();
            if usage.by_model.is_empty() {
                attributed.record(
                    "test-fixture",
                    awaken_runtime_contract::llm::TokenUsage {
                        prompt_tokens: usage.input_tokens,
                        completion_tokens: usage.output_tokens,
                        cache_read_tokens: usage.cache_read_tokens,
                        cache_creation_tokens: usage.cache_creation_tokens,
                    },
                );
            } else {
                for (model, model_usage) in usage.by_model {
                    attributed.record(
                        &model,
                        awaken_runtime_contract::llm::TokenUsage {
                            prompt_tokens: model_usage.input_tokens,
                            completion_tokens: model_usage.output_tokens,
                            cache_read_tokens: model_usage.cache_read_tokens,
                            cache_creation_tokens: model_usage.cache_creation_tokens,
                        },
                    );
                }
            }
            state.push(awaken_runtime_contract::llm::ThreadUsageKey::write(
                &attributed,
            ));
        }
        if self
            .disposition_by_thread
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            == Some(&awaken_agent_contract::ThreadDisposition::Archived)
        {
            state.push(awaken_agent_contract::archive_thread_command());
        }
        for ((owner, outcome_id), projection) in self.outcome_projections.lock().unwrap().iter() {
            if owner != thread_id {
                continue;
            }
            match projection {
                awaken_session_execution_contract::CommittedOutcomeProjection::Completed(report) => {
                    for iteration in &report.iterations {
                        state.push(awaken_agent_contract::agent::state::Command::set(
                            awaken_agent_contract::agent::state::Scope::Thread,
                            awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                            format!("outcome/{outcome_id}/evaluation/{}", iteration.iteration),
                            serde_json::json!({"fixture": "committed"}),
                        ));
                    }
                }
                awaken_session_execution_contract::CommittedOutcomeProjection::Errored(_) => {}
            }
            // A terminal Outcome commit always updates its owned state and
            // removes the active pointer at the same commit coordinate. Keep
            // the fake structurally faithful so projector tests cannot accept a
            // partial transaction that no production coordinator emits.
            state.push(awaken_agent_contract::agent::state::Command::set(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                format!("outcome/{outcome_id}/state"),
                serde_json::json!({"fixture": "terminal"}),
            ));
            state.push(awaken_agent_contract::agent::state::Command::remove(
                awaken_agent_contract::agent::state::Scope::Thread,
                awaken_agent_contract::agent::state::MergePolicy::Disjoint,
                "outcome/active",
            ));
        }
        let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
        for event in lifecycle
            .iter()
            .filter(|event| event.thread_id.0 == thread_id)
        {
            if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                run.state = event.state.clone();
            } else {
                runs.push(awaken_agent_contract::agent::run::Record {
                    id: event.run_id.clone(),
                    thread_id: event.thread_id.clone(),
                    state: event.state.clone(),
                });
            }
        }
        if let Some(run_id) = explicit_run_id
            && !runs.iter().any(|run| run.id == run_id)
        {
            runs.push(awaken_agent_contract::agent::run::Record {
                id: run_id,
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                state: awaken_agent_contract::agent::run::RunState::Running,
            });
        }
        // Honor the recovery contract: a latest coordinate always has its
        // committed Run record. A fixture Run without scripted lifecycle is
        // Awaiting exactly when the fixture exposes its ticket, else Running.
        if !runs.iter().any(|run| run.id == latest_run_id) {
            let awaiting = resume_tickets
                .iter()
                .any(|ticket| ticket.run_id == latest_run_id);
            runs.push(awaken_agent_contract::agent::run::Record {
                id: latest_run_id.clone(),
                thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
                state: if awaiting {
                    awaken_agent_contract::agent::run::RunState::Awaiting
                } else {
                    awaken_agent_contract::agent::run::RunState::Running
                },
            });
        }
        let base_store_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .max()
            .unwrap_or_default();
        let base_store_cursor = if thread_id == session_id {
            self.root_store_cursor_override
                .lock()
                .unwrap()
                .unwrap_or(base_store_cursor)
        } else {
            base_store_cursor
        };
        let atomic_fixture_cursor = if messages.is_empty() && state.is_empty() {
            base_store_cursor
        } else {
            base_store_cursor.max(1)
        };
        let message_commit_cursors = self
            .message_cursors_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| vec![atomic_fixture_cursor; messages.len()]);
        let state_commit_cursors = self
            .state_cursors_by_thread
            .lock()
            .unwrap()
            .get(thread_id)
            .cloned()
            .unwrap_or_else(|| vec![atomic_fixture_cursor; state.len()]);
        let store_cursor = std::iter::once(base_store_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        let snapshot = awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            commit_timestamps: self
                .commit_times_by_thread
                .lock()
                .unwrap()
                .get(thread_id)
                .cloned()
                .unwrap_or_default(),
            thread_id: awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            claimed_run_id: latest_run_id.clone(),
            runs,
            latest_run_id: Some(latest_run_id),
            messages,
            message_commit_cursors,
            state,
            state_commit_cursors,
            events: crate::test_support::lifecycle_audit_records(lifecycle.iter().filter(
                |event| {
                    event.thread_id.0 == thread_id && event.source_commit_cursor <= store_cursor
                },
            )),
            resume_tickets,
            thread_version: lifecycle.len() as u64,
            store_cursor,
            next_commit_ordinal: 0,
        };
        self.commit_staged_child_prefix_after_snapshot(thread_id);
        Ok(Some(snapshot))
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
        awaken_session_error_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        Ok(self
            .disposition_by_thread
            .lock()
            .unwrap()
            .get(&(session_id.to_string(), thread_id.to_string()))
            .copied()
            .unwrap_or_default())
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        let key = (session_id.to_string(), thread_id.to_string());
        let mut dispositions = self.disposition_by_thread.lock().unwrap();
        if dispositions.get(&key) != Some(&awaken_agent_contract::ThreadDisposition::Archived) {
            dispositions.insert(
                key.clone(),
                awaken_agent_contract::ThreadDisposition::Archived,
            );
            self.archive_commits.lock().unwrap().push(key);
        }
        Ok(())
    }

    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError> {
        self.reads
            .on_read("outcome")
            .await
            .map_err(scripted_read_failure)?;
        Ok(self
            .outcome_projections
            .lock()
            .unwrap()
            .get(&(thread.to_string(), outcome_id.to_string()))
            .cloned())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for RehydrateFake {
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        self.ended
            .lock()
            .unwrap()
            .push(effect.command.thread_id.clone());
        crate::test_support::complete_terminal_cleanup_preparation(&effect, &authorization)
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

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(crate::test_support::complete_terminal_cleanup_disposal(
            &effect,
        ))
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
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
impl awaken_session_execution_contract::OutcomeRuntime for RehydrateFake {
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for RehydrateFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The advertised capability surface echoed in the session's agent object. The
    /// default reports nothing; a real host overrides it with its built-in tools,
    /// custom tools, skills, and delegate roster so the session enumerates what it does.
    fn capabilities(&self) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_execution_contract::NeutralRuntime.capabilities()
    }

    fn capabilities_for(
        &self,
        _thread: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_configuration_contract::AgentCapabilities {
            delegates: self.delegate_ids.lock().unwrap().clone(),
            custom_tools: self
                .custom_tools
                .lock()
                .unwrap()
                .iter()
                .map(|tool| awaken_session_configuration_contract::CustomTool {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
            ..Default::default()
        }
    }
}

#[async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RehydrateFake {
    async fn stage_mcp_attachment(
        &self,
        request: awaken_session_mcp_contract::StageMcpAttachment,
    ) -> Result<awaken_session_mcp_contract::McpRealizationReceipt, RunError> {
        self.order.lock().unwrap().push("mcp");
        if let Some(restored) = self
            .restored_runtimes
            .lock()
            .unwrap()
            .iter_mut()
            .find(|restored| restored.0 == request.generation.session_id)
        {
            restored.2 += 1;
        }
        let receipt_fingerprint = request.fingerprint();
        Ok(awaken_session_mcp_contract::McpRealizationReceipt {
            generation: request.generation,
            realization_id: request.realization_id,
            selected_plaintext_holder: request.selected_plaintext_holder,
            actual_realization_kind: None,
            receipt_fingerprint,
        })
    }

    async fn publish_mcp_generation(
        &self,
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        if self
            .reject_mcp_publication
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RunError::unavailable("secret-publication-canary"));
        }
        Ok(())
    }

    async fn drain_mcp_generation(
        &self,
        _generation: awaken_session_mcp_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        Ok(())
    }
}
