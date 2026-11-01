//! The rehydrating Session Runtime fake the Managed adapter's state tests (and
//! the Coordinator's Session adoption tests) drive: it reports committed Thread
//! facts after the durable Session root authorized rehydration, and scripts
//! read failures, pauses and MCP publication refusals.

use awaken_session_execution_contract::{
    EnvironmentRuntime as _, SessionCoordinationReads as _, StreamingRuns as _, ThreadReads as _,
};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::PersistedSession;
use awaken_session_store::SqliteManagedSessionRepository;

use awaken_session_application_contract::DelegatedRun;
use awaken_session_execution_contract::SessionRuns;
use awaken_session_types::RunError;

use crate::read_control::ReadControl;
use awaken_session_application_contract::StepOutcome;

pub(crate) fn scripted_read_failure(port: &'static str) -> RunError {
    RunError::unavailable(format!("scripted {port} read failure"))
}

pub async fn ephemeral_session_repo() -> SqliteManagedSessionRepository {
    SqliteManagedSessionRepository::open_in_memory()
        .await
        .expect("open ephemeral managed Session repository")
}

pub async fn create_session_fixture(
    repo: &dyn awaken_session_application::SessionStore,
    owner: &str,
    mut session: PersistedSession,
) {
    session.revision = awaken_session_types::SessionRevision(0);
    let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    repo.create(
        owner,
        session.clone(),
        awaken_session_contract::IdempotencyRecord {
            key: format!("test:create:{}:{payload_hash}", session.session_id),
            payload_hash,
        },
        Vec::new(),
    )
    .await
    .expect("create Session fixture");
}

pub type RestoredRuntime = (
    String,
    Option<String>,
    usize,
    awaken_session_configuration_contract::SessionNetworkPolicy,
    awaken_provisioning_contract::SandboxOverride,
);

pub type ChildCommitAfterHistorySnapshot = (
    String,
    Vec<Message>,
    Vec<awaken_agent_contract::RunLifecycleEvent>,
);

/// A runtime that reports committed Thread facts after the durable Session root
/// has authorized rehydration. The transcript is projection input only and can
/// never establish Session existence by itself.
#[derive(Clone, Default)]
pub struct RehydrateFake {
    pub reads: ReadControl,
    pub commit_times_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<String, std::collections::BTreeMap<u64, u64>>,
        >,
    >,
    pub restored: Arc<
        awaken_sync::LeafLock<
            Vec<(
                String,
                String,
                awaken_session_configuration_contract::resource::ResolvedSessionResources,
            )>,
        >,
    >,
    pub restored_environments: Arc<awaken_sync::LeafLock<Vec<(String, String, String)>>>,
    pub restored_runtimes: Arc<awaken_sync::LeafLock<Vec<RestoredRuntime>>>,
    pub delegated: Arc<awaken_sync::LeafLock<Vec<DelegatedRun>>>,
    pub coordinated:
        Arc<awaken_sync::LeafLock<Vec<awaken_session_execution_contract::CoordinatedThreadLink>>>,
    pub lifecycle: Arc<awaken_sync::LeafLock<Vec<awaken_agent_contract::RunLifecycleEvent>>>,
    /// Test-only interleaving control for lifecycle projection: when set, a
    /// root recovery read exposes this older backend watermark while the
    /// lifecycle feed may already contain newer commits.
    pub root_store_cursor_override: Arc<awaken_sync::LeafLock<Option<u64>>>,
    pub pending_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<String, awaken_session_application_contract::Pending>,
        >,
    >,
    pub disposition_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<(String, String), awaken_agent_contract::ThreadDisposition>,
        >,
    >,
    pub archive_commits: Arc<awaken_sync::LeafLock<Vec<(String, String)>>>,
    pub thread_tool_replies:
        Arc<awaken_sync::LeafLock<Vec<awaken_session_contract::SessionThreadToolReplyCommand>>>,
    pub delegate_ids: Arc<awaken_sync::LeafLock<Vec<String>>>,
    pub custom_tools:
        Arc<awaken_sync::LeafLock<Vec<awaken_session_configuration_contract::CustomTool>>>,
    pub order: Arc<awaken_sync::LeafLock<Vec<&'static str>>>,
    pub committed: Arc<awaken_sync::LeafLock<Option<Vec<Message>>>>,
    pub committed_by_thread:
        Arc<awaken_sync::LeafLock<std::collections::HashMap<String, Vec<Message>>>>,
    /// Explicit Run records used by canonical coordination fixtures whose
    /// accepted root transcript is committed before any root lifecycle edge.
    pub run_ids_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<String, awaken_agent_contract::agent::run::Id>,
        >,
    >,
    /// Exact fake commit coordinates aligned with `committed_by_thread`.
    /// Absent entries mean the fixture committed its configured messages in
    /// one atomic prefix, not that production may omit the coordinates.
    pub message_cursors_by_thread:
        Arc<awaken_sync::LeafLock<std::collections::HashMap<String, Vec<u64>>>>,
    pub state_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<String, Vec<awaken_agent_contract::agent::state::Command>>,
        >,
    >,
    pub state_cursors_by_thread:
        Arc<awaken_sync::LeafLock<std::collections::HashMap<String, Vec<u64>>>>,
    pub outcome_projections: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<
                (String, String),
                awaken_session_execution_contract::CommittedOutcomeProjection,
            >,
        >,
    >,
    pub usage_by_thread: Arc<
        awaken_sync::LeafLock<
            std::collections::HashMap<String, awaken_session_contract::SessionUsage>,
        >,
    >,
    /// Test-only commit interleaving: return the current child transcript, then
    /// atomically expose the supplied newer transcript/lifecycle facts. This
    /// models a terminal commit racing a projector read without a second fake.
    pub child_commit_after_history_snapshot:
        Arc<awaken_sync::LeafLock<Option<ChildCommitAfterHistorySnapshot>>>,
    pub pending: Arc<awaken_sync::LeafLock<Option<awaken_session_application_contract::Pending>>>,
    /// `RunStateChanged.await_target` of an Awaiting commit that opened a
    /// ticket, by lifecycle cursor; absent models a commit left unaudited.
    pub await_targets: Arc<awaken_sync::LeafLock<crate::AwaitTargets>>,
    /// Every state command each Thread has exposed, with the commit it first
    /// appeared at: committed state is append-only, so a later snapshot
    /// keeps an earlier value and its coordinate.
    pub state_log: Arc<awaken_sync::LeafLock<std::collections::HashMap<String, crate::StateLog>>>,
    /// Awaiting commits that model a legacy audit row without a target.
    pub unaudited_awaits: Arc<
        awaken_sync::LeafLock<std::collections::HashSet<awaken_agent_contract::RunLifecycleCursor>>,
    >,
    pub ended: Arc<awaken_sync::LeafLock<Vec<String>>>,
    pub reject_mcp_publication: Arc<std::sync::atomic::AtomicBool>,
    pub reject_environment_adoption: Arc<std::sync::atomic::AtomicBool>,
}

impl RehydrateFake {
    /// Install the one Runtime-owned accepted `send_message` prefix that
    /// authorizes an ordinary coordinated link. Tests must not fabricate a
    /// Managed-only link anchor: the root ToolUse/ToolResult messages, their
    /// exact commit coordinate, the root Run, and the relationship are one
    /// recovery fixture.
    pub fn install_agent_coordination_prefix(
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
            awaken_tool_contract::tool_batch::ToolBatch::operation_id_for_step(
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
            .get(session_id)
            .cloned()
            .or_else(|| self.committed.lock().clone())
            .unwrap_or_default();
        let mut cursors = self
            .message_cursors_by_thread
            .lock()
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
            .insert(session_id.to_string(), cursors);
        self.committed_by_thread
            .lock()
            .insert(session_id.to_string(), messages);
        self.run_ids_by_thread
            .lock()
            .insert(session_id.to_string(), root_run_id);
        self.coordinated.lock().push(link);
    }

    pub(crate) fn commit_staged_child_prefix_after_snapshot(&self, thread_id: &str) {
        let staged = self.child_commit_after_history_snapshot.lock().take();
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
                    .insert(target_thread.clone(), vec![message_cursor; messages.len()]);
                self.committed_by_thread
                    .lock()
                    .insert(target_thread, messages);
                self.lifecycle.lock().extend(lifecycle);
            } else {
                *self.child_commit_after_history_snapshot.lock() =
                    Some((target_thread, messages, lifecycle));
            }
        }
    }
}

#[async_trait]
impl SessionRuns for RehydrateFake {
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
        _decision: PermissionDecision,
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
}

awaken_session_execution_contract::neutral_role!(LiveInboxAccess for RehydrateFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for RehydrateFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
        crate::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_execution_contract::SessionRealizationControl,
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
        projection: awaken_session_execution_contract::FrozenSessionProjection,
        mode: awaken_session_execution_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure> {
        let adopts_environment = mode.adopts_resident_environment();
        if let Some(init) = crate::complete_session_projection_init(thread, &projection, &mode)? {
            self.order.lock().push("runtime");
            self.restored_runtimes.lock().push((
                thread.to_string(),
                init.runtime,
                0,
                init.environment.network,
                init.environment.sandbox,
            ));
        }
        if adopts_environment && let Some(binding) = projection.environment.binding() {
            self.adopt_session_environment(&projection.baseline.agent_id, thread, binding)
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
        lease: awaken_session_execution_contract::ObservedSessionRealizationLease,
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
impl awaken_session_execution_contract::ChildRunCoordination for RehydrateFake {
    // Fixture rule P1: rehydration observes projections without running a prepared command.
    // A prepared command refuses before activity admission or fixture mutation.
    async fn run_prepared_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitPreparedSessionRun,
        activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
    ) -> Result<awaken_agent_contract::agent::run::RunState, awaken_session_types::RunError> {
        awaken_session_execution_contract::ChildRunCoordination::run_prepared_session_run(
            &awaken_session_execution_contract::NeutralRuntime,
            command,
            activity,
        )
        .await
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        let key = (session_id.to_string(), thread_id.to_string());
        let mut dispositions = self.disposition_by_thread.lock();
        if dispositions.get(&key) != Some(&awaken_agent_contract::ThreadDisposition::Archived) {
            dispositions.insert(
                key.clone(),
                awaken_agent_contract::ThreadDisposition::Archived,
            );
            self.archive_commits.lock().push(key);
        }
        Ok(())
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
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivation,
        awaken_session_types::RunError,
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
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let command = delivery.command;
        let thread_id = command.target.thread_id(&command.session_id);
        let expected_fence = self.session_thread_tool_reply_fence(&command).await?;
        if delivery.fence != expected_fence {
            return Err(RunError::bad_request("reply fence is stale"));
        }
        let removed = self.pending_by_thread.lock().remove(&thread_id.0);
        if removed.as_ref().map(|pending| pending.tool_use_id.as_str())
            != Some(command.tool_use_id.as_str())
        {
            return Err(RunError::bad_request(
                "reply does not match the coordinated child pending ticket",
            ));
        }
        self.thread_tool_replies.lock().push(command);
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
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
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
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for RehydrateFake {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_contract::PersistedSession,
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

    async fn delegated_runs(&self, _thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.order.lock().push("delegations");
        Ok(self.delegated.lock().clone())
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
        self.order.lock().push("coordination");
        Ok(self.coordinated.lock().clone())
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        let thread_id = command.target.thread_id(&command.session_id);
        let pending = self
            .pending_by_thread
            .lock()
            .get(&thread_id.0)
            .cloned()
            .filter(|pending| pending.tool_use_id == command.tool_use_id)
            .ok_or_else(|| RunError::bad_request("reply does not match the pending ticket"))?;
        let run_id = match &command.target {
            awaken_session_contract::SessionThreadTarget::Primary => {
                command.expected_run_id.clone()
            }
            awaken_session_contract::SessionThreadTarget::Child(_) => self
                .coordinated
                .lock()
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
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            activity: awaken_session_contract::SessionThreadToolReplyActivity::Session {
                prior_epoch: Some(1),
            },
            already_applied: false,
        })
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
