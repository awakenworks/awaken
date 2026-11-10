//! Committed tool invocation identity shared by Runtime and its read projections.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Message, Role};
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;

impl ActiveToolBatch {
    /// Provenance and physical-entry reads require a retained batch. Logical
    /// execution recovery may instead consume an explicit clear boundary.
    pub(super) fn require_retained_batch(
        batch: Option<ToolBatch>,
    ) -> Result<ToolBatch, ToolBatchError> {
        batch.ok_or_else(|| {
            ToolBatchError::InvalidPersistedState("tool execution history was removed".into())
        })
    }

    /// Decode each retained write or clear of the one Run-scoped batch cell.
    /// Both Remove and the typed write(None) are legitimate clear boundaries.
    /// Callers share this codec and select their read purpose.
    pub fn recorded_batches<'a>(
        state: &'a [StateCommand],
        run_id: &'a RunId,
    ) -> impl Iterator<Item = Result<(usize, Option<ToolBatch>), ToolBatchError>> + 'a {
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
                    .map_err(|error| ToolBatchError::InvalidPersistedState(error.to_string()))?;
                if batch.as_ref().is_some_and(|batch| batch.run_id() != run_id) {
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
            // A later continuation can clear its active cell without erasing
            // the batch committed with the source Message.
            let batch = Self::require_retained_batch(batch)?;
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
/// An open batch's retained terminal effects enter this execution view at their
/// original command positions, once per recorded value. Applying them after
/// the latest command would let an old Remove erase a later delivery. This is
/// not publication: finalized batches and ordinary committed reads keep their
/// existing top-level state, and the batch still owns its staged effects.
/// This read projection is not execution admission: the Runtime validates the
/// retained batch history before effects. Undecodable history withholds the
/// staged projection here and preserves the original published state.
pub fn store_from_run_commands(commands: Vec<StateCommand>, run_id: &RunId) -> Store {
    let mut kept: Vec<StateCommand> = commands
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
    let published = Store::rebuild(&kept);
    let Some(active) = ActiveToolBatch::load(&published)
        .ok()
        .flatten()
        .filter(|batch| batch.phase() == ToolBatchPhase::Open)
    else {
        // Preserve malformed latest state for the existing typed admission to
        // reject, rather than replacing it with a fabricated empty batch.
        return published;
    };
    // Only the explicitly accepted legacy batch above lacked a Run binding.
    // Stamp this local view through the original binding law, never the log.
    for command in &mut kept {
        command.bind_run(run_id);
    }
    let mut recorded = ActiveToolBatch::recorded_batches(&kept, run_id).peekable();
    let mut effects = std::collections::BTreeMap::<String, Vec<StateCommand>>::new();
    let mut store = Store::new();
    for (position, command) in kept.iter().enumerate() {
        store.apply(command);
        match recorded.peek() {
            Some(Err(_)) => return published,
            Some(Ok((at, _))) if *at == position => {}
            _ => continue,
        }
        let (_, batch) = recorded
            .next()
            .expect("the recorded position matched")
            .expect("the recorded batch was decoded");
        let Some(batch) = batch else {
            effects.clear();
            continue;
        };
        if batch.id() != active.id() {
            continue;
        }
        for entry in batch
            .calls()
            .iter()
            .filter(|entry| entry.phase.is_terminal())
        {
            let previous = effects.entry(entry.call.call_id.clone()).or_default();
            if *previous == entry.result_state {
                continue;
            }
            for effect in &entry.result_state {
                store.apply(effect);
            }
            previous.clone_from(&entry.result_state);
        }
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;
    use awaken_agent_contract::agent::state::Key;
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
    fn source_provenance_survives_a_later_committed_clear() {
        let (snapshot, message) = source();
        let expected = ActiveToolBatch::operation_for_message(&snapshot, &message, "send").unwrap();
        for mut clear in [ActiveToolBatch::remove(), ActiveToolBatch::write(&None)] {
            clear.bind_run(&snapshot.claimed_run_id);
            let mut history = snapshot.clone();
            history.state.push(clear);
            history.state_commit_cursors.push(8);
            assert_eq!(
                ActiveToolBatch::operation_for_message(&history, &message, "send").unwrap(),
                expected
            );

            history.state_commit_cursors[1] = history.message_commit_cursors[0];
            assert!(ActiveToolBatch::operation_for_message(&history, &message, "send").is_err());
        }
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

    #[test]
    fn open_batch_effects_follow_retained_command_order_without_publishing() {
        // Cause/effect graph: C1 owned Run or Thread cell; C2 terminal Set/Remove;
        // C3 absent or later direct Set/Remove; C4 repeated terminal checkpoint
        // after C3; C5 exact explicit/legacy batch ownership. R1=C1+C2+!C3=>
        // terminal effect in execution view; R2=C1+C2+C3+C4=>later direct effect,
        // never replay the old terminal snapshot at C4. Cross all actions,
        // scopes, C3 and C5, plus typed None/Remove clear (64 rows).
        // A foreign Run checkpoint is inert; both clear producers are original
        // StateKey operations, never manufactured closure evidence.
        // E3 raw committed reads never expose nested state; Finalized does not
        // invent publication. E4 malformed latest batch remains a typed error.
        // Expected state is the independently ordered seed/effect/later command
        // sequence; no second reducer or fabricated recovery receipt is used.
        let (snapshot, _) = source();
        let run = snapshot.claimed_run_id;
        for scope in [Scope::Run, Scope::Thread] {
            for terminal_remove in [false, true] {
                for later_remove in [false, true] {
                    for has_later in [false, true] {
                        for legacy in [false, true] {
                            for typed_none in [false, true] {
                                let key = "ordered-effect";
                                let mut seed =
                                    StateCommand::set(scope, MergePolicy::Disjoint, key, 1.into());
                                seed.bind_run(&run);
                                let effect = if terminal_remove {
                                    StateCommand::remove(scope, MergePolicy::Disjoint, key)
                                } else {
                                    StateCommand::set(scope, MergePolicy::Disjoint, key, 2.into())
                                };
                                let mut later = if later_remove {
                                    StateCommand::remove(scope, MergePolicy::Disjoint, key)
                                } else {
                                    StateCommand::set(scope, MergePolicy::Disjoint, key, 3.into())
                                };
                                later.bind_run(&run);
                                let write = |batch: &ToolBatch| {
                                    let mut command = ActiveToolBatch::write(&Some(batch.clone()));
                                    if !legacy {
                                        command.bind_run(&run);
                                    }
                                    command
                                };
                                let mut batch =
                                    ActiveToolBatch::load(&Store::rebuild(&snapshot.state))
                                        .unwrap()
                                        .unwrap();
                                let mut cleared = if typed_none {
                                    ActiveToolBatch::write(&None)
                                } else {
                                    ActiveToolBatch::remove()
                                };
                                cleared.bind_run(&run);
                                let mut history = vec![seed.clone(), cleared, write(&batch)];
                                batch.mark_executing("send").unwrap();
                                batch.complete(ToolOutput::ok("send", "done")).unwrap();
                                batch
                                    .set_result_state("send", vec![effect.clone()])
                                    .unwrap();
                                history.push(write(&batch));
                                let mut expected = vec![seed, effect];
                                if has_later {
                                    history.push(later.clone());
                                    expected.push(later);
                                }
                                history.push(write(&batch));
                                let mut foreign = write(&batch);
                                foreign.run_id = Some(RunId("foreign".into()));
                                history.push(foreign);
                                let raw = Store::rebuild(&history);
                                assert!(
                                    ActiveToolBatch::recorded_execution_domains(&history, &run)
                                        .is_err(),
                                    "a clear is not physical-stop evidence"
                                );
                                let warm = store_from_run_commands(history.clone(), &run);
                                assert_eq!(
                                    warm.get(scope, &Key(key.into())),
                                    Store::rebuild(&expected).get(scope, &Key(key.into())),
                                    "R1/R2 {scope:?}/{terminal_remove}/{later_remove}/{has_later}/{legacy}/{typed_none}"
                                );
                                let cold: Vec<StateCommand> =
                                    serde_json::from_slice(&serde_json::to_vec(&history).unwrap())
                                        .unwrap();
                                assert_eq!(store_from_run_commands(cold, &run), warm, "R1/R2 cold");
                                batch.finalize().unwrap();
                                history.push(write(&batch));
                                let finalized = store_from_run_commands(history.clone(), &run);
                                assert_eq!(
                                    finalized.get(scope, &Key(key.into())),
                                    raw.get(scope, &Key(key.into())),
                                    "E3 no fabricated publication"
                                );
                                let mut corrupt = write(&batch);
                                corrupt.bind_run(&run);
                                corrupt.action =
                                    StateAction::Set(serde_json::json!({"corrupt":true}));
                                history.push(corrupt);
                                assert!(
                                    ActiveToolBatch::load(&store_from_run_commands(history, &run))
                                        .is_err(),
                                    "E4"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
