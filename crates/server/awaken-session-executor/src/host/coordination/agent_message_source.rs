// @formal entrypoint session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
// @formal entrypoint session.agent_message_requires_exact_committed_tool_source_and_payload
//! Refinement from one committed model tool call to a Session command.

use super::*;
use awaken_ext_builtin_tools::{AgentMessageTarget, SendMessageArgs};
use awaken_session_execution_contract::SessionAgentTarget;
use awaken_tool_contract::tool_batch::ToolCallPhase;

pub(super) fn validate(
    snapshot: &RunRecoverySnapshot,
    command: &SessionAgentMessageCommand,
) -> Result<SessionAgentMessageCommand, HostError> {
    if snapshot.thread_id != command.source_thread_id
        || snapshot.claimed_run_id != command.source_run_id
    {
        return Err(HostError::bad_request(
            "Agent message source does not match its committed Thread prefix",
        ));
    }
    let state = Store::rebuild(&snapshot.state);
    let batch = awaken_tool_contract::ActiveToolBatch::load(&state)
        .map_err(|error| HostError::internal(error.to_string()))?
        .ok_or_else(|| HostError::bad_request("Agent message source has no active tool batch"))?;
    if batch.run_id() != &command.source_run_id {
        return Err(HostError::bad_request(
            "Agent message source batch belongs to another Run",
        ));
    }
    let call = batch
        .calls()
        .iter()
        .find(|entry| entry.call.call_id == command.source_call_id)
        .ok_or_else(|| {
            HostError::bad_request("Agent message source call is absent from its active tool batch")
        })?;
    if call.call.tool_id != awaken_ext_builtin_tools::SEND_MESSAGE
        || batch.operation_id(&command.source_call_id) != command.operation_id
        || !matches!(call.phase, ToolCallPhase::Executing { .. })
    {
        return Err(HostError::bad_request(
            "Agent message source does not match an executing send_message operation",
        ));
    }
    let args: SendMessageArgs = serde_json::from_value(call.call.arguments.clone())
        .map_err(|_| HostError::bad_request("Agent message source arguments are invalid"))?;
    let target = match args.normalized_target().map_err(HostError::bad_request)? {
        AgentMessageTarget::Spawn { agent_id } => SessionAgentTarget::Spawn { agent_id },
        AgentMessageTarget::ExistingThread { session_thread_id } => {
            SessionAgentTarget::ExistingThread {
                thread_id: ThreadId(session_thread_id),
            }
        }
    };
    let committed = SessionAgentMessageCommand {
        session_id: awaken_session_types::SessionId::from(&snapshot.thread_id.0),
        source_thread_id: snapshot.thread_id.clone(),
        source_run_id: batch.run_id().clone(),
        source_call_id: call.call.call_id.clone(),
        operation_id: batch.operation_id(&call.call.call_id),
        target,
        message: args.message,
    };
    refine_committed_command(committed, command)
}

fn refine_committed_command(
    committed: SessionAgentMessageCommand,
    requested: &SessionAgentMessageCommand,
) -> Result<SessionAgentMessageCommand, HostError> {
    if !agent_message_operation_matches(
        agent_message_operation(&committed),
        agent_message_operation(requested),
    ) {
        return Err(HostError::bad_request(
            "Agent message command differs from its committed tool call",
        ));
    }
    Ok(committed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentMessageTargetRef<'a, T: ?Sized> {
    Spawn(&'a T),
    ExistingThread(&'a T),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AgentMessageOperationRef<'a, T: ?Sized> {
    session_id: &'a T,
    source_thread_id: &'a T,
    source_run_id: &'a T,
    source_call_id: &'a T,
    operation_id: &'a T,
    target: AgentMessageTargetRef<'a, T>,
    message: &'a T,
}

fn agent_message_operation(
    command: &SessionAgentMessageCommand,
) -> AgentMessageOperationRef<'_, str> {
    AgentMessageOperationRef {
        session_id: command.session_id.as_str(),
        source_thread_id: &command.source_thread_id.0,
        source_run_id: &command.source_run_id.0,
        source_call_id: &command.source_call_id,
        operation_id: &command.operation_id,
        target: match &command.target {
            SessionAgentTarget::Spawn { agent_id } => AgentMessageTargetRef::Spawn(agent_id),
            SessionAgentTarget::ExistingThread { thread_id } => {
                AgentMessageTargetRef::ExistingThread(&thread_id.0)
            }
        },
        message: &command.message,
    }
}

fn agent_message_operation_matches<T: Eq + ?Sized>(
    committed: AgentMessageOperationRef<'_, T>,
    requested: AgentMessageOperationRef<'_, T>,
) -> bool {
    committed == requested
}

#[cfg(kani)]
/// @obligation session.agent_message_exact_replay_reuses_one_dispatch_and_payload_conflict_stutters
/// @obligation session.agent_message_requires_exact_committed_tool_source_and_payload
#[kani::proof]
fn session_agent_message_admission_requires_every_committed_field() {
    // Causes: C1 the production extractor has reconstructed one complete typed
    // command from the committed prefix; C2 each of its seven identity/payload
    // fields equals the requested command. Effects: E1 only the complete C1+C2
    // row returns the committed value consumed by Session admission; E2 every
    // single changed field rejects. This calls the production operation-view
    // comparator, not caller-supplied booleans. Store rebuild,
    // ActiveToolBatch/JSON extraction and String equality remain adapter TCB.
    let changed_axis = kani::any::<u8>();
    kani::assume(changed_axis < 8);
    let exact = [0_u8];
    let changed = [1_u8];
    let committed = AgentMessageOperationRef {
        session_id: &exact,
        source_thread_id: &exact,
        source_run_id: &exact,
        source_call_id: &exact,
        operation_id: &exact,
        target: AgentMessageTargetRef::Spawn(&exact),
        message: &exact,
    };
    let requested = AgentMessageOperationRef {
        session_id: if changed_axis == 1 { &changed } else { &exact },
        source_thread_id: if changed_axis == 2 { &changed } else { &exact },
        source_run_id: if changed_axis == 3 { &changed } else { &exact },
        source_call_id: if changed_axis == 4 { &changed } else { &exact },
        operation_id: if changed_axis == 5 { &changed } else { &exact },
        target: if changed_axis == 6 {
            AgentMessageTargetRef::ExistingThread(&exact)
        } else {
            AgentMessageTargetRef::Spawn(&exact)
        },
        message: if changed_axis == 7 { &changed } else { &exact },
    };

    assert_eq!(
        agent_message_operation_matches(committed, requested),
        changed_axis == 0
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_tool_contract::tool::ToolRecoveryPolicy;
    use awaken_tool_contract::tool_batch::ToolBatch;

    #[test]
    fn command_is_refined_from_one_committed_tool_call() {
        // Cause/effect graph: C1 the recovery prefix belongs to the claimed
        // Thread/Run; C2 ActiveToolBatch contains an Executing send_message
        // call; C3 call id and derived operation id match; C4 normalized target
        // and message match byte-for-byte. Effects: E1 C1+C2+C3+C4 accepts;
        // E2 any false cause rejects before Session activity or Dispatch. The
        // test mutates one cause at a time so no accepted result can be explained
        // by a second trusted Worker-command path.
        //
        // | Rule | Prefix | Tool/phase | Identity | Payload | Effect |
        // | V1 | exact | send/Executing | exact | exact | E1 |
        // | V2 | exact | send/Executing | exact | changed | E2 |
        // | V3 | exact | send/Executing | changed | exact | E2 |
        // | V4 | exact | other/Executing | exact | exact | E2 |
        // | V5 | exact | send/Requested | exact | exact | E2 |
        let run = RunId("source-run".into());
        let thread = ThreadId("source-thread".into());
        let call_id = "source-call";
        let args = serde_json::json!({
            "agent_id": "researcher",
            "message": "investigate"
        });
        let snapshot = |tool_id: &str, executing: bool| {
            let mut batch = ToolBatch::for_step(
                run.clone(),
                2,
                [(
                    ToolCall {
                        call_id: call_id.into(),
                        tool_id: tool_id.into(),
                        arguments: args.clone(),
                    },
                    ToolRecoveryPolicy::default(),
                )],
            )
            .unwrap();
            if executing {
                batch.mark_executing(call_id).unwrap();
            }
            let mut state = awaken_tool_contract::ActiveToolBatch::write(&Some(batch));
            state.bind_run(&run);
            RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: thread.clone(),
                claimed_run_id: run.clone(),
                runs: Vec::new(),
                latest_run_id: Some(run.clone()),
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                state: vec![state],
                state_commit_cursors: vec![1],
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 1,
                next_commit_ordinal: 1,
            }
        };
        let exact = SessionAgentMessageCommand {
            session_id: awaken_session_types::SessionId::from(&thread.0),
            source_thread_id: thread.clone(),
            source_run_id: run.clone(),
            source_call_id: call_id.into(),
            operation_id: ToolBatch::operation_id_for_step(&run, 2, call_id),
            target: SessionAgentTarget::Spawn {
                agent_id: "researcher".into(),
            },
            message: "investigate".into(),
        };

        assert_eq!(
            validate(
                &snapshot(awaken_ext_builtin_tools::SEND_MESSAGE, true),
                &exact,
            )
            .expect("V1 committed command"),
            exact,
            "V1/E1"
        );
        let mut changed_payload = exact.clone();
        changed_payload.message = "different".into();
        assert!(
            validate(
                &snapshot(awaken_ext_builtin_tools::SEND_MESSAGE, true),
                &changed_payload,
            )
            .is_err(),
            "V2/E2"
        );
        let mut changed_identity = exact.clone();
        changed_identity.operation_id = "different-operation".into();
        assert!(
            validate(
                &snapshot(awaken_ext_builtin_tools::SEND_MESSAGE, true),
                &changed_identity,
            )
            .is_err(),
            "V3/E2"
        );
        assert!(
            validate(&snapshot("other_tool", true), &exact).is_err(),
            "V4/E2"
        );
        assert!(
            validate(
                &snapshot(awaken_ext_builtin_tools::SEND_MESSAGE, false),
                &exact,
            )
            .is_err(),
            "V5/E2"
        );
    }
}
