//! Committed tool invocation identity shared by Runtime and its read projections.

use super::*;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

impl ActiveToolBatch {
    /// Decode each retained write of the one Run-scoped batch cell. Callers
    /// share its validation rather than adding another state or batch codec.
    pub fn recorded_batches<'a>(
        state: &'a [StateCommand],
        run_id: &'a RunId,
    ) -> impl Iterator<Item = Result<(usize, ToolBatch), ToolBatchError>> + 'a {
        state
            .iter()
            .enumerate()
            .filter(move |(_, command)| {
                command.scope == Self::SCOPE
                    && command.key.0 == Self::KEY
                    && command.run_id.as_ref() == Some(run_id)
            })
            .map(move |(position, command)| {
                if command.merge != Self::MERGE {
                    return Err(ToolBatchError::InvalidPersistedState(
                        "tool execution history has an incompatible merge policy".into(),
                    ));
                }
                let mut one = Store::new();
                one.apply(command);
                let batch = Self::load(&one)
                    .map_err(|error| ToolBatchError::InvalidPersistedState(error.to_string()))?
                    .ok_or_else(|| {
                        ToolBatchError::InvalidPersistedState(
                            "tool execution history was removed".into(),
                        )
                    })?;
                if batch.run_id() != run_id {
                    return Err(ToolBatchError::InvalidPersistedState(
                        "tool history belongs to another Run".into(),
                    ));
                }
                Ok((position, batch))
            })
    }

    /// A retained batch at the assistant's commit owns the executed operation.
    /// This also reads older resumed executions whose message coordinate advanced
    /// while their batch restarted at zero. No receipt or history is rewritten.
    /// A snapshot without retained batch evidence keeps the existing transcript
    /// coordinate contract; exact retained but conflicting evidence fails closed.
    pub fn operation_for_message(
        snapshot: &RunRecoverySnapshot,
        message: &Message,
        call_id: &str,
    ) -> Result<Option<String>, ToolBatchError> {
        if message.role != Role::Assistant {
            return Ok(None);
        }
        let Some((owner, step, None)) = message.id.assistant_coordinates() else {
            return Ok(None);
        };
        let Some((name, input)) = message.content.iter().find_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } if id == call_id => Some((name, input)),
            _ => None,
        }) else {
            return Ok(None);
        };
        let run_id = RunId(owner.to_owned());
        let aligned = snapshot.messages.len() == snapshot.message_commit_cursors.len()
            && snapshot.state.len() == snapshot.state_commit_cursors.len();
        let source_cursor = aligned
            .then(|| {
                snapshot
                    .messages
                    .iter()
                    .position(|candidate| candidate.id == message.id)
                    .map(|position| snapshot.message_commit_cursors[position])
            })
            .flatten();
        let mut selected: Option<String> = None;
        for recorded in Self::recorded_batches(&snapshot.state, &run_id) {
            let (position, batch) = recorded?;
            if let Some(cursor) = source_cursor
                && snapshot.state_commit_cursors[position] != cursor
            {
                continue;
            }
            let Some(call) = batch
                .calls()
                .iter()
                .find(|call| call.call.call_id == call_id)
            else {
                continue;
            };
            if call.call.tool_id != *name || call.call.arguments != *input {
                return Err(ToolBatchError::InvalidPersistedState(
                    "committed tool source differs from its batch".into(),
                ));
            }
            let operation = batch.operation_id(call_id);
            if selected.as_ref().is_some_and(|old| old != &operation) {
                return Err(ToolBatchError::InvalidPersistedState(
                    "committed tool source has ambiguous batch identity".into(),
                ));
            }
            selected = Some(operation);
        }
        Ok(Some(selected.unwrap_or_else(|| {
            ToolBatch::operation_id_for_step(&run_id, step, call_id)
        })))
    }
}

/// Rebuild one Run's execution view from committed Thread state. Native and
/// external tool continuation share this filter, including the exact legacy
/// ToolBatch migration; other Runs cannot erase or replace this Run's batch.
pub fn store_from_run_commands(commands: Vec<StateCommand>, run_id: &RunId) -> Store {
    let kept: Vec<StateCommand> = commands
        .into_iter()
        .filter(|command| {
            if command.scope != Scope::Run {
                return true;
            }
            if let Some(owner) = &command.run_id {
                return owner == run_id;
            }
            // Legacy commands predate commit-time Run binding. Only the old
            // ToolBatch cell has an embedded Run id that can be migrated safely;
            // every new Run-scoped key is explicitly owned above.
            if command.key.0 != ActiveToolBatch::KEY {
                return false;
            }
            match &command.action {
                StateAction::Set(value) => {
                    serde_json::from_value::<Option<ToolBatch>>(value.clone())
                        .ok()
                        .flatten()
                        .is_some_and(|batch| batch.run_id() == run_id)
                }
                StateAction::Remove => false,
            }
        })
        .collect();
    Store::rebuild(&kept)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;

    fn source() -> (RunRecoverySnapshot, Message) {
        let run = RunId("resume-run".into());
        let args = serde_json::json!({"agent_id":"writer","message":"report"});
        let message = Message::new(
            MessageId::assistant(&run, 1),
            Role::Assistant,
            vec![ContentBlock::tool_use("send", "send_message", args.clone())],
        );
        let batch = ToolBatch::for_step(
            run.clone(),
            0,
            [(
                ToolCall {
                    call_id: "send".into(),
                    tool_id: "send_message".into(),
                    arguments: args,
                },
                ToolRecoveryPolicy::durable_request(),
            )],
        )
        .unwrap();
        let mut command = ActiveToolBatch::write(&Some(batch));
        command.bind_run(&run);
        let snapshot = RunRecoverySnapshot {
            thread_id: ThreadId("session".into()),
            claimed_run_id: run,
            runs: vec![],
            latest_run_id: None,
            messages: vec![message.clone()],
            message_commit_cursors: vec![7],
            commit_timestamps: Default::default(),
            state: vec![command],
            state_commit_cursors: vec![7],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 7,
            next_commit_ordinal: 2,
        };
        (snapshot, message)
    }

    #[test]
    fn recorded_batch_is_the_one_operation_authority_warm_or_cold() {
        // Decision table: S1 retained batch at exact Message commit + historical
        // resumed mismatch => use that batch's executed operation; S2 cold serde
        // rebuild => identical; S3 no retained batch evidence => original transcript
        // coordinate contract; S4 unknown call/truncated/non-assistant => None.
        // Never rewrite the accepted receipt or infer a second child identity.
        let (snapshot, message) = source();
        let expected = ToolBatch::operation_id_for_step(&RunId("resume-run".into()), 0, "send");
        assert_eq!(
            ActiveToolBatch::operation_for_message(&snapshot, &message, "send").unwrap(),
            Some(expected.clone()),
            "S1"
        );
        let cold: RunRecoverySnapshot =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(
            ActiveToolBatch::operation_for_message(&cold, &cold.messages[0], "send").unwrap(),
            Some(expected),
            "S2"
        );
        let mut omitted = snapshot.clone();
        omitted.state.clear();
        omitted.state_commit_cursors.clear();
        assert_eq!(
            ActiveToolBatch::operation_for_message(&omitted, &message, "send").unwrap(),
            Some(ToolBatch::operation_id_for_step(
                &RunId("resume-run".into()),
                1,
                "send"
            )),
            "S3"
        );
        assert_eq!(
            ActiveToolBatch::operation_for_message(&snapshot, &message, "absent").unwrap(),
            None,
            "S4"
        );
        let mut partial = message.clone();
        partial.id = MessageId::assistant_truncated(&RunId("resume-run".into()), 1, 1);
        assert_eq!(
            ActiveToolBatch::operation_for_message(&snapshot, &partial, "send").unwrap(),
            None,
            "S4"
        );
        let mut user = message;
        user.role = Role::User;
        assert_eq!(
            ActiveToolBatch::operation_for_message(&snapshot, &user, "send").unwrap(),
            None,
            "S4"
        );
    }

    #[test]
    fn conflicting_source_or_batch_identity_fails_closed() {
        // S5 same commit/call but changed arguments or tool => error, never fall
        // back to a guessed operation; S6 two distinct recorded operations for
        // the same co-committed call => ambiguity error; S7 foreign Run writes
        // are outside the source and cannot override its identity.
        let (snapshot, message) = source();
        for change_tool in [false, true] {
            let mut altered = message.clone();
            if let ContentBlock::ToolUse { name, input, .. } = &mut altered.content[0] {
                if change_tool {
                    *name = "other".into();
                } else {
                    *input = serde_json::json!({"agent_id":"wrong"});
                }
            }
            assert!(
                ActiveToolBatch::operation_for_message(&snapshot, &altered, "send").is_err(),
                "S5"
            );
        }
        let mut conflicting = snapshot.clone();
        let batch = ToolBatch::for_step(
            RunId("resume-run".into()),
            2,
            [(
                ToolCall {
                    call_id: "send".into(),
                    tool_id: "send_message".into(),
                    arguments: serde_json::json!({"agent_id":"writer","message":"report"}),
                },
                ToolRecoveryPolicy::durable_request(),
            )],
        )
        .unwrap();
        let mut command = ActiveToolBatch::write(&Some(batch));
        command.bind_run(&RunId("resume-run".into()));
        conflicting.state.push(command);
        conflicting.state_commit_cursors.push(7);
        assert!(
            ActiveToolBatch::operation_for_message(&conflicting, &message, "send").is_err(),
            "S6"
        );
        let mut foreign = snapshot.clone();
        foreign.state[0].run_id = Some(RunId("foreign".into()));
        assert_eq!(
            ActiveToolBatch::operation_for_message(&foreign, &message, "send").unwrap(),
            Some(ToolBatch::operation_id_for_step(
                &RunId("resume-run".into()),
                1,
                "send"
            )),
            "S7"
        );
    }
}
