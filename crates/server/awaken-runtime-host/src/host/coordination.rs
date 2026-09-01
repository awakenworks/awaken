//! Durable admission and derived reads for Managed Agent coordination.
//!
//! A child remains an ordinary `RunDispatch` whose logical Thread is stored in
//! the parent Session's commit partition. Relationships are reconstructed from
//! the parent's committed `send_message` tool pairs; this module owns no
//! registry, receipt map, mailbox, or background executor.

use super::*;

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::delegation::{DelegationId, DelegationOrigin};
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecycleFeed as _};
use awaken_ext_builtin_tools::{AgentMessageReceipt, SendMessageArgs};
use awaken_run_ingress::Outbox as _;
use awaken_runtime_contract::tool_batch::ToolBatch;
use awaken_session_contract::{
    CoordinatedRunCommand, CoordinatedRunIntent, CoordinatedThreadLink, CoordinatedThreadTarget,
    SessionAgentMessageReceipt, SessionAgentReportContinuation, coordinated_thread_failed,
};

const MAX_LIFECYCLE_PAGE: usize = 1_024;

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
            awaken_session_contract::stable_fingerprint(&(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionReplyReceipt {
    Absent,
    Exact,
    Conflict,
}

/// Fold one committed ResumeApplied fact into the reply-recovery decision.
/// This is the sole classifier used by the history scan and its Kani proof:
/// unrelated facts stutter, a same-correlation/different-operation fact closes
/// the reply as conflicting, and an exact operation receipt is absorbing.
const fn advance_session_reply_receipt(
    current: SessionReplyReceipt,
    same_correlation: bool,
    same_operation: bool,
) -> SessionReplyReceipt {
    if matches!(current, SessionReplyReceipt::Exact) {
        return SessionReplyReceipt::Exact;
    }
    if same_correlation && same_operation {
        SessionReplyReceipt::Exact
    } else if same_correlation {
        SessionReplyReceipt::Conflict
    } else {
        current
    }
}

#[cfg(kani)]
#[kani::proof]
fn resume_receipt_classification_is_exact_and_conflict_absorbing() {
    // Cause/effect graph: C1 an Event has the expected correlation; C2 it has
    // the expected operation; C3 an exact receipt was already observed.
    // Effects: E1 C1+C2 selects Exact; E2 C1+!C2 selects Conflict unless E1 is
    // already absorbing; E3 !C1 cannot change the accumulated decision.
    // Decision rules K1=C1+C2=>E1, K2=C1+!C2+!C3=>E2,
    // K3=!C1=>E3, K4=C3=>Exact. These are the complete Boolean partitions.
    let current = match kani::any::<u8>() % 3 {
        0 => SessionReplyReceipt::Absent,
        1 => SessionReplyReceipt::Conflict,
        _ => SessionReplyReceipt::Exact,
    };
    let same_correlation = kani::any::<bool>();
    let same_operation = kani::any::<bool>();
    let next = advance_session_reply_receipt(current, same_correlation, same_operation);

    if current == SessionReplyReceipt::Exact || (same_correlation && same_operation) {
        assert!(next == SessionReplyReceipt::Exact, "K1/K4");
    } else if same_correlation {
        assert!(next == SessionReplyReceipt::Conflict, "K2");
    } else {
        assert!(next == current, "K3");
    }
}

#[derive(Clone)]
struct PendingCoordinationCall {
    run_id: RunId,
    step: usize,
    call_id: String,
    args: SendMessageArgs,
}

fn assistant_coordinates(
    message: &Message,
    runs: &[awaken_agent_contract::agent::run::Record],
) -> Result<Option<(RunId, usize)>, HostError> {
    let mut matches = runs.iter().filter_map(|run| {
        message
            .id
            .assistant_step_of(&run.id)
            .map(|step| (run.id.clone(), step))
    });
    let first = matches.next();
    if matches.next().is_some() {
        return Err(HostError::internal(
            "committed assistant message has ambiguous Run coordinates",
        ));
    }
    Ok(first)
}

fn project_committed_coordination_links(
    session_id: &str,
    advisor_model: Option<&str>,
    committed: &RunRecoverySnapshot,
) -> Result<Vec<CoordinatedThreadLink>, HostError> {
    let mut active = Vec::<PendingCoordinationCall>::new();
    let mut links = Vec::<CoordinatedThreadLink>::new();
    for message in &committed.messages {
        if message.role == Role::Assistant {
            active.clear();
            let Some((run_id, step)) = assistant_coordinates(message, &committed.runs)? else {
                continue;
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
                        &run_id,
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
    session_id: &str,
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
        session_id: session_id.to_string(),
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
    session_id: &str,
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
            let expected = awaken_session_contract::coordinated_thread_id(
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
                session_id: session_id.to_string(),
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
    session_id: &str,
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
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<(ThreadId, ResumeTicket), HostError> {
        if command.session_id.trim().is_empty()
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
        let commit = self.commit_for_read(&command.session_id).await?;
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
        if snapshot.latest_run_id.as_ref() != Some(&command.expected_run_id)
            || !snapshot.runs.iter().any(|run| {
                run.id == command.expected_run_id && matches!(run.state, RunState::Awaiting)
            })
        {
            return Err(HostError::bad_request("Session Thread has no awaiting Run"));
        }
        let mut tickets = snapshot.resume_tickets.into_iter().filter(|entry| {
            entry.run_id == command.expected_run_id
                && entry.ticket.thread_id == thread_id
                && entry.ticket.correlation_id == command.expected_correlation_id
        });
        let ticket = tickets.next().ok_or_else(|| {
            HostError::bad_request("Session Thread awaiting ticket changed after Event admission")
        })?;
        if tickets.next().is_some() {
            return Err(HostError::internal(
                "Session Thread has multiple matching committed resume tickets",
            ));
        }
        self.check_pending(
            &ticket.ticket,
            &command.tool_use_id,
            command.reply.client_executed(),
        )?;
        Ok((thread_id, ticket.ticket))
    }

    /// Derive the absorbing follow-up fence from the latest committed Run in the
    /// parent Session partition. No Thread status or dispatch flag shadows that
    /// lifecycle truth.
    pub(super) async fn coordinated_thread_has_failed_run(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<bool, HostError> {
        let commit = self.commit_for_read(session_id).await?;
        Ok(commit
            .latest_run(thread_id)
            .is_some_and(|run| coordinated_thread_failed(&run.state)))
    }

    /// Read the exact parent-partition Awaiting coordinate before the Session
    /// aggregate opens a reply activity. Delivery revalidates this fence, which
    /// closes call-id reuse and concurrent-resume races without another store.
    pub(crate) async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, HostError> {
        let (thread_id, _ticket) = match self.validated_session_thread_tool_reply(command).await {
            Ok(active) => active,
            Err(active_error) => match self.session_thread_tool_reply_receipt(command).await? {
                SessionReplyReceipt::Exact => {
                    return Ok(awaken_session_contract::SessionThreadToolReplyFence {
                        prior_session_activity_epoch: None,
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
        let parent = ThreadId(command.session_id.clone());
        let dispatch = self
            .dispatch_store()?
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let prior_session_activity_epoch = match dispatch {
            Some(dispatch)
                if dispatch.thread_id == thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent) =>
            {
                dispatch.session_activity_epoch
            }
            Some(_) => {
                return Err(HostError::bad_request(
                    "Session Thread reply dispatch affinity is inconsistent",
                ));
            }
            None if thread_id == parent => None,
            None => {
                return Err(HostError::internal(
                    "Session child Thread reply has no exact durable dispatch row",
                ));
            }
        };
        if prior_session_activity_epoch == Some(0) {
            return Err(HostError::internal(
                "Session Thread reply has an invalid durable activity coordinate",
            ));
        }
        Ok(awaken_session_contract::SessionThreadToolReplyFence {
            prior_session_activity_epoch,
            already_applied: false,
        })
    }

    /// Deliver a typed reply to one logical Session Thread while retaining the
    /// Session as the physical commit/history partition. The dispatch row and
    /// committed ticket are the only routing authorities; no child-named
    /// Session context or store is opened.
    pub(crate) async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), HostError> {
        let awaken_session_contract::SessionThreadToolReplyDelivery {
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
        let parent = ThreadId(command.session_id.clone());
        let dispatch = store
            .list_dispatches()
            .await
            .map_err(|error| HostError::internal(error.to_string()))?
            .into_iter()
            .find(|dispatch| dispatch.run_id == command.expected_run_id);
        let store = match dispatch {
            Some(dispatch)
                if dispatch.thread_id == thread_id
                    && dispatch.session_thread_id.as_ref() == Some(&parent) =>
            {
                store
            }
            Some(_) => {
                return Err(HostError::bad_request(
                    "Session Thread reply dispatch affinity is inconsistent",
                ));
            }
            None if thread_id == parent && fence.prior_session_activity_epoch.is_none() => {
                self.enqueue_foreground_session_resume_dispatch(
                    &command.session_id,
                    &ticket,
                    session_activity_epoch,
                )
                .await?
            }
            None => {
                return Err(HostError::internal(
                    "Session Thread reply lost its exact durable dispatch row",
                ));
            }
        };
        let context_messages = command
            .accompanying_system
            .as_ref()
            .map(|system| crate::session_system_message(&command.session_id, system))
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
                fence.prior_session_activity_epoch,
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

    /// Classify the committed receipt only after the current active ticket fails
    /// validation. A successful resume intentionally deletes that ticket, while
    /// the audit fact lives in the same ThreadCommit and therefore survives
    /// response loss, Worker settlement, process restart, and projection lag.
    async fn session_thread_tool_reply_receipt(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<SessionReplyReceipt, HostError> {
        if command.session_id.trim().is_empty()
            || command.expected_run_id.0.trim().is_empty()
            || command.expected_correlation_id.trim().is_empty()
            || command.tool_use_id.trim().is_empty()
        {
            return Err(HostError::bad_request(
                "Session Thread tool reply is incomplete",
            ));
        }
        let thread_id = command.target.thread_id(&command.session_id);
        let commit = self.commit_for_read(&command.session_id).await?;
        let snapshot = commit
            .recovery_snapshot(&thread_id, &command.expected_run_id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let expected_operation = command.delivery_operation_id();
        let mut receipt = SessionReplyReceipt::Absent;
        for event in snapshot.events.iter().filter(|event| {
            event.run_id == command.expected_run_id
                && event.kind == awaken_agent_contract::audit::kind::Kind::ResumeApplied
        }) {
            let correlation = event
                .payload
                .get("correlation_id")
                .and_then(serde_json::Value::as_str);
            let same_correlation = correlation == Some(command.expected_correlation_id.as_str());
            let same_operation = event
                .payload
                .get("operation_id")
                .and_then(serde_json::Value::as_str)
                == Some(expected_operation.as_str());
            receipt = advance_session_reply_receipt(receipt, same_correlation, same_operation);
            if receipt == SessionReplyReceipt::Exact {
                break;
            }
        }
        Ok(receipt)
    }

    async fn coordinated_child_admission(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        intent: CoordinatedRunIntent,
        max_unarchived_threads: usize,
    ) -> Result<awaken_run_ingress::SessionChildAdmission, HostError> {
        if max_unarchived_threads == 0 {
            return Err(HostError::bad_request(
                "Session child capacity must be greater than zero",
            ));
        }
        let links = self.coordinated_threads(session_id).await?;
        if intent == CoordinatedRunIntent::FollowUp
            && !links.iter().any(|link| {
                link.thread_id == *thread_id
                    && matches!(link.target, CoordinatedThreadTarget::Agent { .. })
            })
        {
            return Err(HostError::bad_request(
                "follow-up target is not an ordinary coordinated Agent Thread",
            ));
        }
        if intent == CoordinatedRunIntent::FollowUp
            && self
                .coordinated_thread_has_failed_run(session_id, thread_id)
                .await?
        {
            return Err(HostError::bad_request(
                "Agent Thread terminated after a failed Run",
            ));
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
            awaken_run_ingress::SessionChildAdmission::new(max_unarchived_threads, archived)
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
            .read(&session_id, |slot| {
                (
                    slot.baseline.clone(),
                    slot.tools.clone(),
                    slot.manifest.clone(),
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
            snapshot = awaken_session_contract::project_effective_agent_publication(
                baseline.model_override.as_ref(),
                &baseline.system_prompt,
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
        let activation_input =
            coordinated_activation_input(&session_id, &thread_id, &run_id, &operation_id, message);
        let origin = DelegationOrigin {
            delegation_id: DelegationId(format!(
                "coordination:{}",
                awaken_session_contract::stable_fingerprint(&(
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
            ThreadId(session_id.clone()),
            Some(resources),
            self.agent_publications.as_deref(),
        )
        .map_err(|error| HostError::bad_request(error.to_string()))?
        .with_session_activity_epoch(session_activity_epoch);
        let admission = self
            .coordinated_child_admission(&session_id, &thread_id, intent, max_unarchived_threads)
            .await?;
        let store = self.dispatch_store()?;

        store
            .enqueue_session_child(request, admission)
            .await
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        // Thread disposition, Run commits, and dispatch admission may use
        // different durable adapters. Re-read both existing admission fences
        // after enqueue: whichever side won records cancellation on this exact
        // dispatch and waits for settlement. No relation flag, terminal cache,
        // or process-local lock is introduced.
        let archived = self
            .session_thread_disposition(&session_id, &thread_id)
            .await?
            == awaken_agent_contract::ThreadDisposition::Archived;
        let failed = intent == CoordinatedRunIntent::FollowUp
            && self
                .coordinated_thread_has_failed_run(&session_id, &thread_id)
                .await?;
        if archived || failed {
            // This admission may already be durable, so it is not a definitive
            // caller rejection until its cancellation has settled. Returning an
            // unavailable classification preserves the Session activity receipt
            // across an ambiguous enqueue/response boundary.
            self.cancel_and_await_coordinated_thread_quiescence(&session_id, &thread_id)
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

    /// Persist one terminal child report through the existing Outbox/Inbox and
    /// atomically admit the deterministic later coordinator Run. Awaiting child
    /// state is projected from that child's committed lifecycle and must never
    /// enter the primary Inbox or sample the coordinator.
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
        let message_id = command.message.id.0.clone();
        let store = self.dispatch_store()?;

        let ctx = self.ctx_for(&command.session_id, None).await?;
        let run_id = RunId(format!(
            "coord-report-run-{}",
            awaken_session_contract::stable_fingerprint(&(
                "managed-agent-report-run-v1",
                command.session_id.as_str(),
                command.source_run_id.0.as_str(),
            ))
        ));
        let activation = RunActivation::new(
            run_id,
            ThreadId(command.session_id.clone()),
            ctx.config.clone(),
            Vec::new(),
        )
        .with_model_ref_override(self.inference_routing.override_for(&command.session_id));
        // The report command itself is durable Session provenance. Do not rely
        // on a process-local prepared slot to rediscover self-affinity after a
        // cold restart; the settlement observer must always route this root
        // continuation back to the same Session application.
        let request = self
            .resolved_dispatch(activation)?
            .for_session(ThreadId(command.session_id.clone()))
            .with_session_activity_epoch(command.session_activity_epoch);
        let input = PendingInput {
            message_id,
            run_id: request.run_id().clone(),
            thread_id: ThreadId(command.session_id.clone()),
            correlation_id: String::new(),
            available_at_ms: None,
            context_messages: Vec::new(),
            result: ResumeResult::Input(extract_text(&command.message.content)),
        };
        store
            .relay_and_enqueue(
                input,
                request,
                awaken_run_ingress::ContinuationAdmission::Root,
            )
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        if let Some(pool) = self.dispatch_pool.get() {
            pool.notify().await;
        }
        Ok(())
    }

    pub(crate) async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<CoordinatedThreadLink>, HostError> {
        let delivered = self
            .session_slots
            .read(session_id, |slot| slot.published_snapshot.clone())
            .flatten();
        // Relationship recovery and terminal teardown must remain available
        // after a publication is revoked. The committed advisor call supplies
        // identity; current publication only enriches its model label. Managed
        // can recover an empty label from the Session's already-frozen profile.
        let advisor_model = self
            .resolve_session_publication(session_id, None, delivered)
            .ok()
            .and_then(|(_, _, publication)| publication)
            .and_then(|snapshot| {
                snapshot
                    .resolved_spec
                    .plugin_config
                    .agent
                    .advisor
                    .map(|advisor| advisor.model)
            });
        let commit = self.commit_for_read(session_id).await?;
        let root_thread = ThreadId(session_id.to_string());
        let Some(claimed_run) = commit
            .authoritative_latest_run(&root_thread)
            .await
            .map_err(HostError::internal)?
        else {
            return Ok(Vec::new());
        };
        // The claim only selects the recovery port's required Run coordinate.
        // Every relationship input below comes from the returned atomic prefix,
        // including a newer root Run if it committed between selection and read.
        let committed = commit
            .recovery_snapshot(&root_thread, &claimed_run.id)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let mut links =
            project_committed_coordination_links(session_id, advisor_model.as_deref(), &committed)?;
        if links.is_empty() {
            return Ok(links);
        }

        // Child Run facts live in their logical Threads and are intentionally
        // absent from the root snapshot. Enrich only from lifecycle facts at or
        // before the snapshot's backend watermark, never from a later prefix.
        let mut cursor = RunLifecycleCursor::default();
        loop {
            let page = commit
                .events_after(cursor, MAX_LIFECYCLE_PAGE)
                .await
                .map_err(|error| HostError::internal(error.to_string()))?;
            let reached_snapshot_fence =
                apply_lifecycle_latest_runs(&mut links, &page.events, committed.store_cursor);
            if reached_snapshot_fence || page.events.is_empty() || page.next_cursor == cursor {
                break;
            }
            cursor = page.next_cursor;
        }
        Ok(links)
    }

    /// Read the complete logical child prefix from the parent Session's sole
    /// physical commit authority. The existing recovery port owns consistency;
    /// this method only selects its claimed Run from committed child truth.
    pub(crate) async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<Option<RunRecoverySnapshot>, HostError> {
        let commit = self.commit_for_read(session_id).await?;
        let Some(latest_run) = commit
            .authoritative_latest_run(thread_id)
            .await
            .map_err(HostError::internal)?
        else {
            return Ok(None);
        };
        commit
            .recovery_snapshot(thread_id, &latest_run.id)
            .await
            .map(Some)
            .map_err(|error| {
                HostError::internal(format!(
                    "recover Session Thread {:?} from parent partition: {error}",
                    thread_id.0
                ))
            })
    }

    /// Read the exact claim-selected logical Run through its parent Session's
    /// physical commit partition. Registered-Worker recovery already holds the
    /// guarded [`RunDispatch`](awaken_run_ingress::RunDispatch), so it must not
    /// reopen a logical child as a second physical Session.
    pub(crate) async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
        run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, HostError> {
        self.commit_for_read(session_id)
            .await?
            .recovery_snapshot(thread_id, run_id)
            .await
            .map_err(|error| {
                HostError::internal(format!(
                    "recover Session Thread {:?} Run {:?} from parent partition: {error}",
                    thread_id.0, run_id.0
                ))
            })
    }

    pub(crate) async fn session_thread_usage(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_session_contract::SessionUsage, HostError> {
        let commit = self.commit_for_read(session_id).await?;
        Ok(
            awaken_runtime_contract::llm::ThreadUsage::from_committed_state(
                &commit.committed_state(thread_id),
            )
            .into(),
        )
    }

    pub(crate) async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<awaken_agent_contract::ThreadDisposition, HostError> {
        let commit = self.commit_for_read(session_id).await?;
        awaken_agent_contract::thread_disposition_from_committed_state(
            &commit.committed_state(thread_id),
        )
        .map_err(|error| {
            HostError::internal(format!(
                "recover disposition for Session Thread {:?}: {error}",
                thread_id.0
            ))
        })
    }

    async fn coordinated_thread_dispatches(
        &self,
        session_id: &str,
        thread_id: &ThreadId,
    ) -> Result<Vec<awaken_run_ingress::DispatchSummary>, HostError> {
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
            let _ = wake.publish().await;
        }

        const SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
        tokio::time::timeout(SETTLE_TIMEOUT, async {
            loop {
                let active = self
                    .coordinated_thread_dispatches(session_id, thread_id)
                    .await?
                    .into_iter()
                    .any(|dispatch| {
                        matches!(
                            dispatch.state,
                            awaken_run_ingress::DispatchState::Reserved
                                | awaken_run_ingress::DispatchState::ReservationLeased
                                | awaken_run_ingress::DispatchState::Pending
                                | awaken_run_ingress::DispatchState::Leased
                                | awaken_run_ingress::DispatchState::Awaiting
                        )
                    });
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
                    awaken_run_ingress::DispatchState::Reserved
                        | awaken_run_ingress::DispatchState::ReservationLeased
                        | awaken_run_ingress::DispatchState::Pending
                        | awaken_run_ingress::DispatchState::Leased
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
                .any(|dispatch| dispatch.state == awaken_run_ingress::DispatchState::Awaiting)
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
            .latest_run(thread_id)
            .is_some_and(|run| run.state == RunState::Running)
        {
            return Err(HostError::bad_request(
                "only an idle Agent Thread may be archived",
            ));
        }
        let archive_run = RunId(format!(
            "thread-archive-{}",
            awaken_session_contract::stable_fingerprint(&(
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
    use awaken_agent_contract::agent::state::{Command as StateCommand, MergePolicy};
    use awaken_agent_contract::thread::commit::RunDisposition;
    use awaken_agent_contract::thread::commit::coordinator::Coordinator as _;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::tool::{ToolOutput, ToolRecoveryPolicy};
    use awaken_session_contract::SessionRuntime as _;

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
            session_id,
            &first_run,
            0,
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            session_id,
            &first_run,
            0,
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        assert_eq!(links.len(), 1, "I1/E1");

        apply_advisor_call(
            session_id,
            &first_run,
            0,
            "advisor-call-2",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            session_id,
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
                session_id,
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

    #[tokio::test]
    async fn child_recovery_snapshot_reuses_the_parent_commit_consistency_boundary() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // Cause/effect graph: C1 the logical child has/has-not a committed Run;
        // C2 child and unrelated logical commits share the parent physical
        // partition; C3 the latest child Run is Awaiting with transcript, state,
        // and a resume ticket. Effects: E1 no Run returns None without creating
        // child storage; E2 one recovery read returns a mutually consistent child
        // prefix; E3 the snapshot retains the backend-wide cursor while excluding
        // unrelated logical facts; E4 ManagedHost delegates to this same read;
        // E5 the registered-Worker recovery adapter preserves the guarded parent
        // partition while retaining the child's logical identity; C4 the caller
        // supplies an exact present/absent Run id. E6 exact recovery reuses the
        // same prefix for the present Run and fails closed for the absent Run.
        //
        // | Rule | C1 | C2 | C3 | Effects       |
        // | R1   | N  | -  | -  | E1            |
        // | R2   | Y  | Y  | Y  | E2,E3,E4,E5,E6 |
        let session_id = "snapshot-parent-session";
        let child_id = ThreadId("snapshot-logical-child".into());
        let child_run = RunId("snapshot-child-run".into());
        let host = Arc::new(SharedHost::new(
            Arc::new(crate::host::MemoryHostModel),
            "stub",
        ));

        assert_eq!(
            host.session_thread_recovery_snapshot(session_id, &child_id)
                .await
                .expect("R1 parent-partition read"),
            None,
            "R1/E1"
        );
        assert!(
            !host
                .authority
                .as_ref()
                .expect("test Runtime authority")
                .durable_thread_exists(&child_id.0)
                .await
                .expect("R1 child partition probe"),
            "R1/E1 does not open a child-named physical partition"
        );

        let parent_commit = host
            .commit_for_read(session_id)
            .await
            .expect("parent physical commit authority");
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::running(child_run.clone()),
                true,
                vec![Message::text(
                    MessageId("snapshot-child-user".into()),
                    Role::User,
                    "child question",
                )],
                vec![StateCommand::set(
                    Scope::Thread,
                    MergePolicy::Disjoint,
                    "snapshot-phase",
                    serde_json::json!("running"),
                )],
                Vec::new(),
            ))
            .await
            .expect("R2 first child commit");
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                ThreadId("snapshot-unrelated-logical-thread".into()),
                RunDisposition::running(RunId("snapshot-unrelated-run".into())),
                true,
                vec![Message::text(
                    MessageId("snapshot-unrelated-message".into()),
                    Role::User,
                    "must stay outside the child snapshot",
                )],
                Vec::new(),
                Vec::new(),
            ))
            .await
            .expect("R2 unrelated logical commit");
        let ticket = ResumeTicket::new(
            "snapshot-correlation",
            child_run.clone(),
            child_id.clone(),
            "snapshot-id",
            "snapshot-catalog",
            AwaitTarget::RemoteInput {
                reason: awaken_agent_contract::agent::awaiting::RemoteInputReason::UserInput,
                call_id: "snapshot-call".into(),
            },
        );
        parent_commit
            .commit(awaken_agent_contract::ThreadCommit::assemble(
                child_id.clone(),
                RunDisposition::awaiting(ticket.clone()),
                true,
                vec![Message::text(
                    MessageId("snapshot-child-assistant".into()),
                    Role::Assistant,
                    "child needs input",
                )],
                vec![StateCommand::set(
                    Scope::Thread,
                    MergePolicy::Disjoint,
                    "snapshot-phase",
                    serde_json::json!("awaiting"),
                )],
                Vec::new(),
            ))
            .await
            .expect("R2 awaiting child commit");

        let managed = crate::ManagedHost::new(host.clone());
        let snapshot = managed
            .session_thread_recovery_snapshot(session_id, &child_id.0)
            .await
            .expect("R2 ManagedHost read")
            .expect("R2 committed child Run");
        assert_eq!(snapshot.thread_id, child_id, "R2/E2 logical Thread");
        assert_eq!(snapshot.claimed_run_id, child_run, "R2/E2 claimed Run");
        assert_eq!(
            snapshot.latest_run_id,
            Some(child_run.clone()),
            "R2/E2 latest"
        );
        assert_eq!(snapshot.runs.len(), 1, "R2/E2 one child Run");
        assert_eq!(snapshot.runs[0].state, RunState::Awaiting, "R2/E2 state");
        assert_eq!(
            snapshot
                .messages
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            ["child question", "child needs input"],
            "R2/E2 child transcript is one consistent prefix"
        );
        assert_eq!(snapshot.state.len(), 2, "R2/E2 both child state commands");
        assert_eq!(snapshot.resume_tickets.len(), 1, "R2/E2 active ticket");
        assert_eq!(
            snapshot.resume_tickets[0].run_id, child_run,
            "R2/E2 ticket owner"
        );
        assert_eq!(snapshot.resume_tickets[0].ticket, ticket, "R2/E2 ticket");
        assert_eq!(snapshot.thread_version, 2, "R2/E3 child-only version");
        assert_eq!(snapshot.store_cursor, 3, "R2/E3 parent-store cursor");
        assert_eq!(snapshot.next_commit_ordinal, 2, "R2/E2 claimed ordinal");
        let exact_snapshot = managed
            .session_thread_run_recovery_snapshot(session_id, &child_id.0, &child_run)
            .await
            .expect("R2 ManagedHost exact read")
            .expect("R2 exact committed child Run");
        assert_eq!(exact_snapshot, snapshot, "R2/E6 exact recovery authority");
        assert_eq!(
            managed
                .session_thread_run_recovery_snapshot(
                    session_id,
                    &child_id.0,
                    &RunId("snapshot-absent-run".into()),
                )
                .await
                .expect("R2 absent exact read"),
            None,
            "R2/E6 absent exact Run fails closed"
        );
        let worker_snapshot = host
            .worker_recovery_source()
            .recovery_snapshot_in_session(&ThreadId(session_id.into()), &child_id, &child_run)
            .await
            .expect("R2 registered-Worker parent-partition read");
        assert_eq!(worker_snapshot, snapshot, "R2/E5 one recovery authority");
        assert!(
            !host
                .authority
                .as_ref()
                .expect("test Runtime authority")
                .durable_thread_exists(&child_id.0)
                .await
                .expect("R2 child partition probe"),
            "R2/E4-E5 child truth remains physically in the parent partition"
        );
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
        let published_child = awaken_session_contract::coordinated_thread_id(
            session,
            &run_id,
            &ToolBatch::operation_id_for_step(&run_id, step, published_call),
        );
        let gap_child = awaken_session_contract::coordinated_thread_id(
            session,
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

        let mut warm =
            project_committed_coordination_links(session, Some("advisor-model"), &snapshot)
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
        let without_batch =
            project_committed_coordination_links(session, Some("advisor-model"), &without_batch)
                .expect("P2 snapshot without the publication-gap authority");
        assert_eq!(without_batch.len(), 2, "P2/E1,E2,E3");
        assert!(
            without_batch.iter().all(|link| link.thread_id != gap_child),
            "P2/E2 unpublished link is not invented"
        );

        let cold_snapshot: RunRecoverySnapshot =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        let mut cold =
            project_committed_coordination_links(session, Some("advisor-model"), &cold_snapshot)
                .expect("P4 cold recovery projection");
        assert!(apply_lifecycle_latest_runs(
            &mut cold,
            &lifecycle,
            cold_snapshot.store_cursor
        ));
        assert_eq!(cold, warm, "P4/E5 warm and cold committed truth agree");
    }
}
