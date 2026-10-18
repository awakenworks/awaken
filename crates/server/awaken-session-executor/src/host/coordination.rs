// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Host reply and coordinated Run admission validate the committed
// @formal reason: Thread ticket, Session activity, and exact dispatch before delivery.
//! Durable admission and derived reads for Managed Agent coordination.
//!
//! A child remains an ordinary `RunDispatch` whose logical Thread is stored in
//! the parent Session's commit partition. Relationships are reconstructed from
//! the parent's committed `send_message` tool pairs; this module owns no
//! registry, receipt map, mailbox, or background executor.

use super::*;

mod agent_message_source;
mod session_reply_receipt;

pub(super) use session_reply_receipt::{SessionReplyReceipt, validate_awaiting_tool_authority};

use awaken_dispatch_execution::coordinated_run_activation;
use awaken_dispatch_contract::Outbox as _;
use awaken_session_execution_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind, CoordinatedThreadTarget,
    SessionAgentMessageCommand, SessionAgentMessageReceipt, SessionAgentReportContinuation,
    coordinated_thread_closed,
};

fn reply_dispatch_activity(
    dispatch: Option<&awaken_dispatch_contract::DispatchSummary>,
    thread: &ThreadId,
    parent: &ThreadId,
) -> Result<awaken_session_execution_contract::SessionThreadToolReplyActivity, HostError> {
    use awaken_session_execution_contract::SessionThreadToolReplyActivity;
    match dispatch {
        Some(row) if row.thread_id == *thread => SessionThreadToolReplyActivity::from_dispatch(
            thread,
            row.session_thread_id.as_ref(),
            row.session_activity_epoch,
            parent,
        )
        .ok_or_else(|| {
            HostError::bad_request("Session Thread reply dispatch affinity is inconsistent")
        }),
        Some(_) => Err(HostError::bad_request(
            "Session Thread reply dispatch affinity is inconsistent",
        )),
        None if thread == parent => {
            Ok(SessionThreadToolReplyActivity::Session { prior_epoch: None })
        }
        None => Err(HostError::internal(
            "Session child Thread reply has no exact durable dispatch row",
        )),
    }
}

fn dispatch_admission_error(error: awaken_dispatch_contract::DispatchError) -> HostError {
    match error {
        awaken_dispatch_contract::DispatchError::Rejected(message) => {
            HostError::bad_request(message)
        }
        awaken_dispatch_contract::DispatchError::Conflict(message) => {
            HostError::unavailable(message)
        }
        awaken_dispatch_contract::DispatchError::Unavailable(message) => {
            HostError::unavailable(message)
        }
    }
}

async fn finish_coordinated_child_admission(
    host: &crate::ChildRunCoordination,
    session_id: &str,
    thread_id: ThreadId,
    intent: CoordinatedRunIntent,
) -> Result<SessionAgentMessageReceipt, HostError> {
    // Thread disposition, Run commits, and dispatch admission may use
    // different durable adapters. Re-read both existing admission fences
    // after enqueue: whichever side won records cancellation on this exact
    // dispatch and waits for settlement. No relation flag, terminal cache,
    // or process-local lock is introduced.
    let archived = host
        .thread_reads
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
        return Err(HostError::bad_request(if archived {
            "Agent Thread is archived"
        } else {
            "Agent Thread terminated after a failed Run"
        }));
    }
    host.session_runs.notify_local_pool().await;
    Ok(SessionAgentMessageReceipt { thread_id })
}

impl crate::ChildRunCoordination {
    /// Register before publishing one Session-owned resume, then project only
    /// the next committed Step. The durable reply ingress and Worker execution
    /// are shared with background Managed Event reconciliation.
    pub(crate) async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<CommittedStepReceipt, HostError> {
        let session_id = delivery.command.session_id.clone();
        let run_id = delivery.command.expected_run_id.clone();
        let correlation_id = delivery.command.expected_correlation_id.clone();
        self.session_runs
            .observe_published_resume(
                session_id.as_str(),
                run_id,
                correlation_id,
                stream_sink,
                self.reply_session_thread_tool(delivery),
            )
            .await
    }

    /// Resolve a public reply to the one currently committed Awaiting ticket.
    /// Thread recovery remains the authority; the Session event identity and
    /// version are only admission fences against delayed or reused call ids.
    pub(super) async fn validated_session_thread_tool_reply(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(ThreadId, ResumeTicket), HostError> {
        if !command.has_complete_coordinates() {
            return Err(HostError::bad_request(
                "Session Thread tool reply is incomplete",
            ));
        }
        let thread_id = command.target.thread_id(&command.session_id);
        if command
            .target
            .child_thread_id()
            .is_some_and(|child| child.0 == command.session_id)
        {
            return Err(HostError::bad_request(
                "a coordinated child Thread must differ from its parent Session",
            ));
        }
        let commit = self
            .thread_reads
            .commit_for_read(command.session_id.as_str())
            .await?;
        let snapshot = commit
            .recovery_snapshot(&thread_id, &command.expected_run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if command
            .expected_thread_version
            .is_some_and(|expected| expected != snapshot.thread_version)
        {
            return Err(HostError::bad_request(
                "Session Thread awaiting ticket changed after Event admission",
            ));
        }
        let current = snapshot
            .current_run()
            .map_err(|error| HostError::internal(error.to_string()))?;
        if !current.is_some_and(|run| {
            run.id == command.expected_run_id && matches!(run.state, RunState::Awaiting)
        }) {
            return Err(HostError::bad_request("Session Thread has no awaiting Run"));
        }
        let ticket = snapshot
            .answerable_ticket(&command.expected_run_id)
            .map_err(|error| HostError::internal(error.to_string()))?
            .filter(|ticket| ticket.correlation_id == command.expected_correlation_id)
            .ok_or_else(|| {
                HostError::bad_request(
                    "Session Thread awaiting ticket changed after Event admission",
                )
            })?
            .clone();
        if matches!(
            command.reply,
            awaken_session_execution_contract::SessionThreadToolReply::Input(_)
        ) {
            if ticket.call_id().unwrap_or_default() != command.tool_use_id {
                return Err(HostError::bad_request(
                    "Session input call identity differs from its ticket",
                ));
            }
            if matches!(ticket.target(), AwaitTarget::ToolCall { .. }) {
                validate_awaiting_tool_authority(&thread_id, &ticket, &snapshot.state)?;
            }
            let result = super::run::session_thread_reply_result(
                &ticket,
                &command.tool_use_id,
                &command.reply,
            );
            awaken_runtime_contract::resume::validate_resume(
                &ticket,
                &awaken_runtime_contract::resume::ResumeCommand::from_ticket(
                    &ticket,
                    result,
                    awaken_time::WallClockExt::now_ms(self.clock.as_ref()),
                ),
            )
            .map_err(|error| HostError::bad_request(error.to_string()))?;
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
                return Err(HostError::bad_request(
                    "Session Thread awaiting ticket is not externally answerable",
                ));
            }
        }
        self.session_runs.check_pending(
            &ticket,
            &command.tool_use_id,
            command.reply.client_executed(),
        )?;
        Ok((thread_id, ticket))
    }

    /// Derive the absorbing follow-up fence from the latest committed Run in the
    /// parent Session partition through the contract close rule for the Thread's
    /// kind. No Thread status or dispatch flag shadows that lifecycle truth.
    pub(super) async fn coordinated_thread_is_closed(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        kind: CoordinatedThreadKind,
    ) -> Result<bool, HostError> {
        let commit = self.thread_reads.commit_for_read(session_id).await?;
        Ok(commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(HostError::internal)?
            .is_some_and(|run| coordinated_thread_closed(kind, &run.state)))
    }

    /// Read the exact parent-partition Awaiting coordinate before the Session
    /// aggregate opens a reply activity. Delivery revalidates this fence, which
    /// closes call-id reuse and concurrent-resume races without another store.
    pub(crate) async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, HostError> {
        // Resolve topology before SessionApplication opens a reply activity.
        self.session_runs.coordinator_dispatch()?;
        let (thread_id, _ticket) = match self.validated_session_thread_tool_reply(command).await {
            Ok(active) => active,
            Err(active_error) => match self.session_thread_tool_reply_receipt(command).await? {
                SessionReplyReceipt::Exact => {
                    return Ok(awaken_session_execution_contract::SessionThreadToolReplyFence {
                        activity:
                            awaken_session_execution_contract::SessionThreadToolReplyActivity::Session {
                                prior_epoch: None,
                            },
                        already_applied: true,
                    });
                }
                SessionReplyReceipt::Conflict => {
                    return Err(HostError::bad_request(
                        "Session Thread awaiting correlation was answered by another reply operation",
                    ));
                }
                SessionReplyReceipt::Absent => return Err(active_error),
            },
        };
        let parent = ThreadId(command.session_id.to_string());
        let dispatch = self
            .session_runs
            .coordinator_dispatch()?
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let activity = reply_dispatch_activity(dispatch.as_ref(), &thread_id, &parent)?;
        Ok(
            awaken_session_execution_contract::SessionThreadToolReplyFence {
                activity,
                already_applied: false,
            },
        )
    }

    /// Deliver a typed reply to one logical Session Thread while retaining the
    /// Session as the physical commit/history partition. The dispatch row and
    /// committed ticket are the only routing authorities; no child-named
    /// Session context or store is opened.
    pub(crate) async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), HostError> {
        let awaken_session_execution_contract::SessionThreadToolReplyDelivery {
            command,
            fence,
            session_activity_epoch,
        } = delivery;
        let (thread_id, ticket) = match self.validated_session_thread_tool_reply(&command).await {
            Ok(_) if fence.already_applied => {
                return Err(HostError::internal(
                    "Session Thread reply became active after an applied-receipt fence",
                ));
            }
            Ok(active) => active,
            Err(active_error) => match self.session_thread_tool_reply_receipt(&command).await? {
                SessionReplyReceipt::Exact => return Ok(()),
                SessionReplyReceipt::Conflict => {
                    return Err(HostError::bad_request(
                        "Session Thread awaiting correlation was answered by another reply operation",
                    ));
                }
                SessionReplyReceipt::Absent if fence.already_applied => {
                    return Err(HostError::internal(
                        "Session Thread reply receipt disappeared after fencing",
                    ));
                }
                SessionReplyReceipt::Absent => return Err(active_error),
            },
        };
        let result =
            super::run::session_thread_reply_result(&ticket, &command.tool_use_id, &command.reply);

        let store = self.session_runs.coordinator_dispatch()?;
        let parent = ThreadId(command.session_id.to_string());
        let dispatch = store
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let activity = reply_dispatch_activity(dispatch.as_ref(), &thread_id, &parent)?;
        if std::mem::discriminant(&activity) != std::mem::discriminant(&fence.activity) {
            return Err(HostError::bad_request(
                "Session Thread reply ownership changed after admission",
            ));
        }
        let store = if dispatch.is_some() {
            store
        } else {
            let epoch = session_activity_epoch
                .ok_or_else(|| HostError::internal("foreground Session reply requires activity"))?;
            self.session_runs
                .enqueue_foreground_session_resume_dispatch(
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
            .map_err(|error| HostError::bad_request(error.message))?
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
            .map_err(|error| HostError::internal(error.to_string()))?;
        if self.session_runs.local_pool.get().is_some() {
            self.session_runs.dispatch_hint.publish().await;
        } else {
            store
                .relay()
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
        }
        Ok(())
    }

    async fn coordinated_child_admission(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        intent: CoordinatedRunIntent,
        max_unarchived_threads: usize,
    ) -> Result<awaken_dispatch_contract::SessionChildAdmission, HostError> {
        if max_unarchived_threads == 0 {
            return Err(HostError::bad_request(
                "Session child capacity must be greater than zero",
            ));
        }
        let links = self
            .thread_reads
            .coordinated_threads(session_id, None)
            .await?;
        if intent == CoordinatedRunIntent::FollowUp {
            let Some(link) = links.iter().find(|link| {
                link.thread_id == *thread_id
                    && matches!(link.target, CoordinatedThreadTarget::Agent { .. })
            }) else {
                return Err(HostError::bad_request(
                    "follow-up target is not an ordinary coordinated Agent Thread",
                ));
            };
            if self
                .coordinated_thread_is_closed(session_id, thread_id, link.target.kind())
                .await?
            {
                return Err(HostError::bad_request(
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
                        .thread_reads
                        .session_thread_disposition(session_id, &link.thread_id)
                        .await?
                        == awaken_agent_contract::ThreadDisposition::Archived =>
                {
                    archived.push(link.thread_id);
                }
                CoordinatedThreadTarget::Agent { .. } => {}
            }
        }
        Ok(awaken_dispatch_contract::SessionChildAdmission::new(
            max_unarchived_threads,
            archived,
        )
        .with_capacity_exempt_threads(capacity_exempt))
    }

    pub(crate) async fn admit_coordinated_run(
        &self,
        command: CoordinatedRunCommand,
    ) -> Result<SessionAgentMessageReceipt, HostError> {
        let mut command = command;
        let intent = command.intent;
        let max_unarchived_threads = command.max_unarchived_threads;
        let session_id = command.session_id.clone();
        let thread_id = command.thread_id.clone();
        let session_activity_epoch = command
            .session_activity_epoch
            .filter(|epoch| *epoch != 0)
            .ok_or_else(|| {
                HostError::unavailable("fresh coordination requires an original nonzero activity")
            })?;
        let (baseline, tools, resources, coordinator_publication) = self
            .installed_sessions
            .read(session_id.as_str(), |slot| {
                (
                    slot.inputs.baseline.clone(),
                    slot.inputs.tools.clone(),
                    // Coordinated children share the exact generation selected
                    // for root and extension dispatch. Selection stays inside
                    // this one slot snapshot; child admission neither publishes
                    // nor reconstructs physical active truth.
                    slot.realization
                        .staging()
                        .run_manifest(slot.run.session_dispatch),
                    slot.inputs.published_snapshot.clone(),
                )
            })
            .ok_or_else(|| HostError::bad_request("Session runtime projection is unavailable"))?;
        let baseline = baseline.ok_or_else(|| {
            HostError::bad_request("Session frozen baseline is unavailable for coordination")
        })?;
        let resources = resources.ok_or_else(|| {
            HostError::bad_request("Session resource manifest is unavailable for coordination")
        })?;
        if command.snapshot.metadata.source.revision == 0
            || command.snapshot.metadata.source.agent_id != command.snapshot.root_agent_id
        {
            return Err(HostError::bad_request(
                "coordinated Run does not carry an exact Agent publication",
            ));
        }

        let mut snapshot = command.snapshot;
        let inherit_session_overrides = snapshot.root_agent_id.0 == baseline.agent_id;
        if inherit_session_overrides {
            snapshot = awaken_session_realization_contract::project_effective_agent_publication(
                baseline.model_override.as_ref(),
                &baseline.system_prompt,
                Some(resources.resources.skills()),
                &resources.workspace_id,
                snapshot,
            )
            .map_err(|error| HostError::internal(error.to_string()))?;
            if let Some(tools) = tools.as_ref() {
                super::session::project_session_tool_override(
                    &mut snapshot,
                    tools,
                    super::session::SessionToolsetProjection::ProjectIntoSnapshot,
                );
            }
        }
        super::session::project_managed_coordination_surface(
            &mut snapshot,
            super::session::ManagedCoordinationRole::Child,
        );
        snapshot.recompute_fingerprint().map_err(|error| {
            HostError::internal(format!("fingerprint coordinated child snapshot: {error}"))
        })?;

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
                HostError::bad_request(
                    "coordinated Run has no exact coordinator tool configuration",
                )
            })?;
        command.snapshot = snapshot;
        let activation =
            coordinated_run_activation(command, baseline.agent_id, coordinator_toolsets);
        let request = crate::agent_runner::child_dispatch_request(
            activation,
            ThreadId(session_id.to_string()),
            Some(resources),
            self.capabilities.agent.agent_publications.as_deref(),
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?
        .with_session_activity_epoch(session_activity_epoch);
        let admission = self
            .coordinated_child_admission(
                session_id.as_str(),
                &thread_id,
                intent,
                max_unarchived_threads,
            )
            .await?;
        let store = self.session_runs.coordinator_dispatch()?;

        store
            .enqueue_session_child(request, admission)
            .await
            .map_err(dispatch_admission_error)?;
        finish_coordinated_child_admission(self, session_id.as_str(), thread_id, intent).await
    }

    /// Admit one terminal child report as immutable input of a deterministic
    /// coordinator Run. The source Thread commit owns the report and the target
    /// Thread commit owns its consumption; Outbox/Inbox is intentionally absent.
    pub(crate) async fn continue_session_agent_report(
        &self,
        command: SessionAgentReportContinuation,
    ) -> Result<(), HostError> {
        if command.source_thread_id.0 == command.session_id
            || command.session_activity_epoch == 0
            || command.message.role != Role::User
            || !command
                .message
                .id
                .is_agent_thread_report_of(&command.source_run_id)
        {
            return Err(HostError::bad_request(
                "coordinated Agent report has inconsistent provenance",
            ));
        }
        let store = self.session_runs.coordinator_dispatch()?;

        // This is Coordinator admission, not a physical execution attempt.
        // A completed child may have left its Environment with a remote Worker.
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(command.session_id.as_str(), None)
            .await?;
        let run_id = command.run_id();
        let activation = RunActivation::new(
            run_id,
            ThreadId(command.session_id.to_string()),
            ctx.config.clone(),
            vec![command.message],
        )
        .with_model_ref_override(
            self.capabilities
                .agent
                .inference_routing
                .override_for(command.session_id.as_str()),
        );
        // The report command itself is durable Session provenance. Do not rely
        // on a process-local prepared slot to rediscover self-affinity after a
        // cold restart; the settlement observer must always route this root
        // continuation back to the same Session application.
        let request = self
            .run_assembly
            .prepare_dispatch(activation)
            .fresh()?
            .for_session(ThreadId(command.session_id.to_string()))
            .with_session_activity_epoch(command.session_activity_epoch);
        store
            .enqueue(request)
            .await
            .map_err(dispatch_admission_error)?;
        self.session_runs.notify_local_pool().await;
        Ok(())
    }

    /// Authenticate one coordination command against the source Thread's exact
    /// durable tool operation. Worker claims constrain execution ownership; the
    /// committed `ActiveToolBatch` additionally constrains call and payload.
    pub(crate) async fn validate_session_agent_message_source(
        &self,
        command: &SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageCommand, HostError> {
        let snapshot = self
            .thread_reads
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
    ) -> Result<Vec<awaken_dispatch_contract::DispatchSummary>, HostError> {
        let parent = ThreadId(session_id.to_string());
        Ok(self
            .session_runs
            .coordinator_dispatch()?
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
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
    ) -> Result<(), HostError> {
        self.session_runs
            .interrupt_session_thread(session_id, thread_id)
            .await?;
        if let Some(wake) = self.authority.dispatch_wake() {
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
            HostError::unavailable_classified(
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
    pub(crate) async fn archive_session_thread(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<(), HostError> {
        if thread_id.0 == session_id {
            return Err(HostError::bad_request(
                "the primary Thread is archived through its Session",
            ));
        }
        let already_archived = self
            .thread_reads
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
            return Err(HostError::bad_request(
                "only an idle Agent Thread may be archived",
            ));
        }
        if already_archived
            || child_rows.iter().any(|dispatch| {
                dispatch.state == awaken_dispatch_contract::DispatchState::Awaiting
            })
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

        let commit = self.thread_reads.commit_for_read(session_id).await?;
        if commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(HostError::internal)?
            .is_some_and(|run| run.state == RunState::Running)
        {
            return Err(HostError::bad_request(
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
                    .thread_reads
                    .session_thread_disposition(session_id, thread_id)
                    .await?
                    == awaken_agent_contract::ThreadDisposition::Archived
                {
                    self.cancel_and_await_coordinated_thread_quiescence(session_id, thread_id)
                        .await
                } else {
                    Err(HostError::internal(format!(
                        "commit Session Thread archive: {error}"
                    )))
                }
            }
        }
    }
}

use awaken_dispatch_contract::{
    DispatchEnqueue as _, DispatchReads as _, SessionContinuationOutbox as _,
    SessionRunReservations as _,
};

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_dispatch_contract::DispatchClaims;
    use awaken_session_execution_contract::coordinated_activation_input;

    #[tokio::test]
    async fn dispatch_admission_errors_preserve_commit_certainty() {
        // Cause/effect graph: C1 validation proves no write; C2 an existing
        // identity has another payload; C3 storage or transport cannot prove
        // whether the write committed; C4 the original accepted request is
        // retried exactly. Effects: E1 only C1 may close a new activity; E2 C2/C3
        // remain Unavailable through Host -> Run so Session cannot mistake them
        // for proof of nonacceptance; E3 C4 reuses the original row without a
        // second queue write, claim or settlement. Decision table:
        // R1=C1 -> BadRequest/E1; R2=C2 -> Unavailable/E2;
        // R3=C3 -> Unavailable/E2; R4=C2+C4 -> success/E3.
        // R2 uses the real SQLite enqueue owner: A is committed but its first
        // acknowledgement is deliberately withheld from the retrying caller;
        // B changes only runtime input under the same Run identity. A later
        // conflict cannot erase that accepted A. Summary and operational cursor
        // stutter guard the original authority; no claim is used as a read.
        let rejected = dispatch_admission_error(
            awaken_dispatch_contract::DispatchError::Rejected("invalid shape".into()),
        );
        let unavailable = dispatch_admission_error(
            awaken_dispatch_contract::DispatchError::Unavailable("response lost".into()),
        );
        assert_eq!(rejected.kind, HostErrorKind::BadRequest, "R1/E1");
        assert_eq!(unavailable.kind, HostErrorKind::Unavailable, "R3/E2");
        let rejected = crate::host_error::to_run_error(rejected);
        let unavailable = crate::host_error::to_run_error(unavailable);
        assert_eq!(
            rejected.kind,
            awaken_session_types::RunErrorKind::BadRequest
        );
        assert_eq!(rejected.code, "invalid_request");
        assert_eq!(rejected.message, "invalid shape");
        assert_eq!(
            unavailable.kind,
            awaken_session_types::RunErrorKind::Unavailable
        );
        assert_eq!(unavailable.code, "unavailable");
        assert_eq!(unavailable.message, "response lost");

        use crate::host::worker_resolver::test_support::{eager_environment, test_activation};
        use awaken_dispatch_contract::{
            DispatchCursor, DispatchEnqueue as _, DispatchOperationalFeed as _, DispatchOutcome,
            DispatchSettlement as _, SessionChildAdmission, SessionRuntimeEnvelope,
        };
        use awaken_session_execution_contract::{
            CoordinatedAdmissionRef, SessionCoordinationReads as _,
        };

        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(0));
        let dispatch = Arc::new(
            awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock)
                .await
                .expect("R2 original dispatch owner"),
        );
        let managed =
            crate::host::worker_resolver::test_support::adoption_host_over(dispatch.clone()).await;
        let session = awaken_session_types::SessionId::from("certainty-session");
        let parent = RunId("certainty-parent".into());
        let thread = awaken_session_execution_contract::coordinated_thread_id(
            &session,
            &parent,
            "certainty-operation",
        );
        let run = awaken_session_execution_contract::coordinated_run_id(
            &session,
            &thread,
            &parent,
            "certainty-operation",
        );
        let mut snapshot = test_activation("certainty-child", "certainty-run").snapshot;
        snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
        snapshot.metadata.source.revision = 1;
        snapshot.recompute_fingerprint().unwrap();
        let command = CoordinatedRunCommand {
            intent: CoordinatedRunIntent::Spawn,
            session_id: session.clone(),
            thread_id: thread,
            run_id: run,
            parent_run_id: parent,
            parent_call_id: "certainty-call".into(),
            operation_id: "certainty-operation".into(),
            snapshot,
            message: "original message".into(),
            session_activity_epoch: Some(1),
            max_unarchived_threads: 24,
        };
        assert_eq!(
            managed
                .coordination
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
                .await
                .unwrap(),
            None,
            "R0 actual Missing"
        );
        let mut runtime = awaken_session_configuration_contract::SessionRuntimeProjection {
            environment: eager_environment(),
            tools: None,
            mcp_stages: Some(Vec::new()),
        };
        let (encoded, requirements) = runtime.dispatch().expect("R2 frozen runtime A");
        let mut accepted = awaken_dispatch_contract::RunDispatch::new(
            coordinated_run_activation(command.clone(), "coord-root".into(), Vec::new())
                .with_model_ref_override(Some("frozen-original-route".into())),
        )
        .for_session(ThreadId("certainty-session".into()))
        .with_session_activity_epoch(1)
        .with_session_runtime(SessionRuntimeEnvelope::new(encoded));
        accepted.placement.session_mcp_credentials = Some(requirements);
        let admission = SessionChildAdmission::new(24, Vec::new());
        dispatch
            .enqueue_session_child(accepted.clone(), admission.clone())
            .await
            .expect("R2 A commits; acknowledgement is not delivered to the caller");
        let before = dispatch.list_dispatches().await.expect("R2 original row");
        assert_eq!(before.len(), 1, "R2 A was durably accepted");
        let cursor = dispatch
            .events_after(DispatchCursor::default(), 16, None)
            .await
            .expect("R2 original operation cursor")
            .next_cursor;

        // R5 axes: actual Live A permits its original business ACK despite
        // mutable fresh-input drift. Original inherited ceiling, model override
        // and resource/runtime bytes remain stored A; this ACK grants no effects
        // and does not prove the separate full-input/P0 execution obligation.
        // Changed Session/Thread/Run/parent/call /
        // message/publication/epoch cannot be projected as that accepted A.
        let receipt = managed
            .coordination
            .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.thread_id, command.thread_id, "R5 Live A");
        for axis in 0..9 {
            let mut changed = command.clone();
            match axis {
                0 => changed.session_id = "foreign-session".into(),
                1 => changed.thread_id = ThreadId("foreign-thread".into()),
                2 => changed.parent_run_id = RunId("foreign-parent".into()),
                3 => changed.parent_call_id = "foreign-call".into(),
                4 => changed.message = "different input".into(),
                5 => changed.snapshot.metadata.source.revision += 1,
                6 => changed.session_activity_epoch = None,
                7 => changed.session_activity_epoch = Some(0),
                _ => changed.session_activity_epoch = Some(2),
            }
            let error = managed
                .coordination
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&changed))
                .await
                .expect_err("R5 mismatch is unknown");
            assert_eq!(
                error.kind,
                awaken_session_types::RunErrorKind::Unavailable,
                "R5 axis {axis}"
            );
        }
        assert_eq!(
            dispatch.list_dispatches().await.unwrap(),
            before,
            "R5 observer stutter"
        );

        runtime.tools = Some(Default::default());
        let (encoded, requirements) = runtime.dispatch().expect("R2 frozen runtime B");
        assert_ne!(
            accepted.session_runtime.as_ref().unwrap().projection_json,
            encoded,
            "R2 tools None inherits while Some(empty) explicitly clears"
        );
        let mut changed = accepted
            .clone()
            .with_session_runtime(SessionRuntimeEnvelope::new(encoded));
        changed.placement.session_mcp_credentials = Some(requirements);
        let conflict = dispatch
            .enqueue_session_child(changed, admission.clone())
            .await
            .expect_err("R2 same Run with different runtime input must conflict");
        let message = match &conflict {
            awaken_dispatch_contract::DispatchError::Conflict(message) => message.clone(),
            other => panic!("R2 expected actual identity conflict, got {other}"),
        };
        let conflict = dispatch_admission_error(conflict);
        assert_eq!(conflict.kind, HostErrorKind::Unavailable, "R2/E2");
        let conflict = crate::host_error::to_run_error(conflict);
        assert_eq!(
            conflict.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "R2/E2 the Session settler must retain the accepted activity"
        );
        assert_eq!(conflict.code, "unavailable");
        assert_eq!(
            conflict.message, message,
            "R2 original diagnostic preserved"
        );
        assert_eq!(
            dispatch.list_dispatches().await.unwrap(),
            before,
            "R2 stutter"
        );

        dispatch
            .enqueue_session_child(accepted.clone(), admission)
            .await
            .expect("R4 exact A still replays");
        assert_eq!(dispatch.list_dispatches().await.unwrap(), before, "R4/E3");
        let after = dispatch.events_after(cursor, 16, None).await.unwrap();
        assert!(after.events.is_empty(), "R2/R4 no claim or settlement");
        assert_eq!(after.next_cursor, cursor, "R2/R4 cursor stutter");
        // R6: a SessionCommand row is not an ordinary child/report admission.
        // Same original read role sees the row but cannot grant its ACK.
        let mut scoped = command.clone();
        scoped.operation_id = "reserved-root-operation".into();
        scoped.thread_id = awaken_session_execution_contract::coordinated_thread_id(
            &scoped.session_id,
            &scoped.parent_run_id,
            &scoped.operation_id,
        );
        scoped.run_id = awaken_session_execution_contract::coordinated_run_id(
            &scoped.session_id,
            &scoped.thread_id,
            &scoped.parent_run_id,
            &scoped.operation_id,
        );
        let scoped_request = awaken_dispatch_contract::RunDispatch::new(
            coordinated_run_activation(scoped.clone(), "coord-root".into(), Vec::new()),
        )
        .for_session(ThreadId(session.to_string()))
        .with_session_activity_epoch(1)
        .with_session_command_identity();
        dispatch.enqueue(scoped_request).await.unwrap();
        assert!(
            managed
                .coordination
                .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&scoped))
                .await
                .is_err(),
            "R6 no root-reservation ACK"
        );

        // R7: completion tombstones lack immutable body/epoch proof; never
        // fabricate an accepted receipt or turn this retained identity Missing.
        let claimed = dispatch
            .claim_run(
                accepted.run_id(),
                "completion-owner",
                1_000,
                &Default::default(),
            )
            .await
            .unwrap()
            .unwrap();
        dispatch
            .settle(
                accepted.run_id(),
                claimed.lease.epoch(),
                DispatchOutcome::Done,
                &[],
            )
            .await
            .unwrap();
        let completed = managed
            .coordination
            .coordinated_admission_receipt(CoordinatedAdmissionRef::Child(&command))
            .await
            .expect_err("R7 insufficient tombstone");
        assert_eq!(
            completed.kind,
            awaken_session_types::RunErrorKind::Unavailable
        );
    }

    #[test]
    fn coordinated_message_has_one_target_thread_owner_for_spawn_and_follow_up() {
        // Cause/effect graph: C1 a coordinated command starts a Thread; C2 it
        // follows up an existing Thread; C3 the exact durable operation is
        // replayed; C4 a different Run/operation is admitted. Effects: E1 C1
        // and C2 both freeze exactly one user message in the target Run
        // activation; E2 C3 reproduces the same message identity; E3 C4 gets a
        // distinct identity. Decision rules M1=(C1,C3)->E1+E2,
        // M2=(C2,C3)->E1+E2, M3=(*,C4)->E3. Spawn/follow-up is intentionally
        // absent from the identity function: both use the same Thread-owned
        // transition and neither creates PendingInput or an outbox row.
        let thread = ThreadId("agent-thread".into());
        let run = RunId("agent-run".into());
        let spawn =
            coordinated_activation_input("session", &thread, &run, "operation", "first".into());
        let follow_up_replay =
            coordinated_activation_input("session", &thread, &run, "operation", "first".into());
        let next = coordinated_activation_input(
            "session",
            &thread,
            &RunId("next-run".into()),
            "next-operation",
            "second".into(),
        );

        assert_eq!(spawn.len(), 1, "M1/E1");
        assert_eq!(spawn[0].role, Role::User, "M1/E1");
        assert_eq!(spawn[0].text_content(), "first", "M1/E1");
        assert_eq!(spawn, follow_up_replay, "M1+M2/E2");
        assert_ne!(spawn[0].id, next[0].id, "M3/E3");
    }

    #[tokio::test]
    async fn coordinated_child_admission_uses_the_canonical_dispatch_resource_generation() {
        // Cause/effect graph: C1 a prepared Session has no active manifest or a
        // stale active A; C2 its aggregate transition desires B, or is absent
        // for a legacy Session. Effects: E1 production coordinated admission
        // queues desired B before physical publication; E2 desired B wins over
        // stale A; E3 legacy admission falls back to A; E4 the queued manifest
        // owns exact Workspace scope and Worker capability; E5 selection never
        // publishes or replaces active. E6 a real File binding, exact path and
        // read-only access survive the envelope; revision-only empty fixtures
        // cannot establish the file projection that the writer needs.
        //
        // | Rule | Active | Transition | Queued | Active after | Effects    |
        // | C1   | none   | 0->B       | B      | none         | E1,E4,E5  |
        // | C2   | A      | A->B       | B      | A            | E2,E4,E5  |
        // | C3   | A      | none       | A      | A            | E3,E4,E5  |

        let manifest = |revision| {
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
                    vec![awaken_session_configuration_contract::resource::ResolvedInput {
                        created_at_unix_ms: None,
                        updated_at_unix_ms: None,
                        binding_id: awaken_resource_contract::BindingId::from(format!(
                            "file-binding-{revision}"
                        )),
                        source: awaken_session_configuration_contract::resource::ResolvedInputSource::File {
                            file_id: format!("file-{revision}").into(),
                        },
                        mount_path: format!("/mnt/files/confirmed-{revision}.json"),
                        access: awaken_resource_contract::ResourceAccess::ReadOnly,
                        instructions: None,
                    }],
                    Vec::new(),
                )
                .expect("File-bearing manifest"),
            )
        };
        for (rule, active_revision, desired_revision, expected_revision) in [
            ("C1", None, Some(7), 7),
            ("C2", Some(4), Some(7), 7),
            ("C3", Some(4), None, 4),
        ] {
            let session_id = format!("coordinated-resource-{rule}");
            let child_thread = ThreadId(format!("coordinated-resource-child-{rule}"));
            let child_run = RunId(format!("coordinated-resource-run-{rule}"));
            let active = active_revision.map(manifest);
            let transition = desired_revision.map(|revision| {
                awaken_session_configuration_contract::resource::SessionResourceTransition::new(
                    active.clone().unwrap_or_else(|| manifest(0)),
                    manifest(revision),
                )
                .expect("test transition belongs to one Workspace")
            });
            let baseline = awaken_session_configuration_contract::SessionBaseline::compile(
                awaken_session_configuration_contract::SessionBaselineInputs {
                    environment: awaken_session_configuration_contract::EnvironmentSnapshot {
                        environment_id: format!("coordinated-resource-environment-{rule}"),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
                            format!("coordinated-resource-environment-{rule}-v1"),
                        ),
                        sandbox: Default::default(),
                        sandbox_provisioning: Default::default(),
                        idle_retention: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
                        credential_realization:
                            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
                    },
                    runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                    mcp_authoring: Default::default(),
                    agent_id: "agent-a".into(),
                    agent_revision: Some(1),
                    model: "model-a".into(),
                    model_override: None,
                    runtime: Some("native".into()),
                    delegate_ids: Vec::new(),
                    toolsets: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                },
            );
            let clock = std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0));
            let dispatch = Arc::new(
                awaken_dispatch::open_sqlite_dispatch_store_in_memory(clock.clone())
                    .await
                    .expect("coordinated Resource dispatch store"),
            );
            let host = (crate::HostDependencies::ephemeral(
                Arc::new(crate::host::MemoryHostModel),
                "stub",
            )
            .await
            .with_ephemeral_dispatch(dispatch.clone()))
            .build()
            .expect("validated Host dependencies");
            host.run_assembly
                .installed_sessions
                .update(&session_id, |slot| {
                    slot.run.session_dispatch = true;
                    slot.replace_frozen_with(|next| {
                        next.baseline = Some(baseline);
                        // This resource-generation fixture explicitly supplies the
                        // coordinator's unrestricted configuration; missing input is
                        // not evidence of an unrestricted parent.
                        next.tools = Some(Default::default());
                    });
                    slot.realization.staging_mut().manifest = active.clone();
                    slot.realization.staging_mut().transition = transition;
                });
            let mut snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("agent-a")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model-a", "native",
                ))
                .build();
            snapshot.metadata.source.agent_id = snapshot.root_agent_id.clone();
            snapshot.metadata.source.revision = 1;
            snapshot
                .recompute_fingerprint()
                .expect("published child snapshot");

            let receipt = host
                .coordination
                .admit_coordinated_run(CoordinatedRunCommand {
                    intent: CoordinatedRunIntent::Spawn,
                    session_id: awaken_session_types::SessionId::from(&session_id),
                    thread_id: child_thread.clone(),
                    run_id: child_run.clone(),
                    parent_run_id: RunId(format!("coordinated-resource-parent-{rule}")),
                    parent_call_id: format!("coordinated-resource-call-{rule}"),
                    operation_id: format!("coordinated-resource-operation-{rule}"),
                    snapshot,
                    message: format!("coordinate Resource rule {rule}"),
                    session_activity_epoch: Some(1),
                    max_unarchived_threads: 24,
                })
                .await
                .unwrap_or_else(|error| panic!("{rule} production child admission: {error}"));
            assert_eq!(receipt.thread_id, child_thread, "{rule} child identity");

            clock.set(1);
            let claimed = dispatch
                .claim_run(
                    &child_run,
                    "resource-inspector",
                    30_000,
                    &Default::default(),
                )
                .await
                .unwrap_or_else(|error| panic!("{rule} inspect queued child: {error}"))
                .unwrap_or_else(|| panic!("{rule} queued child is claimable"));
            let carried = claimed
                .request
                .session_resources
                .as_ref()
                .unwrap_or_else(|| panic!("{rule} child Resource envelope"))
                .decode_with(
                    awaken_session_configuration_contract::resource::SessionResourceManifest::decode_resources,
                )
                .unwrap_or_else(|error| panic!("{rule} decode child Resource envelope: {error}"));
            assert_eq!(carried.revision, expected_revision, "{rule} generation");
            assert_eq!(
                carried,
                manifest(expected_revision),
                "{rule} exact manifest"
            );
            assert_eq!(
                claimed.request.execution_scope,
                Some(awaken_tenancy::ExecutionScopeRef(
                    awaken_tenancy::ScopeId::from("workspace-a")
                )),
                "{rule}/E4 exact scope",
            );
            assert!(
                claimed
                    .request
                    .placement
                    .required_capabilities
                    .contains(awaken_dispatch_contract::SESSION_RESOURCES_CAPABILITY),
                "{rule}/E4 eligible Worker capability",
            );
            assert_eq!(
                host.realization_executor
                    .staging()
                    .thread_resource_manifest(&session_id)
                    .expect("installed Sessions"),
                active,
                "{rule}/E5 admission must not publish active",
            );
            // W1 current unrestricted coordinator input is still explicit;
            // W2 a later projected tool update applies to the next child turn,
            // while the already-queued activation remains byte-stable. These
            // are execution-projection tests; Session CAS update admission has
            // its own existing application tests.
            let before = claimed.request.activation.clone();
            assert_eq!(
                before.inherited_toolsets().unwrap().sources(),
                &[Vec::new()],
                "W1 exact source"
            );
            let updated_tools = awaken_session_configuration_contract::SessionToolConfiguration {
                toolsets: vec![awaken_runtime_contract::agent_bindings::ToolsetPolicy {
                    source: awaken_runtime_contract::agent_bindings::ToolsetSource::Agent,
                    default: Default::default(),
                    overrides: vec![awaken_runtime_contract::agent_bindings::ToolPolicyOverride::with_optional_configuration(
                        "web_fetch", Default::default(), Some(serde_json::json!({"type":"web_fetch","domains":{"type":"allow","domains":["docs.example.com"]}})),
                    )],
                }],
                client_tools: Vec::new(),
            };
            host.run_assembly
                .installed_sessions
                .replace_frozen_with(&session_id, |next| next.tools = Some(updated_tools.clone()));
            let mut target = awaken_runtime_contract::ExecutableAgentSnapshot::builder("roster-b")
                .model(awaken_runtime_contract::resolved::ModelBinding::new(
                    "test", "model-b", "native",
                ))
                .build();
            target.metadata.source.agent_id = target.root_agent_id.clone();
            target.metadata.source.revision = 1;
            target.recompute_fingerprint().unwrap();
            let own = target.resolved_spec.plugin_config.agent.toolsets.clone();
            let next_run = RunId(format!("coordinated-resource-next-{rule}"));
            host.coordination
                .admit_coordinated_run(CoordinatedRunCommand {
                    intent: CoordinatedRunIntent::Spawn,
                    session_id: awaken_session_types::SessionId::from(&session_id),
                    thread_id: ThreadId(format!("coordinated-resource-next-thread-{rule}")),
                    run_id: next_run.clone(),
                    parent_run_id: RunId("parent-source-next".into()),
                    parent_call_id: "next-call".into(),
                    operation_id: format!("next-source-{rule}"),
                    snapshot: target,
                    message: "next turn".into(),
                    session_activity_epoch: Some(1),
                    max_unarchived_threads: 24,
                })
                .await
                .unwrap();
            let next = dispatch
                .claim_run(&next_run, "source-inspection", 1_000, &Default::default())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                next.request
                    .activation
                    .inherited_toolsets()
                    .unwrap()
                    .sources(),
                &[updated_tools.toolsets],
                "W2 next turn gets current coordinator"
            );
            assert_eq!(
                next.request
                    .activation
                    .snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .toolsets,
                own,
                "W2 ID roster keeps own publication tools"
            );
            assert_eq!(
                claimed.request.activation, before,
                "W2 existing run remains frozen"
            );

            use awaken_dispatch_contract::DispatchEnqueue as _;
            use awaken_session_execution_contract::SessionCoordinationReads as _;
            let managed = host.clone();
            let baseline = host
                .run_assembly
                .installed_sessions
                .read(&session_id, |slot| slot.inputs.baseline.clone())
                .unwrap()
                .unwrap();
            // R8 exact report Message and source Run bind the original continuation;
            // a changed rendered name/body is not guessed from current configuration.
            let report = SessionAgentReportContinuation {
                session_id: awaken_session_types::SessionId::from(&session_id),
                source_thread_id: child_thread.clone(),
                source_run_id: child_run.clone(),
                session_activity_epoch: 1,
                message: Message::text(
                    awaken_agent_contract::agent::message::Id::agent_thread_report(&child_run),
                    Role::User,
                    "Message from agent Child: original report",
                ),
            };
            let report_request = awaken_dispatch_contract::RunDispatch::new(RunActivation::new(
                report.run_id(),
                ThreadId(report.session_id.to_string()),
                before.snapshot.clone(),
                vec![report.message.clone()],
            ))
            .for_session(ThreadId(report.session_id.to_string()))
            .with_session_activity_epoch(1);
            dispatch.enqueue(report_request).await.unwrap();
            let report_rows = dispatch.list_dispatches().await.unwrap();
            assert_eq!(
                managed
                    .coordination
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report, &baseline
                        )
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .thread_id
                    .0,
                report.session_id.as_str()
            );
            let mut changed = report.clone();
            changed.message = Message::text(
                changed.message.id.clone(),
                Role::User,
                "Message from agent Renamed: original report",
            );
            assert!(
                managed
                    .coordination
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &changed, &baseline
                        )
                    )
                    .await
                    .is_err(),
                "R8 no body normalization"
            );
            assert_eq!(
                dispatch.list_dispatches().await.unwrap(),
                report_rows,
                "R8 no effect"
            );
            let mut unknown_root = baseline.clone();
            unknown_root.agent_revision = None;
            assert!(
                managed
                    .coordination
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report,
                            &unknown_root
                        )
                    )
                    .await
                    .is_err(),
                "R8 legacy root pin stays Unknown"
            );
            unknown_root.agent_revision = Some(2);
            assert!(
                managed
                    .coordination
                    .coordinated_admission_receipt(
                        awaken_session_execution_contract::CoordinatedAdmissionRef::Report(
                            &report,
                            &unknown_root
                        )
                    )
                    .await
                    .is_err(),
                "R8 different root pin"
            );
        }
    }
}
