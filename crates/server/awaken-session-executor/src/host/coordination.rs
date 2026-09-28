// @formal entrypoint session.agent_message_requires_exact_committed_tool_source_and_payload
// @formal entrypoint tool_result.conflicting_delivery_operation_never_reuses_a_receipt
// @formal entrypoint tool_result.exact_delivery_retry_replays_one_receipt
//! Durable admission and derived reads for Managed Agent coordination.
//!
//! A child remains an ordinary `RunDispatch` whose logical Thread is stored in
//! the parent Session's commit partition. Relationships are reconstructed from
//! the parent's committed `send_message` tool pairs; this module owns no
//! registry, receipt map, mailbox, or background executor.

use super::*;
use awaken_dispatch_contract::ControlDispatchQueue;

mod agent_message_source;
mod reads;
mod session_reply_receipt;

pub(super) use session_reply_receipt::{SessionReplyReceipt, validate_awaiting_tool_authority};

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::delegation::{DelegationId, DelegationOrigin};
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecycleFeed as _};
use awaken_ext_builtin_tools::{AgentMessageReceipt, SendMessageArgs};
use awaken_dispatch::Outbox as _;
use awaken_runtime_contract::tool_batch::ToolBatch;
use awaken_session_execution_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadKind, CoordinatedThreadLink,
    CoordinatedThreadTarget, SessionAgentMessageCommand, SessionAgentMessageReceipt,
    SessionAgentReportContinuation, coordinated_thread_closed,
};

const MAX_LIFECYCLE_PAGE: usize = 1_024;

fn reply_dispatch_activity(
    dispatch: Option<&awaken_dispatch::DispatchSummary>,
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

fn dispatch_admission_error(error: awaken_dispatch::DispatchError) -> HostError {
    match error {
        awaken_dispatch::DispatchError::Rejected(message) => HostError::bad_request(message),
        awaken_dispatch::DispatchError::Conflict(message) => HostError::conflict(message),
        awaken_dispatch::DispatchError::Unavailable(message) => HostError::unavailable(message),
    }
}

fn coordinated_activation_input(
    session_id: &str,
    thread_id: &ThreadId,
    run_id: &RunId,
    operation_id: &str,
    message: String,
) -> Vec<Message> {
    vec![Message::text(
        MessageId(format!(
            "coord-input-{}",
            awaken_agent_contract::stable_fingerprint(&(
                session_id,
                thread_id.0.as_str(),
                run_id.0.as_str(),
                operation_id,
            ))
        )),
        Role::User,
        message,
    )]
}

#[derive(Clone)]
struct PendingCoordinationCall {
    run_id: RunId,
    step: usize,
    call_id: String,
    args: SendMessageArgs,
}

fn project_committed_coordination_links(
    session_id: &awaken_session_types::SessionId,
    advisor_model: Option<&str>,
    committed: &RunRecoverySnapshot,
) -> Result<Vec<CoordinatedThreadLink>, HostError> {
    let mut active = Vec::<PendingCoordinationCall>::new();
    let mut links = Vec::<CoordinatedThreadLink>::new();
    let mut runs = std::collections::HashMap::<_, Option<&RunId>>::new();
    for run in &committed.runs {
        runs.entry(run.id.0.as_str())
            .and_modify(|selected| *selected = None)
            .or_insert(Some(&run.id));
    }
    for message in &committed.messages {
        if message.role == Role::Assistant {
            active.clear();
            let Some((owner, step, None)) = message.id.assistant_coordinates() else {
                continue;
            };
            let run_id = match runs.get(owner).copied().flatten() {
                Some(run_id) => run_id,
                None if runs.contains_key(owner) => {
                    return Err(HostError::internal(
                        "committed assistant message has ambiguous Run coordinates",
                    ));
                }
                None => continue,
            };
            for block in &message.content {
                if let ContentBlock::ToolUse { id, name, input } = block
                    && name == awaken_ext_builtin_tools::SEND_MESSAGE
                {
                    let args = serde_json::from_value::<SendMessageArgs>(input.clone()).map_err(
                        |error| {
                            HostError::internal(format!(
                                "committed send_message arguments are invalid: {error}"
                            ))
                        },
                    )?;
                    active.push(PendingCoordinationCall {
                        run_id: run_id.clone(),
                        step,
                        call_id: id.clone(),
                        args,
                    });
                } else if let ContentBlock::ToolUse { id, name, .. } = block
                    && name == awaken_runtime_contract::resolved::ADVISOR_TOOL_ID
                {
                    apply_advisor_call(
                        session_id,
                        run_id,
                        step,
                        id,
                        advisor_model.unwrap_or_default(),
                        &mut links,
                    )?;
                }
            }
            continue;
        }
        for block in &message.content {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = block
            else {
                continue;
            };
            let Some(position) = active.iter().position(|call| call.call_id == *tool_use_id) else {
                continue;
            };
            let call = active.remove(position);
            apply_coordination_result(session_id, &call, content, *is_error, &mut links)?;
        }
    }

    // Tool results publish to the parent transcript only after the whole
    // assistant batch finishes. The snapshot's state is the matching prefix,
    // so the existing ActiveToolBatch remains the sole crash authority during
    // that publication gap; no relationship row or second read is introduced.
    let state = Store::rebuild(&committed.state);
    let active_batch = awaken_runtime_contract::ActiveToolBatch::load(&state)
        .map_err(|error| HostError::internal(error.to_string()))?;
    apply_active_batch_coordination_results(
        session_id,
        &active,
        active_batch.as_ref(),
        &mut links,
    )?;
    Ok(links)
}

fn apply_lifecycle_latest_runs(
    links: &mut [CoordinatedThreadLink],
    events: &[awaken_agent_contract::RunLifecycleEvent],
    store_cursor: u64,
) -> bool {
    for event in events {
        if event.source_commit_cursor > store_cursor {
            return true;
        }
        if let Some(link) = links
            .iter_mut()
            .find(|link| link.thread_id == event.thread_id)
        {
            link.latest_run_id = Some(event.run_id.clone());
        }
    }
    false
}

fn apply_advisor_call(
    session_id: &awaken_session_types::SessionId,
    run_id: &RunId,
    step: usize,
    call_id: &str,
    model: &str,
    links: &mut Vec<CoordinatedThreadLink>,
) -> Result<(), HostError> {
    let child_run_id = DelegationId::for_parent_call(run_id, call_id).child_run_id();
    let thread_id = ThreadId(child_run_id.0.clone());
    let operation_id = ToolBatch::operation_id_for_step(run_id, step, call_id);
    if let Some(existing) = links.iter().find(|link| link.thread_id == thread_id) {
        if existing.target.advisor_model() != Some(model)
            || existing.created_by_operation_id != operation_id
        {
            return Err(HostError::internal(
                "one advisor Thread is attributed to conflicting committed calls",
            ));
        }
        return Ok(());
    }
    links.push(CoordinatedThreadLink {
        session_id: session_id.clone(),
        thread_id,
        target: CoordinatedThreadTarget::Advisor {
            model: model.to_string(),
        },
        created_by_operation_id: operation_id,
        latest_run_id: Some(child_run_id),
    });
    Ok(())
}

fn apply_coordination_result(
    session_id: &awaken_session_types::SessionId,
    call: &PendingCoordinationCall,
    content: &[ContentBlock],
    is_error: bool,
    links: &mut Vec<CoordinatedThreadLink>,
) -> Result<(), HostError> {
    if is_error {
        return Ok(());
    }
    let receipt: AgentMessageReceipt =
        serde_json::from_str(&extract_text(content)).map_err(|e| {
            HostError::internal(format!(
                "committed send_message result has an invalid receipt: {e}"
            ))
        })?;
    if !receipt.accepted || receipt.session_thread_id.trim().is_empty() {
        return Err(HostError::internal(
            "committed send_message result is not an accepted Thread receipt",
        ));
    }
    let thread_id = ThreadId(receipt.session_thread_id);
    let operation_id = ToolBatch::operation_id_for_step(&call.run_id, call.step, &call.call_id);
    match (
        call.args
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        call.args
            .session_thread_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
    ) {
        (Some(agent_id), None) => {
            let expected = awaken_session_execution_contract::coordinated_thread_id(
                session_id,
                &call.run_id,
                &operation_id,
            );
            if thread_id != expected {
                return Err(HostError::internal(
                    "committed coordination receipt does not match its deterministic Thread",
                ));
            }
            if let Some(existing) = links.iter().find(|link| link.thread_id == thread_id) {
                if existing.target.agent_id() != Some(agent_id) {
                    return Err(HostError::internal(
                        "one coordinated Thread is attributed to multiple Agents",
                    ));
                }
                return Ok(());
            }
            links.push(CoordinatedThreadLink {
                session_id: session_id.clone(),
                thread_id,
                target: CoordinatedThreadTarget::Agent {
                    agent_id: agent_id.to_string(),
                },
                created_by_operation_id: operation_id,
                latest_run_id: None,
            });
        }
        (None, Some(target)) => {
            if thread_id.0 != target || !links.iter().any(|link| link.thread_id == thread_id) {
                return Err(HostError::internal(
                    "committed follow-up receipt names an unknown coordinated Thread",
                ));
            }
        }
        _ => {
            return Err(HostError::internal(
                "committed send_message arguments have an invalid target",
            ));
        }
    }
    Ok(())
}

fn apply_active_batch_coordination_results(
    session_id: &awaken_session_types::SessionId,
    active: &[PendingCoordinationCall],
    batch: Option<&awaken_runtime_contract::ToolBatch>,
    links: &mut Vec<CoordinatedThreadLink>,
) -> Result<(), HostError> {
    let Some(batch) = batch else {
        return Ok(());
    };
    for call in active {
        if batch.run_id() != &call.run_id {
            continue;
        }
        let Some(entry) = batch.calls().iter().find(|entry| {
            entry.call.call_id == call.call_id
                && entry.call.tool_id == awaken_ext_builtin_tools::SEND_MESSAGE
        }) else {
            continue;
        };
        for message in &entry.result_messages {
            for block in &message.content {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = block
                    && tool_use_id == &call.call_id
                {
                    apply_coordination_result(session_id, call, content, *is_error, links)?;
                }
            }
        }
    }
    Ok(())
}

impl SharedHost {
    /// Resolve a public reply to the one currently committed Awaiting ticket.
    /// Thread recovery remains the authority; the Session event identity and
    /// version are only admission fences against delayed or reused call ids.
    pub(super) async fn validated_session_thread_tool_reply(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<(ThreadId, ResumeTicket), HostError> {
        if command.session_id.as_str().trim().is_empty()
            || command.expected_run_id.0.trim().is_empty()
            || command.expected_correlation_id.trim().is_empty()
            || command.tool_use_id.trim().is_empty()
        {
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
        let commit = self.commit_for_read(command.session_id.as_str()).await?;
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
        self.check_pending(
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
        let commit = self.commit_for_read(session_id).await?;
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
        self.dispatch_store()?
            .session_continuation()
            .ok_or_else(|| HostError::internal("Session reply requires a continuation owner"))?;
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
            .dispatch_store()?
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

        let store = self.dispatch_store()?;
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
            .session_continuation()
            .ok_or_else(|| HostError::internal("Session reply requires a continuation owner"))?
            .stage_session_resume(
                input,
                &parent,
                fence.activity.prior_epoch(),
                session_activity_epoch,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.notify().await;
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
    ) -> Result<awaken_dispatch::SessionChildAdmission, HostError> {
        if max_unarchived_threads == 0 {
            return Err(HostError::bad_request(
                "Session child capacity must be greater than zero",
            ));
        }
        let links = self.coordinated_threads(session_id, None).await?;
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
            awaken_dispatch::SessionChildAdmission::new(max_unarchived_threads, archived)
                .with_capacity_exempt_threads(capacity_exempt),
        )
    }

    pub(crate) async fn admit_coordinated_run(
        &self,
        command: CoordinatedRunCommand,
    ) -> Result<SessionAgentMessageReceipt, HostError> {
        let CoordinatedRunCommand {
            intent,
            session_id,
            thread_id,
            run_id,
            parent_run_id,
            parent_call_id,
            operation_id,
            snapshot,
            message,
            session_activity_epoch,
            max_unarchived_threads,
        } = command;
        let (baseline, tools, resources) = self
            .session_slots
            .read(session_id.as_str(), |slot| {
                (
                    slot.baseline.clone(),
                    slot.tools.clone(),
                    // Coordinated children share the exact generation selected
                    // for root and extension dispatch. Selection stays inside
                    // this one slot snapshot; child admission neither publishes
                    // nor reconstructs physical active truth.
                    slot.dispatch_resource_manifest(),
                )
            })
            .ok_or_else(|| HostError::bad_request("Session runtime projection is unavailable"))?;
        let baseline = baseline.ok_or_else(|| {
            HostError::bad_request("Session frozen baseline is unavailable for coordination")
        })?;
        let resources = resources.ok_or_else(|| {
            HostError::bad_request("Session resource manifest is unavailable for coordination")
        })?;
        if snapshot.metadata.source.revision == 0
            || snapshot.metadata.source.agent_id != snapshot.root_agent_id
        {
            return Err(HostError::bad_request(
                "coordinated Run does not carry an exact Agent publication",
            ));
        }

        let mut snapshot = snapshot;
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
        let activation_input = coordinated_activation_input(
            session_id.as_str(),
            &thread_id,
            &run_id,
            &operation_id,
            message,
        );
        let origin = DelegationOrigin {
            delegation_id: DelegationId(format!(
                "coordination:{}",
                awaken_agent_contract::stable_fingerprint(&(
                    session_id.as_str(),
                    operation_id.as_str(),
                ))
            )),
            parent_run_id: parent_run_id.clone(),
            parent_call_id,
            depth: 1,
            agent_lineage: vec![baseline.agent_id],
        };
        let activation = RunActivation::new(run_id, thread_id.clone(), snapshot, activation_input)
            .with_delegation_origin(origin);
        let request = crate::agent_runner::child_dispatch_request(
            activation,
            ThreadId(session_id.to_string()),
            Some(resources),
            self.agent_publications.as_deref(),
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
        let store = self.dispatch_store()?;

        store
            .enqueue_session_child(request, admission)
            .await
            .map_err(dispatch_admission_error)?;
        // Thread disposition, Run commits, and dispatch admission may use
        // different durable adapters. Re-read both existing admission fences
        // after enqueue: whichever side won records cancellation on this exact
        // dispatch and waits for settlement. No relation flag, terminal cache,
        // or process-local lock is introduced.
        let archived = self
            .session_thread_disposition(session_id.as_str(), &thread_id)
            .await?
            == awaken_agent_contract::ThreadDisposition::Archived;
        // Follow-up admission above accepted only ordinary Agent Threads.
        let failed = intent == CoordinatedRunIntent::FollowUp
            && self
                .coordinated_thread_is_closed(
                    session_id.as_str(),
                    &thread_id,
                    CoordinatedThreadKind::Agent,
                )
                .await?;
        if archived || failed {
            // This admission may already be durable, so it is not a definitive
            // caller rejection until cancellation has settled. Once quiescent,
            // the bad request is safe: no accepted child execution remains.
            self.cancel_and_await_coordinated_thread_quiescence(session_id.as_str(), &thread_id)
                .await?;
            return Err(HostError::bad_request(if archived {
                "Agent Thread is archived"
            } else {
                "Agent Thread terminated after a failed Run"
            }));
        }
        if let Some(pool) = self.dispatch_pool.get() {
            pool.notify().await;
        }
        Ok(SessionAgentMessageReceipt { thread_id })
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
        let store = self.dispatch_store()?;

        let ctx = self.ctx_for(command.session_id.as_str(), None).await?;
        let run_id = RunId(format!(
            "coord-report-run-{}",
            awaken_agent_contract::stable_fingerprint(&(
                "managed-agent-report-run-v1",
                command.session_id.as_str(),
                command.source_run_id.0.as_str(),
            ))
        ));
        let activation = RunActivation::new(
            run_id,
            ThreadId(command.session_id.to_string()),
            ctx.config.clone(),
            vec![command.message],
        )
        .with_model_ref_override(
            self.inference_routing
                .override_for(command.session_id.as_str()),
        );
        // The report command itself is durable Session provenance. Do not rely
        // on a process-local prepared slot to rediscover self-affinity after a
        // cold restart; the settlement observer must always route this root
        // continuation back to the same Session application.
        let request = self
            .resolved_dispatch(activation)?
            .for_session(ThreadId(command.session_id.to_string()))
            .with_session_activity_epoch(command.session_activity_epoch);
        store
            .enqueue(request)
            .await
            .map_err(dispatch_admission_error)?;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.notify().await;
        }
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
    ) -> Result<Vec<awaken_dispatch::DispatchSummary>, HostError> {
        let parent = ThreadId(session_id.to_string());
        Ok(self
            .dispatch_store()?
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
        self.interrupt_session_thread(session_id, thread_id).await?;
        if let Some(wake) = self
            .authority
            .as_ref()
            .and_then(|authority| authority.dispatch_wake())
        {
            // This is only a latency hint. The durable cancellation intent and
            // dispatch row remain the recovery authority if the hint is lost.
            awaken_dispatch::publish_dispatch_hint(wake.as_ref(), &CancellationToken::new())
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
                    .any(|dispatch| dispatch_remains_unsettled(dispatch.state));
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
                    awaken_dispatch::DispatchState::Reserved
                        | awaken_dispatch::DispatchState::ReservationLeased
                        | awaken_dispatch::DispatchState::Pending
                        | awaken_dispatch::DispatchState::Leased
                )
            })
        {
            return Err(HostError::bad_request(
                "only an idle Agent Thread may be archived",
            ));
        }
        if already_archived
            || child_rows
                .iter()
                .any(|dispatch| dispatch.state == awaken_dispatch::DispatchState::Awaiting)
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

        let commit = self.commit_for_read(session_id).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::tool::{ToolOutput, ToolRecoveryPolicy};

    #[test]
    fn coordination_uses_the_neutral_assistant_decoder_for_exact_run_membership() {
        // Causes: C1 full/truncated/malformed Assistant identity; C2 selected,
        // foreign or delimiter-containing Run; C3 Assistant/User role; C4 zero
        // or 10,000 unrelated selected Runs. Effects: only the full Assistant
        // message owned by the exact selected nested Run projects one Advisor
        // link, with the same Run/Step/call identity. This is the Host integration
        // of MessageId's exhaustive codec table; it introduces no second parser.
        let selected = RunId("root-assistant-0".into());
        let mut runs = (0..10_000)
            .map(|i| awaken_agent_contract::agent::run::Record {
                id: RunId(format!("unrelated-{i}")),
                thread_id: ThreadId("session".into()),
                state: RunState::Running,
            })
            .collect::<Vec<_>>();
        runs.extend([
            awaken_agent_contract::agent::run::Record {
                id: RunId("root".into()),
                thread_id: ThreadId("session".into()),
                state: RunState::Running,
            },
            awaken_agent_contract::agent::run::Record {
                id: selected.clone(),
                thread_id: ThreadId("session".into()),
                state: RunState::Running,
            },
        ]);
        let advisor = |id, role| {
            Message::new(
                id,
                role,
                vec![ContentBlock::tool_use(
                    "advisor-call",
                    awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                    serde_json::json!({}),
                )],
            )
        };
        let snapshot = RunRecoverySnapshot {
            thread_id: ThreadId("session".into()),
            claimed_run_id: selected.clone(),
            runs,
            latest_run_id: Some(selected.clone()),
            messages: vec![
                advisor(MessageId::assistant(&selected, 3), Role::Assistant),
                advisor(
                    MessageId::assistant_truncated(&selected, 3, 0),
                    Role::Assistant,
                ),
                advisor(
                    MessageId::assistant(&RunId("foreign".into()), 3),
                    Role::Assistant,
                ),
                advisor(MessageId::assistant(&selected, 4), Role::User),
                advisor(MessageId("root-assistant-bad".into()), Role::Assistant),
            ],
            message_commit_cursors: vec![1, 2, 3, 4, 5],
            commit_timestamps: Default::default(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 5,
            store_cursor: 5,
            next_commit_ordinal: 1,
        };
        let links = project_committed_coordination_links(
            &awaken_session_types::SessionId::from("session"),
            Some("advisor-model"),
            &snapshot,
        )
        .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target.advisor_model(), Some("advisor-model"));
        assert_eq!(
            links[0].latest_run_id,
            Some(DelegationId::for_parent_call(&selected, "advisor-call").child_run_id())
        );
        assert_eq!(
            links[0].created_by_operation_id,
            ToolBatch::operation_id_for_step(&selected, 3, "advisor-call")
        );
        let mut ambiguous = snapshot.clone();
        ambiguous
            .runs
            .push(awaken_agent_contract::agent::run::Record {
                id: selected,
                thread_id: ThreadId("session".into()),
                state: RunState::Running,
            });
        let error = project_committed_coordination_links(
            &awaken_session_types::SessionId::from("session"),
            Some("advisor-model"),
            &ambiguous,
        )
        .unwrap_err();
        assert!(error.message.contains("ambiguous Run coordinates"));
    }

    #[test]
    fn dispatch_admission_errors_preserve_commit_certainty() {
        // Cause/effect graph: C1 validation proves no write; C2 an existing
        // identity has another payload; C3 storage or transport cannot prove
        // whether the write committed. Effects: E1 caller correction may close
        // a new activity; E2 the original activity remains recoverable; E3 exact
        // retry remains mandatory. Decision table: R1=C1->BadRequest/E1,
        // R2=C2->Conflict/E2, R3=C3->Unavailable/E3.
        let rejected = dispatch_admission_error(awaken_dispatch::DispatchError::Rejected(
            "invalid shape".into(),
        ));
        let conflict = dispatch_admission_error(awaken_dispatch::DispatchError::Conflict(
            "identity collision".into(),
        ));
        let unavailable = dispatch_admission_error(awaken_dispatch::DispatchError::Unavailable(
            "response lost".into(),
        ));
        assert_eq!(rejected.kind, HostErrorKind::BadRequest, "R1/E1");
        assert_eq!(conflict.kind, HostErrorKind::Conflict, "R2/E2");
        assert_eq!(unavailable.kind, HostErrorKind::Unavailable, "R3/E3");
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
        // publishes or replaces active.
        //
        // | Rule | Active | Transition | Queued | Active after | Effects    |
        // | C1   | none   | 0->B       | B      | none         | E1,E4,E5  |
        // | C2   | A      | A->B       | B      | A            | E2,E4,E5  |
        // | C3   | A      | none       | A      | A            | E3,E4,E5  |
        use awaken_dispatch::DispatchQueue as _;

        let manifest = |revision| {
            awaken_session_resource_contract::SessionResourceManifest::at_revision(
                "workspace-a",
                revision,
                awaken_session_resource_contract::ResolvedSessionResources::default(),
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
                awaken_session_resource_contract::SessionResourceTransition::new(
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
                            awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
                    },
                    runtime_placement: awaken_session_configuration_contract::SessionRuntimePlacement::Worker,
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
                awaken_dispatch::AnyDispatchStore::open_sqlite_in_memory(clock.clone())
                    .expect("coordinated Resource dispatch store"),
            );
            let host = SharedHost::new(Arc::new(crate::host::MemoryHostModel), "stub")
                .with_dispatch_store(dispatch.clone());
            host.session_slots.update(&session_id, |slot| {
                slot.session_dispatch = true;
                slot.baseline = Some(baseline);
                slot.manifest = active.clone();
                slot.resource_transition = transition;
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
                    session_activity_epoch: 1,
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
                .decode_manifest()
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
                    .contains(awaken_dispatch::SESSION_RESOURCES_CAPABILITY),
                "{rule}/E4 eligible Worker capability",
            );
            assert_eq!(
                host.thread_resource_manifest(&session_id),
                active,
                "{rule}/E5 admission must not publish active",
            );
        }
    }

    #[test]
    fn advisor_call_identity_is_one_thread_per_consultation_and_idempotent_per_replay() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 the same committed `(Run,call)` is replayed;
        // C2 a second advisor call occurs in that Run; C3 the same call id occurs
        // in another Run; C4 one identity is attributed to a conflicting model.
        // Effects: E1 C1 reuses its one derived link; E2 C2/C3 each derive a new
        // Thread/operation/Run identity; E3 C4 fails instead of mutating or
        // duplicating the authoritative link.
        //
        // Decision table:
        // | Rule | same Run | same call | same model | Effect |
        // | I1   | yes      | yes       | yes        | E1     |
        // | I2   | yes      | no        | yes        | E2     |
        // | I3   | no       | yes       | yes        | E2     |
        // | I4   | yes      | yes       | no         | E3     |
        let session_id = "advisor-session";
        let first_run = RunId("advisor-parent-run-1".into());
        let second_run = RunId("advisor-parent-run-2".into());
        let mut links = Vec::new();

        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &first_run,
            0,
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &first_run,
            0,
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        assert_eq!(links.len(), 1, "I1/E1");

        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &first_run,
            0,
            "advisor-call-2",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &second_run,
            0,
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        assert_eq!(links.len(), 3, "I2-I3/E2");
        assert_eq!(
            links
                .iter()
                .map(|link| link.thread_id.0.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "I2-I3/E2 unique consultation Threads"
        );
        assert_eq!(
            links
                .iter()
                .map(|link| link.created_by_operation_id.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            3,
            "I2-I3/E2 unique operations"
        );
        assert!(links.iter().all(|link| {
            link.latest_run_id.as_ref().map(|run| &run.0) == Some(&link.thread_id.0)
                && link.target.advisor_model() == Some("claude-opus-5")
        }));

        assert!(
            apply_advisor_call(
                &awaken_session_types::SessionId::from(session_id),
                &first_run,
                0,
                "advisor-call-1",
                "conflicting-model",
                &mut links,
            )
            .is_err(),
            "I4/E3"
        );
        assert_eq!(links.len(), 3, "I4/E3 leaves links unchanged");
    }

    #[test]
    fn one_recovery_prefix_projects_links_gap_and_fenced_child_runs_warm_or_cold() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 one root recovery snapshot owns Run coordinates,
        // messages, state, and watermark; C2 a send receipt is in the transcript;
        // C3 another send receipt is committed only in ActiveToolBatch; C4 an
        // advisor call is committed; C5 child lifecycle is at/beyond the snapshot
        // watermark; C6 projection is warm or deserialized after restart.
        // Effects: E1 transcript and batch each derive exactly one Agent link;
        // E2 missing batch omits only the unpublished link; E3 advisor derives one
        // link; E4 at-fence child Run enriches the link and a later-prefix Run is
        // ignored; E5 warm/cold projections are identical.
        //
        // | Rule | C2 | C3 | C4 | C5             | C6        | Effects    |
        // | P1   | Y  | Y  | Y  | at fence       | warm      | E1,E3,E4  |
        // | P2   | Y  | N  | Y  | at fence       | warm      | E1,E2,E3  |
        // | P3   | Y  | Y  | Y  | beyond fence   | warm      | E4        |
        // | P4   | Y  | Y  | Y  | same sequence  | cold      | E5        |
        let session = "parent-session";
        let run_id = RunId("parent-run".into());
        let step = 2;
        let published_call = "published-send";
        let gap_call = "gap-send";
        let advisor_call = "advisor-call";
        let published_child = awaken_session_execution_contract::coordinated_thread_id(
            &awaken_session_types::SessionId::from(session),
            &run_id,
            &ToolBatch::operation_id_for_step(&run_id, step, published_call),
        );
        let gap_child = awaken_session_execution_contract::coordinated_thread_id(
            &awaken_session_types::SessionId::from(session),
            &run_id,
            &ToolBatch::operation_id_for_step(&run_id, step, gap_call),
        );
        let advisor_child = ThreadId(
            DelegationId::for_parent_call(&run_id, advisor_call)
                .child_run_id()
                .0,
        );
        let published_receipt = serde_json::to_string(&AgentMessageReceipt {
            session_thread_id: published_child.0.clone(),
            accepted: true,
        })
        .unwrap();
        let gap_receipt = serde_json::to_string(&AgentMessageReceipt {
            session_thread_id: gap_child.0.clone(),
            accepted: true,
        })
        .unwrap();
        let assistant = Message::new(
            MessageId::assistant(&run_id, step),
            Role::Assistant,
            vec![
                ContentBlock::tool_use(
                    published_call,
                    awaken_ext_builtin_tools::SEND_MESSAGE,
                    serde_json::json!({"agent_id": "researcher", "message": "investigate"}),
                ),
                ContentBlock::tool_use(
                    gap_call,
                    awaken_ext_builtin_tools::SEND_MESSAGE,
                    serde_json::json!({"agent_id": "reviewer", "message": "review"}),
                ),
                ContentBlock::tool_use(
                    advisor_call,
                    awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                    serde_json::json!({}),
                ),
            ],
        );
        let published_result = Message::new(
            MessageId::tool_result(published_call),
            Role::Tool,
            vec![ContentBlock::ToolResult {
                tool_use_id: published_call.into(),
                content: vec![ContentBlock::text(published_receipt)],
                is_error: false,
            }],
        );
        let mut batch = ToolBatch::for_step(
            run_id.clone(),
            step,
            [(
                ToolCall {
                    call_id: gap_call.into(),
                    tool_id: awaken_ext_builtin_tools::SEND_MESSAGE.into(),
                    arguments: serde_json::json!({}),
                },
                ToolRecoveryPolicy::default(),
            )],
        )
        .unwrap();
        batch.mark_executing(gap_call).unwrap();
        batch
            .complete(ToolOutput::ok(gap_call, "accepted"))
            .unwrap();
        batch
            .set_result_messages(
                gap_call,
                vec![Message::new(
                    MessageId::tool_result(gap_call),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: gap_call.into(),
                        content: vec![ContentBlock::text(gap_receipt)],
                        is_error: false,
                    }],
                )],
            )
            .unwrap();
        let mut batch_state = awaken_runtime_contract::ActiveToolBatch::write(&Some(batch));
        batch_state.bind_run(&run_id);
        let snapshot = RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId(session.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run_id.clone(),
                thread_id: ThreadId(session.into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run_id.clone()),
            messages: vec![assistant, published_result],
            message_commit_cursors: Vec::new(),
            state: vec![batch_state],
            state_commit_cursors: vec![7],
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 2,
            store_cursor: 7,
            next_commit_ordinal: 2,
        };
        let published_child_run = RunId("published-child-run".into());
        let gap_child_run = RunId("gap-child-run".into());
        let later_gap_run = RunId("later-gap-run".into());
        let lifecycle = vec![
            awaken_agent_contract::RunLifecycleEvent {
                cursor: RunLifecycleCursor(6_000),
                source_commit_cursor: 6,
                thread_id: published_child.clone(),
                run_id: published_child_run.clone(),
                kind: awaken_agent_contract::RunLifecycleEventKind::Running,
                state: RunState::Running,
                await_reason: None,
            },
            awaken_agent_contract::RunLifecycleEvent {
                cursor: RunLifecycleCursor(7_000),
                source_commit_cursor: 7,
                thread_id: gap_child.clone(),
                run_id: gap_child_run.clone(),
                kind: awaken_agent_contract::RunLifecycleEventKind::Running,
                state: RunState::Running,
                await_reason: None,
            },
            awaken_agent_contract::RunLifecycleEvent {
                cursor: RunLifecycleCursor(8_000),
                source_commit_cursor: 8,
                thread_id: gap_child.clone(),
                run_id: later_gap_run,
                kind: awaken_agent_contract::RunLifecycleEventKind::Running,
                state: RunState::Running,
                await_reason: None,
            },
        ];

        let mut warm = project_committed_coordination_links(
            &awaken_session_types::SessionId::from(session),
            Some("advisor-model"),
            &snapshot,
        )
        .expect("P1 one committed root prefix");
        assert!(
            apply_lifecycle_latest_runs(&mut warm, &lifecycle, snapshot.store_cursor),
            "P3/E4 stops at the first event beyond the snapshot watermark"
        );
        assert_eq!(warm.len(), 3, "P1/E1,E3");
        assert!(warm.iter().any(|link| {
            link.thread_id == published_child
                && link.target.agent_id() == Some("researcher")
                && link.latest_run_id.as_ref() == Some(&published_child_run)
        }));
        assert!(warm.iter().any(|link| {
            link.thread_id == gap_child
                && link.target.agent_id() == Some("reviewer")
                && link.latest_run_id.as_ref() == Some(&gap_child_run)
        }));
        assert!(warm.iter().any(|link| {
            link.thread_id == advisor_child && link.target.advisor_model() == Some("advisor-model")
        }));

        let mut without_batch = snapshot.clone();
        without_batch.state.clear();
        let without_batch = project_committed_coordination_links(
            &awaken_session_types::SessionId::from(session),
            Some("advisor-model"),
            &without_batch,
        )
        .expect("P2 snapshot without the publication-gap authority");
        assert_eq!(without_batch.len(), 2, "P2/E1,E2,E3");
        assert!(
            without_batch.iter().all(|link| link.thread_id != gap_child),
            "P2/E2 unpublished link is not invented"
        );

        let cold_snapshot: RunRecoverySnapshot =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        let mut cold = project_committed_coordination_links(
            &awaken_session_types::SessionId::from(session),
            Some("advisor-model"),
            &cold_snapshot,
        )
        .expect("P4 cold recovery projection");
        assert!(apply_lifecycle_latest_runs(
            &mut cold,
            &lifecycle,
            cold_snapshot.store_cursor
        ));
        assert_eq!(cold, warm, "P4/E5 warm and cold committed truth agree");
    }
}
