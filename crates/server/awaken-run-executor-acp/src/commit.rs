// @formal surface requirement=protocol.acp.acp_normalization
// @formal reason: ACP projects messages, ToolBatch, permission ticket and terminal outcome through one
// @formal reason: ThreadCommit. Only Runtime-owned tickets create a Runtime batch; unconsumed exact
// @formal reason: permission continuations settle without implying an executed effect was absent.
//! ACP's one ThreadCommit projection: messages, batch, ticket and terminal fact.
//! Transport notifications and one-shot permission grants never commit directly.
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

/// Commit a terminal failure that occurred before/instead of a Step (open fault):
/// the activation input and one assistant message with the failure prompt, plus
/// the terminal state.
pub(super) async fn finish_failure(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    failure: &AcpFailure,
    unconsumed_resume: Option<&PermissionResume>,
) -> Result<RunState> {
    let disposition = RunDisposition::ended(activation.run_id.clone(), failure_cause(failure));
    let state = disposition.state();
    let mut messages = activation.input.clone();
    let mut next_assistant_step = assistant_step_base(context, activation);
    push_assistant_text(
        &mut messages,
        &activation.run_id,
        &mut next_assistant_step,
        failure.prompt(),
    );
    commit(
        context,
        Vec::new(),
        unconsumed_resume,
        &activation.thread_id,
        disposition,
        messages,
        Vec::new(),
    )
    .await?;
    Ok(state)
}

/// Commit the Step's messages and disposition through the one boundary (G13).
pub(super) async fn commit(
    context: &RuntimeRunContext,
    audit: Vec<awaken_agent_contract::audit::draft::Draft>,
    unconsumed_resume: Option<&PermissionResume>,
    thread_id: &ThreadId,
    disposition: RunDisposition,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<()> {
    commit_with_requested_tool(
        context,
        audit,
        unconsumed_resume,
        thread_id,
        disposition,
        messages,
        state,
        None,
    )
    .await
}

/// Commit the automatic MCP handoff before Runtime may enter its executor.
#[allow(clippy::too_many_arguments)]
pub(super) async fn commit_requested_tool(
    context: &RuntimeRunContext,
    audit: Vec<awaken_agent_contract::audit::draft::Draft>,
    unconsumed_resume: Option<&PermissionResume>,
    thread_id: &ThreadId,
    run_id: RunId,
    call: ToolCall,
    messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<()> {
    commit_with_requested_tool(
        context,
        audit,
        unconsumed_resume,
        thread_id,
        RunDisposition::running(run_id),
        messages,
        state,
        Some(call),
    )
    .await
}

fn requested_batch(
    run_id: &RunId,
    messages: &[Message],
    call: ToolCall,
) -> Result<awaken_tool_contract::ToolBatch> {
    // Results must point to the actual ToolUse occurrence, not the next unused
    // Assistant step. Managed transcript attribution uses this exact coordinate.
    let source = messages
        .iter()
        .rev()
        .find(|message| {
            message.role == Role::Assistant
                && message.content.iter().any(|block| {
                    matches!(block,
            ContentBlock::ToolUse { id, .. } if id == &call.call_id)
                })
        })
        .ok_or_else(|| Error::Commit("tool handoff has no Assistant ToolUse".into()))?;
    if !source.content.iter().any(|block| matches!(block,
        ContentBlock::ToolUse { id, name, input }
            if id == &call.call_id && canonical_permission_tool_id(name) == call.tool_id && input == &call.arguments)) {
        return Err(Error::Commit("tool handoff differs from its Assistant ToolUse".into()));
    }
    let step = source.id.assistant_step_of(run_id).ok_or_else(|| {
        Error::Commit("tool handoff requires its current Run Assistant coordinate".into())
    })?;
    awaken_tool_contract::ToolBatch::for_step(
        run_id.clone(),
        step,
        [(
            call,
            awaken_tool_contract::tool::ToolRecoveryPolicy::default(),
        )],
    )
    .map_err(|error| Error::Commit(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
async fn commit_with_requested_tool(
    context: &RuntimeRunContext,
    audit: Vec<awaken_agent_contract::audit::draft::Draft>,
    unconsumed_resume: Option<&PermissionResume>,
    thread_id: &ThreadId,
    disposition: RunDisposition,
    mut messages: Vec<Message>,
    state: Vec<StateCommand>,
    requested: Option<ToolCall>,
) -> Result<()> {
    // A final/new-Await commit consumes the prior permission continuation. If
    // that exact decision never granted execution, settle its original ToolUse
    // here, atomically with ticket replacement/removal. Do not infer non-execution
    // from a missing result after a grant was consumed: that outcome is unknown.
    if let Some(pending) = unconsumed_resume
        && !messages.iter().flat_map(|message| &message.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &pending.call_id)
        })
    {
        let reason = if pending.allow && context.tool_capability_narrowing.disabled_reason().is_some() {
            "Tool request was not executed: tools are disabled for this Run. An earlier approval cannot restore removed authority."
        } else if pending.allow && context.tool_permission_policy.is_some() {
            "Tool request was not executed before this permission continuation ended. Earlier approval does not override current Run restrictions or approve a changed request."
        } else if pending.allow {
            "Tool request was not executed: the resumed agent did not reissue the approved tool with its exact arguments before this permission continuation ended. A changed request requires its own approval."
        } else {
            "Tool request was not executed because permission was denied."
        };
        messages.push(Message {
            id: acp_tool_message_id(disposition.run_id(), "permission-unexecuted", &pending.call_id),
            role: Role::Tool,
            content: vec![ContentBlock::tool_result_with_error(
                pending.call_id.clone(), vec![ContentBlock::text(reason)], true,
            )],
        });
    }
    // Only Awaken-owned continuations execute through the Runtime ToolBatch.
    // Opaque ACP permissions retain the explicit RunAttemptExecutor authority;
    // client execution reuses the same batch with an ExternalResult wait.
    let mut state = state;
    use awaken_runtime_contract::permission::RunToolCapability;
    let run_id = disposition.run_id();
    let commands = context
        .reader
        .as_ref()
        .map(|reader| reader.committed_state(thread_id))
        .unwrap_or_default();
    let recorded = RunToolCapability::recorded(&commands, run_id)
        .map_err(|error| Error::Commit(error.to_string()))?;
    let mut store = awaken_tool_contract::tool_batch::store_from_run_commands(commands, run_id);
    RunToolCapability::project_recorded(&mut store, recorded);
    let fresh = context
        .reader
        .as_ref()
        .is_none_or(|reader| reader.run(run_id).is_none());
    RunToolCapability::stage(
        &mut store,
        &mut state,
        context.tool_capability_narrowing,
        fresh,
    )
    .map_err(|error| Error::Commit(error.to_string()))?;
    if let Some(call) = requested {
        if disposition.state() != RunState::Running {
            return Err(Error::Commit(
                "automatic tool handoff requires Running".into(),
            ));
        }
        let batch = requested_batch(disposition.run_id(), &messages, call)?;
        state.push(awaken_tool_contract::ActiveToolBatch::write(&Some(batch)));
    } else if let Some(ticket) = disposition.resume_ticket()
        && let AwaitTarget::ToolCall {
            reason,
            call_id,
            tool,
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        } = ticket.target()
        && matches!(
            reason,
            ToolAwaitReason::Permission | ToolAwaitReason::ClientExecution
        )
    {
        let mut batch = requested_batch(
            &ticket.run_id,
            &messages,
            ToolCall {
                call_id: call_id.clone(),
                tool_id: tool.tool_id.clone(),
                arguments: tool.arguments.clone(),
            },
        )?;
        batch
            .mark_awaiting(
                call_id,
                match reason {
                    ToolAwaitReason::Permission => {
                        awaken_tool_contract::ToolWaitKind::ToolPermission
                    }
                    ToolAwaitReason::ClientExecution => {
                        awaken_tool_contract::ToolWaitKind::ExternalResult
                    }
                    _ => unreachable!("guarded by the owned wait reason"),
                },
                &ticket.correlation_id,
            )
            .map_err(|error| Error::Commit(error.to_string()))?;
        state.push(awaken_tool_contract::ActiveToolBatch::write(&Some(batch)));
    } else {
        state.push(awaken_tool_contract::ActiveToolBatch::remove());
    }
    let terminal = match disposition.state() {
        RunState::Ended(cause) => Some(CommittedTerminalRun {
            run_id: disposition.run_id().clone(),
            thread_id: thread_id.clone(),
            cause,
        }),
        RunState::Running | RunState::Awaiting => None,
    };
    let coordinator = context
        .commit
        .as_ref()
        .ok_or_else(|| Error::Commit("ACP execution requires a CommitCoordinator".to_string()))?;
    awaken_agent_contract::thread::commit::commit_run_with_events(
        coordinator.as_ref(),
        thread_id,
        disposition,
        messages,
        state,
        audit,
    )
    .await
    .map_err(|e| Error::Commit(e.to_string()))?;

    if let Some(terminal) = &terminal {
        // Observation is post-commit and failure-isolated by the shared
        // runtime-contract helper. Stable-id recovery may redeliver.
        let _ = deliver_committed_terminal(&context.terminal_observers, terminal).await;
    }
    Ok(())
}

#[cfg(test)]
mod requested_tests {
    use super::*;

    #[tokio::test]
    async fn automatic_handoff_commits_requested_without_a_permission_ticket() {
        // C1 automatic exported arrival; C2 durable commit available/missing;
        // C3 distinct canonical Assistant steps with a reused wire id; C4 a
        // later unrelated placeholder. R4 the batch uses the exact originating
        // Assistant step, never the next step: Managed occurrence attribution
        // must pair its result with that ToolUse despite concurrent updates.
        // R5 missing, foreign-Run, User-authored or argument-drifted sources
        // refuse before commit; no caller/model-authored id supplies authority.
        // R1 C1+C2 -> Running + Requested batch + exact ToolUse, no user ask;
        // R2 !C2 -> reject, no dispatch; R3 C3 -> distinct Runtime operation ids.
        // Execution/recovery is verified by the Host's token-requiring integration
        // test; this fixture tests the producer's durable handoff boundary.
        use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
        let storage = Arc::new(awaken_store_inmem::MemoryCommitCoordinator::new(
            awaken_time::system_clock(),
        ));
        let context = RuntimeRunContext::new()
            .with_commit(storage.clone())
            .with_reader(storage.clone());
        let run = RunId("automatic-run".into());
        let thread = ThreadId("automatic-thread".into());
        let call = ToolCall {
            call_id: "wire-reset".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"file_path":"/workspace/SKILL.md"}),
        };
        let mut operations = Vec::new();
        for step in [0, 3] {
            commit_requested_tool(
                &context,
                Vec::new(),
                None,
                &thread,
                run.clone(),
                call.clone(),
                vec![
                    Message::new(
                        MessageId::assistant(&run, step),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            &call.call_id,
                            &call.tool_id,
                            call.arguments.clone(),
                        )],
                    ),
                    Message::new(
                        MessageId::assistant(&run, step + 1),
                        Role::Assistant,
                        vec![ContentBlock::tool_use(
                            "not-arrived",
                            "read",
                            serde_json::json!({}),
                        )],
                    ),
                ],
                Vec::new(),
            )
            .await
            .unwrap();
            let store = awaken_tool_contract::tool_batch::store_from_run_commands(
                storage.committed_state(&thread),
                &run,
            );
            let batch = awaken_tool_contract::ActiveToolBatch::load(&store)
                .unwrap()
                .unwrap();
            assert_eq!(
                batch.phase(),
                awaken_tool_contract::tool_batch::ToolBatchPhase::Open,
                "R1"
            );
            assert_eq!(
                batch.calls()[0].phase,
                awaken_tool_contract::tool_batch::ToolCallPhase::Requested,
                "R1 call is committed before execution"
            );
            assert_eq!(storage.run_state(&run), Some(RunState::Running));
            assert!(
                storage.resume_ticket(&run).is_none(),
                "no manufactured approval"
            );
            let expected = awaken_tool_contract::ToolBatch::for_step(
                run.clone(),
                step,
                [(
                    call.clone(),
                    awaken_tool_contract::tool::ToolRecoveryPolicy::default(),
                )],
            )
            .unwrap();
            assert_eq!(
                batch.operation_id(&call.call_id),
                expected.operation_id(&call.call_id),
                "R4 originating occurrence"
            );
            operations.push(batch.operation_id(&call.call_id));
        }
        assert_ne!(
            operations[0], operations[1],
            "R3 canonical step namespaces the wire id"
        );
        let before = storage.committed_messages(&thread);
        let source = |owner: &RunId, role, arguments| {
            Message::new(
                MessageId::assistant(owner, 9),
                role,
                vec![ContentBlock::tool_use(
                    &call.call_id,
                    &call.tool_id,
                    arguments,
                )],
            )
        };
        for invalid in [
            Vec::new(),
            vec![source(
                &RunId("foreign".into()),
                Role::Assistant,
                call.arguments.clone(),
            )],
            vec![source(&run, Role::User, call.arguments.clone())],
            vec![source(
                &run,
                Role::Assistant,
                serde_json::json!({"file_path":"changed"}),
            )],
        ] {
            assert!(
                commit_requested_tool(
                    &context,
                    Vec::new(),
                    None,
                    &thread,
                    run.clone(),
                    call.clone(),
                    invalid,
                    Vec::new()
                )
                .await
                .is_err(),
                "R5"
            );
            assert_eq!(storage.committed_messages(&thread), before, "R5 no commit");
        }
        assert!(
            commit_requested_tool(
                &RuntimeRunContext::new(),
                Vec::new(),
                None,
                &thread,
                run,
                call,
                Vec::new(),
                Vec::new()
            )
            .await
            .is_err(),
            "R2"
        );
    }
}
