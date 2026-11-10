// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Coordinated Thread admission, replies, interruption and archive validate the committed
// @formal reason: Thread ticket, Session activity and exact dispatch row before delivery; the committed
// @formal reason: Session partition and the dispatch owner are the only authorities.
//! [`CoordinatedThreads`]: durable admission and derived reads for the Agent
//! coordination of a Session.
//!
//! A child remains an ordinary `RunDispatch` whose logical Thread is stored in
//! the parent Session's commit partition. Relationships are reconstructed from
//! the parent's committed `send_message` tool pairs; this module owns no
//! registry, receipt map, mailbox, or background executor. Every process that
//! admits a Session's coordinated Runs (the Coordinator over its dispatch
//! projections, a co-located executor over its installed Sessions) supplies
//! the Session's frozen inputs through [`SessionLowering`].

mod agent_message_source;
mod session_reply_receipt;
mod terminal_quiescence;

use session_reply_receipt::SessionReplyReceipt;
pub use session_reply_receipt::validate_awaiting_tool_authority;
pub use terminal_quiescence::{ResidentAttempt, RootAttempt};

use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::{AwaitTarget, ResumeTicket};
use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView as _;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource as _;
use awaken_dispatch_contract::{
    DispatchEnqueue as _, DispatchReads as _, PendingInput, SessionContinuationOutbox as _,
    SessionRunReservations as _,
};
use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::DispatchPublication;
use awaken_dispatch_execution::coordinated_run_activation;
use awaken_dispatch_execution::durable_control::DurableRunControl;
use awaken_dispatch_execution::session_dispatch::DispatchPreparation;
use awaken_runtime_contract::runs::{DelegatedRun, Pending, RunError};

use awaken_runtime_contract::{
    CancellationToken, ExecutableAgentSnapshot, PublishedAgentSnapshotSource, RunActivation,
};
use awaken_session_configuration_contract::resource::SessionResourceManifest;
use awaken_session_configuration_contract::{SessionBaseline, SessionToolConfiguration};
use awaken_session_execution_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind, CoordinatedThreadTarget,
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentReportContinuation,
    coordinated_thread_closed,
};

use crate::{CommittedReads, session_thread_reply_result};

/// The frozen inputs a coordinated child Run inherits from its Session.
pub struct ChildRunInputs {
    pub baseline: SessionBaseline,
    pub tools: Option<SessionToolConfiguration>,
    /// The Resource generation the Session's Runs carry.
    pub manifest: SessionResourceManifest,
    /// The coordinator's own publication, whose toolsets a child inherits when
    /// the Session replaces none.
    pub coordinator_publication: Option<ExecutableAgentSnapshot>,
}

/// The Session's frozen inputs a coordination command lowers over.
#[async_trait::async_trait]
pub trait SessionLowering: Send + Sync {
    /// The frozen inputs a coordinated child Run of `session` inherits.
    fn child_run_inputs(&self, session: &str) -> Result<ChildRunInputs, RunError>;
    /// The snapshot a root continuation Run of `session` executes.
    async fn continuation_snapshot(
        &self,
        session: &str,
    ) -> Result<ExecutableAgentSnapshot, RunError>;
    /// The snapshot a foreground root resume of `session` executes.
    async fn resume_snapshot(&self, session: &str) -> Result<ExecutableAgentSnapshot, RunError>;
    /// The Session's frozen model selection.
    fn model_ref_override(&self, session: &str) -> Result<Option<String>, RunError>;
    /// The dispatch preparation of `activation` over the installed Sessions.
    fn prepare(&self, activation: RunActivation) -> DispatchPreparation<'_>;
}

/// The Agent coordination of every Session this process admits Runs for.
/// Consumers: the Session application's coordination command and read roles.
pub struct CoordinatedThreads {
    owner: Option<Arc<AnyCoordinatorDispatch>>,
    publication: DispatchPublication,
    reads: Arc<CommittedReads>,
    lowering: Arc<dyn SessionLowering>,
    publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
    wake: Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>,
    clock: Arc<dyn awaken_time::WallClock>,
}

/// What [`CoordinatedThreads`] admits through and reads from.
pub struct CoordinationAuthorities {
    /// The durable dispatch owner; a process without one admits nothing.
    pub owner: Option<Arc<AnyCoordinatorDispatch>>,
    pub publication: DispatchPublication,
    pub reads: Arc<CommittedReads>,
    pub lowering: Arc<dyn SessionLowering>,
    pub publications: Option<Arc<dyn PublishedAgentSnapshotSource>>,
    /// The cross-node dispatch wake, a latency hint only.
    pub wake: Option<Arc<dyn awaken_dispatch_contract::WakeSignal>>,
    pub clock: Arc<dyn awaken_time::WallClock>,
}

impl CoordinatedThreads {
    #[must_use]
    pub fn new(authorities: CoordinationAuthorities) -> Self {
        let CoordinationAuthorities {
            owner,
            publication,
            reads,
            lowering,
            publications,
            wake,
            clock,
        } = authorities;
        Self {
            owner,
            publication,
            reads,
            lowering,
            publications,
            wake,
            clock,
        }
    }

    /// The Coordinator dispatch owner coordinated Runs are admitted through.
    pub fn owner(&self) -> Result<&Arc<AnyCoordinatorDispatch>, RunError> {
        self.owner
            .as_ref()
            .ok_or_else(|| RunError::internal("operation requires a Coordinator dispatch owner"))
    }

    /// The same immutable publication the Session's budget and coordination use.
    #[must_use]
    pub fn publication(&self) -> &DispatchPublication {
        &self.publication
    }

    /// The durable Run control over the dispatch owner.
    pub fn durable_run_control(&self) -> Result<DurableRunControl, RunError> {
        Ok(self.publication.durable_run_control(self.owner()?.clone()))
    }
}

fn reply_dispatch_activity(
    dispatch: Option<&awaken_dispatch_contract::DispatchSummary>,
    thread: &ThreadId,
    parent: &ThreadId,
) -> Result<awaken_session_contract::SessionThreadToolReplyActivity, RunError> {
    use awaken_session_contract::SessionThreadToolReplyActivity;
    match dispatch {
        Some(row) if row.thread_id == *thread => SessionThreadToolReplyActivity::from_dispatch(
            thread,
            row.session_thread_id.as_ref(),
            row.session_activity_epoch,
            parent,
        )
        .ok_or_else(|| {
            RunError::bad_request("Session Thread reply dispatch affinity is inconsistent")
        }),
        Some(_) => Err(RunError::bad_request(
            "Session Thread reply dispatch affinity is inconsistent",
        )),
        None if thread == parent => {
            Ok(SessionThreadToolReplyActivity::Session { prior_epoch: None })
        }
        None => Err(RunError::internal(
            "Session child Thread reply has no exact durable dispatch row",
        )),
    }
}

pub fn dispatch_admission_error(error: awaken_dispatch_contract::DispatchError) -> RunError {
    match error {
        awaken_dispatch_contract::DispatchError::Rejected(message) => {
            RunError::bad_request(message)
        }
        awaken_dispatch_contract::DispatchError::Conflict(message) => {
            RunError::unavailable(message)
        }
        awaken_dispatch_contract::DispatchError::Unavailable(message) => {
            RunError::unavailable(message)
        }
    }
}

async fn finish_coordinated_child_admission(
    host: &CoordinatedThreads,
    session_id: &str,
    thread_id: ThreadId,
    intent: CoordinatedRunIntent,
) -> Result<SessionAgentMessageReceipt, RunError> {
    // Thread disposition, Run commits, and dispatch admission may use
    // different durable adapters. Re-read both existing admission fences
    // after enqueue: whichever side won records cancellation on this exact
    // dispatch and waits for settlement. No relation flag, terminal cache,
    // or process-local lock is introduced.
    let archived = host
        .reads
        .session_thread_disposition(session_id, &thread_id)
        .await?
        == awaken_agent_contract::ThreadDisposition::Archived;
    // Follow-up admission above accepted only ordinary Agent Threads.
    let failed = intent == CoordinatedRunIntent::FollowUp
        && host
            .coordinated_thread_is_closed(session_id, &thread_id, CoordinatedThreadKind::Agent)
            .await?;
    if archived || failed {
        // This admission may already be durable, so it is not a definitive
        // caller rejection until cancellation has settled. Once quiescent,
        // the bad request is safe: no accepted child execution remains.
        host.cancel_and_await_coordinated_thread_quiescence(session_id, &thread_id)
            .await?;
        return Err(RunError::bad_request(if archived {
            "Agent Thread is archived"
        } else {
            "Agent Thread terminated after a failed Run"
        }));
    }
    host.publication.hint_local_pool().await;
    Ok(SessionAgentMessageReceipt { thread_id })
}

impl CoordinatedThreads {
    /// Resolve a public reply to the one currently committed Awaiting ticket.
    /// Thread recovery remains the authority; the Session event identity and
    /// version are only admission fences against delayed or reused call ids.
    pub async fn validated_session_thread_tool_reply(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(ThreadId, ResumeTicket), RunError> {
        if !command.has_complete_coordinates() {
            return Err(RunError::bad_request(
                "Session Thread tool reply is incomplete",
            ));
        }
        let thread_id = command.target.thread_id(&command.session_id);
        if command
            .target
            .child_thread_id()
            .is_some_and(|child| child.0 == command.session_id)
        {
            return Err(RunError::bad_request(
                "a coordinated child Thread must differ from its parent Session",
            ));
        }
        let commit = self
            .reads
            .commit_for_read(command.session_id.as_str())
            .await?;
        let snapshot = commit
            .recovery_snapshot(&thread_id, &command.expected_run_id)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        if command
            .expected_thread_version
            .is_some_and(|expected| expected != snapshot.thread_version)
        {
            return Err(RunError::bad_request(
                "Session Thread awaiting ticket changed after Event admission",
            ));
        }
        let current = snapshot
            .current_run()
            .map_err(|error| RunError::internal(error.to_string()))?;
        if !current.is_some_and(|run| {
            run.id == command.expected_run_id && matches!(run.state, RunState::Awaiting)
        }) {
            return Err(RunError::bad_request("Session Thread has no awaiting Run"));
        }
        let ticket = snapshot
            .answerable_ticket(&command.expected_run_id)
            .map_err(|error| RunError::internal(error.to_string()))?
            .filter(|ticket| ticket.correlation_id == command.expected_correlation_id)
            .ok_or_else(|| {
                RunError::bad_request(
                    "Session Thread awaiting ticket changed after Event admission",
                )
            })?
            .clone();
        if matches!(
            command.reply,
            awaken_session_contract::SessionThreadToolReply::Input(_)
        ) {
            if ticket.call_id().unwrap_or_default() != command.tool_use_id {
                return Err(RunError::bad_request(
                    "Session input call identity differs from its ticket",
                ));
            }
            if matches!(ticket.target(), AwaitTarget::ToolCall { .. }) {
                validate_awaiting_tool_authority(&thread_id, &ticket, &snapshot.state)?;
            }
            let result = session_thread_reply_result(&ticket, &command.tool_use_id, &command.reply);
            awaken_runtime_contract::resume::validate_resume(
                &ticket,
                &awaken_runtime_contract::resume::ResumeCommand::from_ticket(
                    &ticket,
                    result,
                    awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                ),
            )
            .map_err(|error| RunError::bad_request(error.to_string()))?;
            return Ok((thread_id, ticket));
        }
        match ticket.target() {
            AwaitTarget::ToolCall { .. } => {
                validate_awaiting_tool_authority(&thread_id, &ticket, &snapshot.state)?
            }
            // RemoteInput deliberately has no local ToolBatch: its exact Run,
            // Thread, correlation and call id are closed inside ResumeTicket,
            // and `check_pending` below validates the public `agent_input`
            // projection plus client-result kind against that same ticket.
            AwaitTarget::RemoteInput { .. } => {}
            AwaitTarget::Pause(_) => {
                return Err(RunError::bad_request(
                    "Session Thread awaiting ticket is not externally answerable",
                ));
            }
        }
        check_pending(
            &ticket,
            &command.tool_use_id,
            command.reply.client_executed(),
        )?;
        Ok((thread_id, ticket))
    }

    /// Derive the absorbing follow-up fence from the latest committed Run in the
    /// parent Session partition through the contract close rule for the Thread's
    /// kind. No Thread status or dispatch flag shadows that lifecycle truth.
    pub async fn coordinated_thread_is_closed(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        kind: CoordinatedThreadKind,
    ) -> Result<bool, RunError> {
        let commit = self.reads.commit_for_read(session_id).await?;
        Ok(commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(RunError::internal)?
            .is_some_and(|run| coordinated_thread_closed(kind, &run.state)))
    }

    /// Read the exact parent-partition Awaiting coordinate before the Session
    /// aggregate opens a reply activity. Delivery revalidates this fence, which
    /// closes call-id reuse and concurrent-resume races without another store.
    pub async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        // Resolve topology before SessionApplication opens a reply activity.
        self.owner()?;
        let (thread_id, _ticket) = match self.validated_session_thread_tool_reply(command).await {
            Ok(active) => active,
            Err(active_error) => match self.session_thread_tool_reply_receipt(command).await? {
                SessionReplyReceipt::Exact => {
                    return Ok(awaken_session_contract::SessionThreadToolReplyFence {
                        activity:
                            awaken_session_contract::SessionThreadToolReplyActivity::Session {
                                prior_epoch: None,
                            },
                        already_applied: true,
                    });
                }
                SessionReplyReceipt::Conflict => {
                    return Err(RunError::bad_request(
                        "Session Thread awaiting correlation was answered by another reply operation",
                    ));
                }
                SessionReplyReceipt::Absent => return Err(active_error),
            },
        };
        let parent = ThreadId(command.session_id.to_string());
        let dispatch = self
            .owner()?
            .list_dispatches()
            .await
            .map_err(|error| RunError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let activity = reply_dispatch_activity(dispatch.as_ref(), &thread_id, &parent)?;
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            activity,
            already_applied: false,
        })
    }

    /// Deliver a typed reply to one logical Session Thread while retaining the
    /// Session as the physical commit/history partition. The dispatch row and
    /// committed ticket are the only routing authorities; no child-named
    /// Session context or store is opened.
    pub async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        let awaken_session_contract::SessionThreadToolReplyDelivery {
            command,
            fence,
            session_activity_epoch,
        } = delivery;
        let (thread_id, ticket) = match self.validated_session_thread_tool_reply(&command).await {
            Ok(_) if fence.already_applied => {
                return Err(RunError::internal(
                    "Session Thread reply became active after an applied-receipt fence",
                ));
            }
            Ok(active) => active,
            Err(active_error) => match self.session_thread_tool_reply_receipt(&command).await? {
                SessionReplyReceipt::Exact => return Ok(()),
                SessionReplyReceipt::Conflict => {
                    return Err(RunError::bad_request(
                        "Session Thread awaiting correlation was answered by another reply operation",
                    ));
                }
                SessionReplyReceipt::Absent if fence.already_applied => {
                    return Err(RunError::internal(
                        "Session Thread reply receipt disappeared after fencing",
                    ));
                }
                SessionReplyReceipt::Absent => return Err(active_error),
            },
        };
        let result = session_thread_reply_result(&ticket, &command.tool_use_id, &command.reply);

        let store = self.owner()?.clone();
        let parent = ThreadId(command.session_id.to_string());
        let dispatch = store
            .list_dispatches()
            .await
            .map_err(|error| RunError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let activity = reply_dispatch_activity(dispatch.as_ref(), &thread_id, &parent)?;
        if std::mem::discriminant(&activity) != std::mem::discriminant(&fence.activity) {
            return Err(RunError::bad_request(
                "Session Thread reply ownership changed after admission",
            ));
        }
        let store = if dispatch.is_some() {
            store
        } else {
            let epoch = session_activity_epoch
                .ok_or_else(|| RunError::internal("foreground Session reply requires activity"))?;
            self.enqueue_foreground_session_resume_dispatch(
                command.session_id.as_str(),
                &ticket,
                epoch,
            )
            .await?
        };
        let context_messages = command
            .accompanying_system
            .as_ref()
            .map(|system| crate::session_system_message(command.session_id.as_str(), system))
            .transpose()
            .map_err(|error| RunError::bad_request(error.message))?
            .into_iter()
            .collect::<Vec<_>>();
        let message_id = command.delivery_operation_id();
        let input = PendingInput {
            message_id,
            run_id: command.expected_run_id,
            thread_id,
            correlation_id: command.expected_correlation_id,
            available_at_ms: None,
            result,
            context_messages,
        };
        store
            .stage_session_resume(
                input,
                &parent,
                fence.activity.prior_epoch(),
                session_activity_epoch,
            )
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        self.publication
            .publish_staged(store.as_ref())
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(())
    }

    async fn coordinated_child_admission(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        intent: CoordinatedRunIntent,
        max_unarchived_threads: usize,
        task_callback: Option<(
            &awaken_session_execution_contract::AdmitSessionRun,
            &awaken_agent_contract::agent::delegation::DelegationOrigin,
        )>,
        root_agent_id: &str,
    ) -> Result<awaken_dispatch_contract::SessionChildAdmission, RunError> {
        if max_unarchived_threads == 0 {
            return Err(RunError::bad_request(
                "Session child capacity must be greater than zero",
            ));
        }
        let links = self.reads.coordinated_threads(session_id, None).await?;
        if intent == CoordinatedRunIntent::FollowUp {
            let kind = if let Some((task, origin)) = task_callback {
                // This callback addresses an existing Native or coordinated
                // Thread. It is not permission to create another child, and it
                // retains the same archive/failed-Run fences below and after enqueue.
                let Some((agent, recorded_origin)) = self
                    .reads
                    .task_callback_source(
                        &awaken_session_types::SessionId::from(session_id),
                        thread_id,
                        root_agent_id,
                    )
                    .await?
                else {
                    return Err(RunError::unavailable(
                        "Task callback creation identity is unavailable",
                    ));
                };
                if agent != task.agent_id || &recorded_origin != origin {
                    return Err(RunError::bad_request(
                        "Task callback target differs from committed creation identity",
                    ));
                }
                if self
                    .reads
                    .session_thread_disposition(session_id, thread_id)
                    .await?
                    == awaken_agent_contract::ThreadDisposition::Archived
                {
                    return Err(RunError::bad_request("Task callback Thread is archived"));
                }
                CoordinatedThreadKind::Agent
            } else {
                links
                    .iter()
                    .find(|link| {
                        link.thread_id == *thread_id
                            && matches!(link.target, CoordinatedThreadTarget::Agent { .. })
                    })
                    .ok_or_else(|| {
                        RunError::bad_request(
                            "follow-up target is not an ordinary coordinated Agent Thread",
                        )
                    })?
                    .target
                    .kind()
            };
            if self
                .coordinated_thread_is_closed(session_id, thread_id, kind)
                .await?
            {
                return Err(RunError::bad_request(
                    "Agent Thread terminated after a failed Run",
                ));
            }
        }

        let mut archived = Vec::new();
        let mut capacity_exempt = Vec::new();
        for link in links {
            match link.target {
                CoordinatedThreadTarget::Advisor { .. } => {
                    capacity_exempt.push(link.thread_id);
                }
                CoordinatedThreadTarget::Agent { .. }
                    if self
                        .reads
                        .session_thread_disposition(session_id, &link.thread_id)
                        .await?
                        == awaken_agent_contract::ThreadDisposition::Archived =>
                {
                    archived.push(link.thread_id);
                }
                CoordinatedThreadTarget::Agent { .. } => {}
            }
        }
        Ok(
            awaken_dispatch_contract::SessionChildAdmission::new(max_unarchived_threads, archived)
                .with_capacity_exempt_threads(capacity_exempt),
        )
    }

    pub async fn admit_coordinated_run(
        &self,
        command: CoordinatedRunCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        let mut command = command;
        let task_attention = command.task_attention.clone();
        let task_execution = command.task_attention_execution.clone();
        let task_origin = task_execution
            .as_ref()
            .and_then(|source| source.0.delegation_origin.clone());
        if task_attention.is_some() != task_origin.is_some()
            || task_attention.is_some() != task_execution.is_some()
        {
            return Err(RunError::bad_request(
                "Task callback requires its original Thread creation origin",
            ));
        }
        if let Some(task) = &task_attention {
            let tenure = task
                .private_task_input
                .as_ref()
                .and_then(|input| input.tenure())
                .ok_or_else(|| RunError::bad_request("Task child command has no bound tenure"))?;
            if task.session_id != command.session_id
                || task.run_id != command.run_id
                || task.agent_id != command.snapshot.root_agent_id.0
                || tenure.thread != command.thread_id.0
                || tenure.session_thread() != command.session_id.as_str()
                || command.intent != CoordinatedRunIntent::FollowUp
            {
                return Err(RunError::bad_request(
                    "Task child command coordinates differ",
                ));
            }
        }
        if let (Some(task), Some(source)) =
            (&task_attention, command.task_attention_execution.as_mut())
        {
            if source.0.snapshot != command.snapshot || source.0.thread_id != command.thread_id {
                return Err(RunError::unavailable(
                    "Task callback execution differs from its original source",
                ));
            }
            source.0 = source
                .0
                .clone()
                .narrow_tools(task.execution_requirements.tool_capability_narrowing)
                .map_err(RunError::bad_request)?;
        }
        let intent = command.intent;
        let max_unarchived_threads = command.max_unarchived_threads;
        let session_id = command.session_id.clone();
        let thread_id = command.thread_id.clone();
        let session_activity_epoch = command
            .session_activity_epoch
            .filter(|epoch| *epoch != 0)
            .ok_or_else(|| {
                RunError::unavailable("fresh coordination requires an original nonzero activity")
            })?;
        let ChildRunInputs {
            baseline,
            tools,
            manifest: resources,
            coordinator_publication,
        } = self.lowering.child_run_inputs(session_id.as_str())?;
        if command.snapshot.metadata.source.revision == 0
            || command.snapshot.metadata.source.agent_id != command.snapshot.root_agent_id
        {
            return Err(RunError::bad_request(
                "coordinated Run does not carry an exact Agent publication",
            ));
        }

        let mut snapshot = command.snapshot;
        let inherit_session_overrides =
            task_attention.is_none() && snapshot.root_agent_id.0 == baseline.agent_id;
        if inherit_session_overrides {
            snapshot = awaken_session_execution_contract::project_effective_agent_publication(
                baseline.model_override.as_ref(),
                &baseline.system_prompt,
                Some(resources.resources.skills()),
                &resources.workspace_id,
                snapshot,
            )
            .map_err(|error| RunError::internal(error.to_string()))?;
            if let Some(tools) = tools.as_ref() {
                crate::project_session_tool_override(
                    &mut snapshot,
                    tools,
                    crate::SessionToolsetProjection::ProjectIntoSnapshot,
                );
            }
        }
        if task_attention.is_none() {
            crate::project_managed_coordination_surface(
                &mut snapshot,
                crate::ManagedCoordinationRole::Child,
            );
        }
        if task_attention.is_none() {
            snapshot.recompute_fingerprint().map_err(|error| {
                RunError::internal(format!("fingerprint coordinated child snapshot: {error}"))
            })?;
        }

        // `send_message` is an Agent/Thread command, not server message
        // ingress.  The source Thread's ActiveToolBatch is the durable request
        // authority; the target's deterministic fresh Run owns the accepted
        // message as frozen activation input.  Dispatch carries that complete
        // Run activation and never stages a parallel PendingInput/outbox fact.
        let coordinator_toolsets = tools
            .as_ref()
            .map(|tools| tools.toolsets.clone())
            .or_else(|| {
                coordinator_publication.as_ref().map(|publication| {
                    publication
                        .resolved_spec
                        .plugin_config
                        .agent
                        .toolsets
                        .clone()
                })
            })
            .ok_or_else(|| {
                RunError::bad_request("coordinated Run has no exact coordinator tool configuration")
            })?;
        command.snapshot = snapshot;
        let root_agent_id = baseline.agent_id;
        let activation =
            coordinated_run_activation(command, root_agent_id.clone(), coordinator_toolsets);
        let mut request = crate::child_dispatch_request(
            activation,
            ThreadId(session_id.to_string()),
            Some(resources),
            self.publications.as_deref(),
            task_execution.as_ref().map(|source| source.1.as_slice()),
        )
        .map_err(|error| {
            if task_attention.is_some() {
                RunError::unavailable(error.to_string())
            } else {
                RunError::bad_request(error.to_string())
            }
        })?
        .with_session_activity_epoch(session_activity_epoch);
        if let Some(task) = task_attention.as_ref() {
            request.private_task_input = task.private_task_input.clone();
            request = request
                .with_session_command_fingerprint(task.command_fingerprint())
                .with_session_command_identity();
            if !request.is_task_child_command() {
                return Err(RunError::bad_request("Task child identity is incomplete"));
            }
            request
                .placement
                .required_capabilities
                .insert(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY.to_owned());
        }
        let admission = self
            .coordinated_child_admission(
                session_id.as_str(),
                &thread_id,
                intent,
                max_unarchived_threads,
                task_attention.as_ref().zip(task_origin.as_ref()),
                &root_agent_id,
            )
            .await?;
        let store = self.owner()?.clone();

        store
            .enqueue_session_child(request, admission)
            .await
            .map_err(dispatch_admission_error)?;
        finish_coordinated_child_admission(self, session_id.as_str(), thread_id, intent).await
    }

    /// Admit one terminal child report as immutable input of a deterministic
    /// coordinator Run. The source Thread commit owns the report and the target
    /// Thread commit owns its consumption; Outbox/Inbox is intentionally absent.
    pub async fn continue_session_agent_report(
        &self,
        command: SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        if command.source_thread_id.0 == command.session_id
            || command.session_activity_epoch == 0
            || command.message.role != Role::User
            || !command
                .message
                .id
                .is_agent_thread_report_of(&command.source_run_id)
        {
            return Err(RunError::bad_request(
                "coordinated Agent report has inconsistent provenance",
            ));
        }
        let store = self.owner()?.clone();

        // This is Coordinator admission, not a physical execution attempt.
        // A completed child may have left its Environment with a remote Worker.
        let snapshot = self
            .lowering
            .continuation_snapshot(command.session_id.as_str())
            .await?;
        let run_id = command.run_id();
        let activation = RunActivation::new(
            run_id,
            ThreadId(command.session_id.to_string()),
            snapshot,
            vec![command.message],
        )
        .with_model_ref_override(
            self.lowering
                .model_ref_override(command.session_id.as_str())?,
        );
        // The report command itself is durable Session provenance. Do not rely
        // on a process-local prepared installed Session to rediscover self-affinity after a
        // cold restart; the settlement observer must always route this root
        // continuation back to the same Session application.
        let request = self
            .lowering
            .prepare(activation)
            .fresh()?
            .for_session(ThreadId(command.session_id.to_string()))
            .with_session_activity_epoch(command.session_activity_epoch);
        store
            .enqueue(request)
            .await
            .map_err(dispatch_admission_error)?;
        self.publication.hint_local_pool().await;
        Ok(())
    }

    /// Authenticate one coordination command against the source Thread's exact
    /// durable tool operation. Worker claims constrain execution ownership; the
    /// committed `ActiveToolBatch` additionally constrains call and payload.
    pub async fn validate_session_agent_message_source(
        &self,
        command: &SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageCommand, RunError> {
        let snapshot = self
            .reads
            .session_thread_run_recovery_snapshot(
                command.session_id.as_str(),
                &command.source_thread_id,
                &command.source_run_id,
            )
            .await?;
        agent_message_source::validate(&snapshot, command)
    }

    async fn coordinated_thread_dispatches(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<Vec<awaken_dispatch_contract::DispatchSummary>, RunError> {
        let parent = ThreadId(session_id.to_string());
        Ok(self
            .owner()?
            .list_dispatches()
            .await
            .map_err(|error| RunError::internal(error.to_string()))?
            .into_iter()
            .filter(|dispatch| {
                dispatch.thread_id == *thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent)
            })
            .collect())
    }

    async fn cancel_and_await_coordinated_thread_quiescence(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        self.interrupt_session_thread(session_id, thread_id).await?;
        if let Some(wake) = self.wake.as_ref() {
            // This is only a latency hint. The durable cancellation intent and
            // dispatch row remain the recovery authority if the hint is lost.
            awaken_dispatch_execution::publish_dispatch_hint(
                wake.as_ref(),
                &CancellationToken::new(),
            )
            .await;
        }

        const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
        tokio::time::timeout(SETTLE_TIMEOUT, async {
            loop {
                let active = self
                    .coordinated_thread_dispatches(session_id, thread_id)
                    .await?
                    .into_iter()
                    .any(|dispatch| dispatch.state.remains_unsettled());
                if !active {
                    return Ok(());
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| {
            RunError::unavailable_classified(
                "coordination_settle_timeout",
                "Agent Thread interruption did not settle before archive timed out",
            )
        })?
    }

    /// Commit the one Thread-scoped archive fact through the parent Session's
    /// physical commit boundary. A deterministic maintenance Run supplies the
    /// existing ThreadCommit envelope without emitting a lifecycle transition;
    /// the disposition cell remains the sole archive truth and makes retries
    /// idempotent after an ambiguous response.
    pub async fn archive_session_thread(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        if thread_id.0 == session_id {
            return Err(RunError::bad_request(
                "the primary Thread is archived through its Session",
            ));
        }
        let already_archived = self
            .reads
            .session_thread_disposition(session_id, thread_id)
            .await?
            == awaken_agent_contract::ThreadDisposition::Archived;

        let child_rows = self
            .coordinated_thread_dispatches(session_id, thread_id)
            .await?;
        if !already_archived
            && child_rows.iter().any(|dispatch| {
                matches!(
                    dispatch.state,
                    awaken_dispatch_contract::DispatchState::Reserved
                        | awaken_dispatch_contract::DispatchState::ReservationLeased
                        | awaken_dispatch_contract::DispatchState::Pending
                        | awaken_dispatch_contract::DispatchState::Leased
                )
            })
        {
            return Err(RunError::bad_request(
                "only an idle Agent Thread may be archived",
            ));
        }
        if already_archived
            || child_rows
                .iter()
                .any(|dispatch| dispatch.state == awaken_dispatch_contract::DispatchState::Awaiting)
        {
            // The protocol projects an Awaiting tool boundary as idle for archive admission. Use
            // the ordinary claim-fenced interruption path to close its pending
            // calls; never manufacture terminal state in the protocol adapter.
            self.cancel_and_await_coordinated_thread_quiescence(session_id, thread_id)
                .await?;
            if already_archived {
                return Ok(());
            }
        }

        let commit = self.reads.commit_for_read(session_id).await?;
        if commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(RunError::internal)?
            .is_some_and(|run| run.state == RunState::Running)
        {
            return Err(RunError::bad_request(
                "only an idle Agent Thread may be archived",
            ));
        }
        let archive_run = RunId(format!(
            "thread-archive-{}",
            awaken_agent_contract::stable_fingerprint(&(
                "thread-archive-v1",
                session_id,
                thread_id.0.as_str(),
            ))
        ));
        let archive = awaken_agent_contract::ThreadCommit::assemble(
            thread_id.clone(),
            awaken_agent_contract::thread::commit::RunDisposition::ended(
                archive_run,
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            ),
            false,
            Vec::new(),
            vec![awaken_agent_contract::archive_thread_command()],
            Vec::new(),
        );
        use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
        match commit.commit(archive).await {
            Ok(_) => {
                // Catch an admission that passed its pre-check immediately
                // before the disposition commit. Its own post-check also
                // cancels, while this side makes archive retry independently
                // recoverable after either caller crashes.
                self.cancel_and_await_coordinated_thread_quiescence(session_id, thread_id)
                    .await
            }
            Err(error) => {
                // Concurrent/retried archive may have committed after our read.
                // Re-read the one durable cell before surfacing the commit race.
                if self
                    .reads
                    .session_thread_disposition(session_id, thread_id)
                    .await?
                    == awaken_agent_contract::ThreadDisposition::Archived
                {
                    self.cancel_and_await_coordinated_thread_quiescence(session_id, thread_id)
                        .await
                } else {
                    Err(RunError::internal(format!(
                        "commit Session Thread archive: {error}"
                    )))
                }
            }
        }
    }
}

impl CoordinatedThreads {
    /// Persist interruption of every executable dispatch for one logical child.
    /// A row whose Run already committed `Ended` is the worker's current
    /// before-settle boundary, not executable work; skipping it lets a failed
    /// boundary cancel raced later continuations without fencing its own claim.
    /// Awaiting native children are resolved by the DispatchWorker's specialized
    /// claim-fenced interruption command; running/remote children retain the
    /// canonical cancellation path.
    pub async fn interrupt_session_thread(
        &self,
        session_id: &str,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        if child_thread_id.0 == session_id {
            return Err(RunError::bad_request(
                "a coordinated child Thread must differ from its parent Session",
            ));
        }
        let commit = self.reads.commit_for_read(session_id).await?;
        let child_runs = self
            .executable_session_dispatch_runs(session_id, child_thread_id, &commit)
            .await?;
        for run_id in child_runs {
            self.durable_run_control()?
                .persist_recoverable_cancellation(
                    &run_id,
                    awaken_dispatch_execution::durable_control::RunResidence::Elsewhere,
                )
                .await?;
        }
        Ok(())
    }

    /// Select interruptible work only from the one Session-affined dispatch
    /// authority. Retry-exhausted work remains interruptible because it still
    /// owns an accepted Session Event; the control path requeues that same row
    /// only to let the canonical cancellation claim settle it. Primary
    /// interruption requires one result; child interruption deliberately
    /// consumes every result so a raced continuation is fenced too.
    pub async fn executable_session_dispatch_runs(
        &self,
        session_id: &str,
        logical_thread_id: &ThreadId,
        commit: &crate::SessionCommit,
    ) -> Result<Vec<RunId>, RunError> {
        let parent = ThreadId(session_id.to_string());
        let rows = self
            .owner()?
            .list_dispatches()
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(rows
            .into_iter()
            .filter(|dispatch| {
                dispatch.thread_id == *logical_thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent)
                    && matches!(
                        dispatch.state,
                        awaken_dispatch_contract::DispatchState::Reserved
                            | awaken_dispatch_contract::DispatchState::ReservationLeased
                            | awaken_dispatch_contract::DispatchState::Pending
                            | awaken_dispatch_contract::DispatchState::Leased
                            | awaken_dispatch_contract::DispatchState::Awaiting
                            | awaken_dispatch_contract::DispatchState::DeadLetter
                    )
            })
            .filter(|dispatch| {
                !commit
                    .run_state(&dispatch.run_id)
                    .is_some_and(|state| state.is_terminal())
            })
            .map(|dispatch| dispatch.run_id)
            .collect())
    }

    /// Rebuild the sole durable resume target for a foreground root Run whose
    /// committed Awaiting ticket predates dispatch ownership. Budget and tool
    /// replies share this handoff; child Runs must already have a durable row.
    pub async fn enqueue_foreground_session_resume_dispatch(
        &self,
        session_id: &str,
        ticket: &ResumeTicket,
        session_activity_epoch: u64,
    ) -> Result<Arc<AnyCoordinatorDispatch>, RunError> {
        if ticket.thread_id.0 != session_id || session_activity_epoch == 0 {
            return Err(RunError::internal(
                "only an exact foreground Session root can enter durable resume",
            ));
        }
        let store = self.owner()?.clone();
        let snapshot = self.lowering.resume_snapshot(session_id).await?;
        if ticket.inherited_toolsets.is_none()
            && !snapshot
                .resolved_spec
                .plugin_config
                .agent
                .delegates
                .is_empty()
        {
            return Err(RunError::bad_request(
                "legacy Run ticket cannot resume a delegation-capable root without frozen tool sources",
            ));
        }
        let activation =
            crate::resume_activation(ThreadId(session_id.to_owned()), snapshot, ticket)
                .with_model_ref_override(self.lowering.model_ref_override(session_id)?);
        let request = self
            .lowering
            .prepare(activation)
            .resume()?
            .for_session(ThreadId(session_id.to_string()))
            .with_session_activity_epoch(session_activity_epoch);
        // One exact enqueue closes the foreground-to-durable crash window. A
        // concurrent exact retry is idempotent; a changed Run payload is rejected
        // by the existing dispatch identity fence before any resume is staged.
        store
            .enqueue(request)
            .await
            .map_err(|error| RunError::internal(error.to_string()))?;
        Ok(store)
    }
}

/// Fail closed before resuming: the asserted `tool_use_id` must name the Run's
/// pending tool and its binding must match the inbound resume kind.
pub fn check_pending(
    ticket: &ResumeTicket,
    tool_use_id: &str,
    want_client: bool,
) -> Result<(), RunError> {
    let pending = Pending::from_resume_ticket(ticket)
        .ok_or_else(|| RunError::internal("awaiting run has no pending tool"))?;
    if pending.tool_use_id != tool_use_id {
        return Err(RunError::bad_request(format!(
            "tool_use_id {tool_use_id:?} does not match the pending tool"
        )));
    }
    if pending.client_executed != want_client {
        let (got, expected) = if want_client {
            ("built-in", "a confirmation")
        } else {
            ("client-executed", "a client tool result")
        };
        return Err(RunError::bad_request(format!(
            "pending tool is {got}; answer it with {expected}"
        )));
    }
    Ok(())
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for CoordinatedThreads {
    async fn source_dispatch_observation(
        &self,
        session: &awaken_session_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        let Ok(source) = self.owner() else {
            return Default::default();
        };
        awaken_dispatch_execution::read_source_dispatch_observation(source.as_ref(), session).await
    }

    async fn execution_recovery_observation(
        &self,
        session: &awaken_session_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionExecutionRecoveryObservation {
        let Ok(source) = self.owner() else {
            return awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryUnavailable,
            };
        };
        awaken_dispatch_execution::read_execution_recovery(
            source.as_ref(),
            self.reads.as_ref(),
            session,
        )
        .await
    }

    async fn task_callback_source(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &ThreadId,
        root_agent_id: &str,
    ) -> Result<
        Option<(
            String,
            awaken_agent_contract::agent::delegation::DelegationOrigin,
        )>,
        RunError,
    > {
        self.reads
            .task_callback_source(session_id, thread_id, root_agent_id)
            .await
    }

    async fn task_origin_execution(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &ThreadId,
        origin_run_id: &awaken_agent_contract::RunId,
    ) -> Result<
        Box<(
            awaken_runtime_contract::RunActivation,
            Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
        )>,
        RunError,
    > {
        crate::session_run_reservation::task_origin_execution(
            self.owner()?.as_ref(),
            session_id,
            thread_id,
            origin_run_id,
        )
        .await
    }

    async fn session_run_command_receipt(
        &self,
        command: &awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<Option<awaken_session_execution_contract::SessionRunReservation>, RunError> {
        use awaken_dispatch_contract::{
            SessionRunAdmissionObservation, SessionRunAdmissionReads as _,
        };
        let source = self
            .owner()
            .map_err(|error| RunError::unavailable(error.message))?;
        let observed = source
            .observe_session_run_admission(&command.run_id)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let fingerprint = command.command_fingerprint();
        let thread = command
            .private_task_input
            .as_ref()
            .and_then(|input| input.tenure())
            .map_or(command.session_id.as_str(), |tenure| tenure.thread.as_str());
        match observed {
            SessionRunAdmissionObservation::Missing => Ok(None),
            SessionRunAdmissionObservation::Live(request) => {
                let Some(stored) = request
                    .session_command_fingerprint
                    .as_ref()
                    .filter(|fp| fp.is_current())
                else {
                    return Err(RunError::unavailable(
                        "original Task command identity is unavailable",
                    ));
                };
                if stored != &fingerprint
                    || request.thread_id().0 != thread
                    || request.session_thread_id().0 != command.session_id.as_str()
                {
                    return Err(RunError::conflict(
                        "Task attention Run id was reused with different immutable input",
                    ));
                }
                if let Some(epoch) = request.session_activity_epoch.filter(|epoch| *epoch > 0) {
                    Ok(Some(awaken_session_execution_contract::SessionRunReservation::AlreadyActivated { session_activity_epoch: epoch }))
                } else {
                    Err(RunError::unavailable(
                        "exact Task attention is reserved but not activated",
                    ))
                }
            }
            SessionRunAdmissionObservation::Completed { completion, .. } => {
                if completion.request_fingerprint.is_none()
                    || completion.thread_id.is_none()
                    || completion.session_thread_id.is_none()
                {
                    return Err(RunError::unavailable(
                        "original Task completion identity is unavailable",
                    ));
                }
                if completion
                    .thread_id
                    .as_ref()
                    .is_some_and(|id| id.0 == thread)
                    && completion
                        .session_thread_id
                        .as_ref()
                        .is_some_and(|id| id.0 == command.session_id.as_str())
                    && completion.request_fingerprint.as_deref() == Some(fingerprint.as_str())
                {
                    Ok(Some(
                        awaken_session_execution_contract::SessionRunReservation::Completed,
                    ))
                } else {
                    Err(RunError::conflict(
                        "Task attention completion belongs to different immutable input",
                    ))
                }
            }
        }
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError>
    {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        if command.activity_epoch() == Some(0)
            || matches!(command, awaken_session_execution_contract::CoordinatedAdmissionRef::Report(_, baseline)
                if baseline.agent_revision.is_none_or(|revision| revision == 0))
        {
            return Err(RunError::unavailable(
                "original coordination activity epoch is invalid",
            ));
        }
        let run = command.run_id();
        let source = self
            .owner()
            .map_err(|error| RunError::unavailable(error.message))?;
        let reads: &dyn awaken_dispatch_contract::SessionRunAdmissionReads = source.as_ref();
        let observed = reads
            .observe_session_run_admission(&run)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        if let SessionRunAdmissionObservation::Completed { completion, .. } = &observed {
            let session = command.session_id();
            let thread = command.thread_id();
            if completion.run_id == run
                && completion.thread_id.as_ref() == Some(&thread)
                && completion
                    .session_thread_id
                    .as_ref()
                    .is_some_and(|id| id.0 == session.as_str())
                && let Some(archive) = self
                    .reads
                    .archived_run_activation(session.as_str(), &thread, &run)
                    .await?
                && archive.full_dispatch
                && command.activity_epoch().is_some()
                && archive.activity_epoch == command.activity_epoch()
                && completion.request_fingerprint.as_deref()
                    == Some(archive.admission_fingerprint.as_str())
                && awaken_dispatch_execution::coordinated_activation_matches(
                    &command,
                    &archive.activation,
                )
            {
                return Ok(Some(
                    awaken_session_execution_contract::SessionAgentMessageReceipt {
                        thread_id: thread,
                    },
                ));
            }
        }
        match observed {
            SessionRunAdmissionObservation::Missing => Ok(None),
            SessionRunAdmissionObservation::Live(request)
                if request.identity_scope
                    == awaken_dispatch_contract::DispatchIdentityScope::FullDispatch
                    && request.session_command_fingerprint.is_none()
                    && request.session_thread_id.as_ref()
                        == Some(&awaken_agent_contract::agent::thread::Id(
                            command.session_id().to_string(),
                        ))
                    && command.activity_epoch().is_some_and(|epoch| epoch != 0)
                    && request.session_activity_epoch == command.activity_epoch()
                    && awaken_dispatch_execution::coordinated_activation_matches(
                        &command,
                        &request.activation,
                    ) =>
            {
                Ok(Some(
                    awaken_session_execution_contract::SessionAgentMessageReceipt {
                        thread_id: command.thread_id(),
                    },
                ))
            }
            _ => Err(RunError::unavailable(
                "original coordinated admission evidence is incomplete or mismatched",
            )),
        }
    }

    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        let commit = self.reads.commit_for_read(session_id.as_str()).await?;
        commit
            .authoritative_run(run_id)
            .await
            .map(|record| record.map(|record| record.state))
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.reads.delegated_runs(thread).await
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.reads
            .coordinated_threads(session_id.as_str(), observed_root)
            .await
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, RunError> {
        CoordinatedThreads::session_thread_tool_reply_fence(self, command).await
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageCommand, RunError> {
        CoordinatedThreads::validate_session_agent_message_source(self, command).await
    }
}
