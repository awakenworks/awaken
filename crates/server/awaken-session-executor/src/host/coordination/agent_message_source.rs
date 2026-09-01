//! Refinement from one committed model tool call to a Session command.

use super::*;
use awaken_ext_builtin_tools::{AgentMessageTarget, SendMessageArgs};
use awaken_runtime_contract::tool_batch::ToolCallPhase;
use awaken_session_contract::SessionAgentTarget;

pub(super) fn validate(
    snapshot: &RunRecoverySnapshot,
    command: &SessionAgentMessageCommand,
) -> Result<(), HostError> {
    if snapshot.thread_id != command.source_thread_id
        || snapshot.claimed_run_id != command.source_run_id
    {
        return Err(HostError::bad_request(
            "Agent message source does not match its committed Thread prefix",
        ));
    }
    let state = Store::rebuild(&snapshot.state);
    let batch = awaken_runtime_contract::ActiveToolBatch::load(&state)
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
    let target = args.normalized_target().map_err(HostError::bad_request)?;
    let target_matches = match (&target, &command.target) {
        (
            AgentMessageTarget::Spawn { agent_id },
            SessionAgentTarget::Spawn { agent_id: expected },
        ) => agent_id == expected,
        (
            AgentMessageTarget::ExistingThread { session_thread_id },
            SessionAgentTarget::ExistingThread { thread_id },
        ) => session_thread_id == &thread_id.0,
        _ => false,
    };
    if !target_matches || args.message != command.message {
        return Err(HostError::bad_request(
            "Agent message target or payload differs from its committed tool call",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_runtime_contract::tool::ToolRecoveryPolicy;
    use awaken_runtime_contract::tool_batch::ToolBatch;

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
            let mut state = awaken_runtime_contract::ActiveToolBatch::write(&Some(batch));
            state.bind_run(&run);
            RunRecoverySnapshot {
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
            session_id: thread.0.clone(),
            source_thread_id: thread.clone(),
            source_run_id: run.clone(),
            source_call_id: call_id.into(),
            operation_id: ToolBatch::operation_id_for_step(&run, 2, call_id),
            target: SessionAgentTarget::Spawn {
                agent_id: "researcher".into(),
            },
            message: "investigate".into(),
        };

        assert!(
            validate(
                &snapshot(awaken_ext_builtin_tools::SEND_MESSAGE, true),
                &exact,
            )
            .is_ok(),
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
