// @formal entrypoint runtime.settled_step_projection_is_closed_exact_and_non_widening
//! Committed-step and resume assertions at the Host boundary.

use super::*;

impl SharedHost {
    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// Run's pending tool and its binding must match the inbound resume kind.
    pub(in crate::host) fn check_pending(
        &self,
        ticket: &ResumeTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), HostError> {
        let pending = Pending::from_resume_ticket(ticket)
            .ok_or_else(|| HostError::internal("awaiting run has no pending tool"))?;
        if pending.tool_use_id != tool_use_id {
            return Err(HostError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        if pending.client_executed != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(HostError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}

/// Locate the committed prefix immediately before one exact admitted input set.
pub(super) fn message_prefix_before_exact_inputs(
    committed: &RunRecoverySnapshot,
    expected_input_ids: &[String],
) -> Result<usize, HostError> {
    if expected_input_ids.is_empty() {
        return Err(HostError::bad_request(
            "Session Run admission requires at least one input message",
        ));
    }
    let mut positions = Vec::with_capacity(expected_input_ids.len());
    for expected in expected_input_ids {
        let mut matches = committed
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.id.0 == *expected)
            .map(|(index, _)| index);
        let Some(position) = matches.next() else {
            return Err(HostError::internal(format!(
                "committed Session Run is missing input message `{expected}`"
            )));
        };
        if matches.next().is_some() {
            return Err(HostError::internal(format!(
                "committed Session Run contains duplicate input message `{expected}`"
            )));
        }
        positions.push(position);
    }
    let first = positions[0];
    if positions
        .iter()
        .copied()
        .enumerate()
        .any(|(offset, position)| position != first.saturating_add(offset))
    {
        return Err(HostError::internal(
            "committed Session Run input messages are not one exact ordered prefix",
        ));
    }
    Ok(first)
}

/// Bind executor completion to the exact durable Thread/Run state and inputs.
pub(super) fn verify_committed_step(
    committed: &RunRecoverySnapshot,
    thread_id: &ThreadId,
    run_id: &RunId,
    returned_state: &RunState,
    before: usize,
    expected_input_ids: &[String],
) -> Result<Vec<Message>, HostError> {
    if &committed.thread_id != thread_id || &committed.claimed_run_id != run_id {
        return Err(HostError::internal(
            "committed step proof names another Thread or Run",
        ));
    }
    let committed_state = committed
        .run_record(run_id)
        .map_err(|error| HostError::internal(error.to_string()))?
        .map(|run| &run.state)
        .ok_or_else(|| HostError::internal("committed step proof has no Run record"))?;
    if committed_state != returned_state {
        return Err(HostError::internal(
            "executor result does not match the committed Run state",
        ));
    }
    // A fast child report may admit the next Run before the original caller
    // reads its settled response. The latest Run is then a different, valid
    // coordinate; its messages must not enter this step's protocol response.
    // The existing lifecycle event and message commit cursors bound the exact
    // terminal prefix without reopening another store or inventing a receipt.
    let superseded = committed.latest_run_id.as_ref() != Some(run_id);
    let message_end = if superseded {
        committed
            .current_run()
            .map_err(|error| HostError::internal(error.to_string()))?
            .ok_or_else(|| HostError::internal("committed step proof has no latest Run"))?;
        if !matches!(returned_state, RunState::Ended(_)) {
            return Err(HostError::internal(
                "nonterminal step proof is not the latest Run on its Thread",
            ));
        }
        let cursors = &committed.message_commit_cursors;
        if cursors.len() != committed.messages.len()
            || cursors
                .iter()
                .any(|cursor| *cursor == 0 || *cursor > committed.store_cursor)
            || cursors.windows(2).any(|pair| pair[0] > pair[1])
        {
            return Err(HostError::internal(
                "superseded step proof lacks ordered message commit cursors",
            ));
        }
        let lifecycle = committed
            .lifecycle_events_after(awaken_agent_contract::RunLifecycleCursor(0), usize::MAX)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let mut terminal = lifecycle.events.iter().filter(|event| {
            &event.run_id == run_id
                && &event.state == returned_state
                && matches!(
                    event.kind,
                    awaken_agent_contract::RunLifecycleEventKind::Completed
                        | awaken_agent_contract::RunLifecycleEventKind::Failed
                        | awaken_agent_contract::RunLifecycleEventKind::Cancelled
                )
        });
        let exact = terminal.next().ok_or_else(|| {
            HostError::internal("superseded step proof has no exact terminal lifecycle event")
        })?;
        if exact.source_commit_cursor == 0 || terminal.next().is_some() {
            return Err(HostError::internal(
                "superseded step proof has ambiguous terminal lifecycle evidence",
            ));
        }
        cursors.partition_point(|cursor| *cursor <= exact.source_commit_cursor)
    } else {
        committed.messages.len()
    };
    if matches!(returned_state, RunState::Running) {
        return Err(HostError::internal("committed step proof is not settled"));
    }
    if message_end < before {
        return Err(HostError::internal(
            "committed Thread message prefix moved backwards",
        ));
    }
    if committed.thread_version == 0 || committed.next_commit_ordinal == 0 {
        return Err(HostError::internal(
            "committed step proof has no durable commit identity",
        ));
    }
    for expected in expected_input_ids {
        if !committed.messages[..message_end]
            .iter()
            .any(|message| message.id.0 == *expected)
        {
            return Err(HostError::internal(format!(
                "committed step proof is missing input message `{expected}`"
            )));
        }
    }
    let suffix = committed.messages[before..message_end].to_vec();
    if superseded
        && suffix.iter().any(|message| {
            message.role == Role::Assistant
                && message
                    .id
                    .assistant_coordinates()
                    .is_none_or(|(owner, _, _)| owner != run_id.0.as_str())
        })
    {
        return Err(HostError::internal(
            "superseded step proof contains another Run's assistant output",
        ));
    }
    // Natural completion requires committed assistant output. An error state
    // already carries its typed explanation in the authoritative RunState.
    if matches!(returned_state, RunState::Ended(EndCause::NaturalEnd))
        && !suffix.iter().any(|message| message.role == Role::Assistant)
    {
        return Err(HostError::internal(
            "committed natural terminal step has no assistant output",
        ));
    }
    Ok(suffix)
}

#[cfg(test)]
mod committed_step_proof_tests {
    use super::*;
    use awaken_agent_contract::agent::message::Role;
    use awaken_agent_contract::agent::run::Record as RunRecord;
    use awaken_agent_contract::audit::{kind::Kind as AuditKind, record::Record as AuditRecord};

    fn snapshot() -> RunRecoverySnapshot {
        let thread_id = ThreadId("thread-proof".into());
        let run_id = RunId("run-proof".into());
        RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: thread_id.clone(),
            claimed_run_id: run_id.clone(),
            runs: vec![RunRecord {
                id: run_id.clone(),
                thread_id,
                state: RunState::Ended(EndCause::NaturalEnd),
            }],
            latest_run_id: Some(run_id),
            messages: vec![
                Message::text(MessageId("input-proof".into()), Role::User, "question"),
                Message::text(MessageId("output-proof".into()), Role::Assistant, "answer"),
            ],
            message_commit_cursors: Vec::new(),
            state: Vec::new(),
            state_commit_cursors: Vec::new(),
            events: Vec::new(),
            resume_tickets: Vec::new(),
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 1,
        }
    }

    #[test]
    fn committed_step_proof_follows_the_fmeca_decision_table() {
        // Constraint/Invariant: the authoritative inputs and ownership boundaries
        // documented here remain the only decision source; no parallel path is admitted.
        // Decision rule: execute every reachable cause partition documented here and
        // require its stated effects, including each fail-closed outcome.
        // FMECA causes: C1 exact Thread/claimed Run; C2 latest Run identity or
        // exact terminal supersession evidence;
        // C3 committed RunRecord exists; C4 executor/committed state agree;
        // C5 committed message count does not regress; C6 every accepted input
        // MessageId exists; C7 the snapshot carries a nonzero commit identity;
        // C8 a natural terminal step has committed assistant output, while an
        // error terminal carries its explanation in the committed RunState; C9
        // the state is settled rather than Running. Effects: E1 issue the
        // unforgeable receipt and project only the committed suffix; E2 fail
        // closed before StepOutcome/SSE completion. Severity is critical: any
        // false rule would report completion without authoritative history.
        //
        // | Rule | Failed cause | Effect |
        // | P1   | none         | E1     |
        // | P2   | C1           | E2     |
        // | P3   | C2           | E2     |
        // | P4   | C3           | E2     |
        // | P5   | C4           | E2     |
        // | P6   | C5           | E2     |
        // | P7   | C6           | E2     |
        // | P8   | C7           | E2     |
        // | P9   | C8           | E2     |
        // | P10  | C9           | E2     |
        // P4 also crosses exact unique/absent/equal-duplicate/conflicting and
        // blank/foreign owner records. All malformed selections fail through
        // the shared SDK law before any completion receipt can be issued.
        let thread = ThreadId("thread-proof".into());
        let run = RunId("run-proof".into());
        let state = RunState::Ended(EndCause::NaturalEnd);
        let expected = vec!["input-proof".to_string()];

        let suffix =
            verify_committed_step(&snapshot(), &thread, &run, &state, 1, &expected).expect("P1/E1");
        assert_eq!(suffix.len(), 1, "P1/E1");
        assert_eq!(suffix[0].id.0, "output-proof", "P1/E1");
        let receipt = CommittedStepReceipt::from_verified(
            VerifiedStepProjection {
                run_id: run.clone(),
                new_messages: suffix,
                state: state.clone(),
                pending: None,
                await_reason: None,
                delegated_runs: Vec::new(),
            },
            &snapshot(),
        );
        assert_eq!(receipt.thread_id().0, "thread-proof", "P1/E1");
        assert_eq!(receipt.commit_sequence(), 1, "P1/E1");
        assert_eq!(receipt.store_cursor(), 1, "P1/E1");
        assert_eq!(receipt.operation_ordinal(), 0, "P1/E1");
        assert_eq!(
            receipt.first_message_id().map(|id| id.0.as_str()),
            Some("output-proof"),
            "P1/E1"
        );
        assert_eq!(
            receipt.last_message_id().map(|id| id.0.as_str()),
            Some("output-proof"),
            "P1/E1"
        );

        let mut cases = Vec::new();
        let mut wrong_thread = snapshot();
        wrong_thread.thread_id = ThreadId("other".into());
        cases.push(("P2", wrong_thread, state.clone(), 1, expected.clone()));
        let mut stale_latest = snapshot();
        stale_latest.latest_run_id = Some(RunId("older".into()));
        cases.push(("P3", stale_latest, state.clone(), 1, expected.clone()));
        let mut missing_run = snapshot();
        missing_run.runs.clear();
        cases.push(("P4", missing_run, state.clone(), 1, expected.clone()));
        for defect in 0..4 {
            let mut invalid = snapshot();
            match defect {
                0 | 1 => {
                    let mut duplicate = invalid.runs[0].clone();
                    if defect == 1 {
                        duplicate.state = RunState::Awaiting;
                    }
                    invalid.runs.push(duplicate);
                }
                2 => invalid.runs[0].thread_id = ThreadId("foreign".into()),
                _ => invalid.runs[0].thread_id = ThreadId(" ".into()),
            }
            cases.push(("P4", invalid, state.clone(), 1, expected.clone()));
        }
        cases.push((
            "P5",
            snapshot(),
            RunState::Ended(EndCause::Cancelled),
            1,
            expected.clone(),
        ));
        cases.push(("P6", snapshot(), state.clone(), 3, expected.clone()));
        cases.push((
            "P7",
            snapshot(),
            state.clone(),
            1,
            vec!["missing-input".into()],
        ));
        let mut missing_commit_identity = snapshot();
        missing_commit_identity.thread_version = 0;
        missing_commit_identity.next_commit_ordinal = 0;
        cases.push((
            "P8",
            missing_commit_identity,
            state.clone(),
            1,
            expected.clone(),
        ));
        let mut missing_output = snapshot();
        missing_output.messages.pop();
        cases.push(("P9", missing_output, state.clone(), 0, expected.clone()));
        let mut running = snapshot();
        running.runs[0].state = RunState::Running;
        cases.push(("P10", running, RunState::Running, 0, expected.clone()));
        for (rule, snapshot, returned, before, inputs) in cases {
            assert!(
                verify_committed_step(&snapshot, &thread, &run, &returned, before, &inputs)
                    .is_err(),
                "{rule}/E2"
            );
        }

        let mut committed_error = snapshot();
        committed_error.messages.pop();
        let error_state = RunState::Ended(EndCause::Error(
            awaken_agent_contract::agent::run::Failure::Inference {
                code: "provider_error".into(),
                message: "provider connection closed".into(),
            },
        ));
        committed_error.runs[0].state = error_state.clone();
        assert!(
            verify_committed_step(&committed_error, &thread, &run, &error_state, 0, &expected,)
                .is_ok(),
            "P11/E1 committed typed error is its own explanation"
        );
    }

    #[test]
    fn settled_run_projection_is_bounded_when_a_child_report_starts_a_successor() {
        // Causes: C1 the requested Run ended at commit 2; C2 its fast child
        // report admits another Run at commits 3-4 before response projection;
        // C3 message cursors are aligned and ordered; C4 the original Run has
        // exactly one terminal event with a positive source commit cursor.
        // Effects: E1 the original response contains only its own committed
        // input/output; E2 absent, malformed, ambiguous or foreign evidence
        // refuses the response. Decision rules: S1=C1+C2+C3+C4 -> E1;
        // S2=C1+C2+(!C3|!C4) -> E2; S3 a newer input asserted as the old Run's
        // input -> E2. The latest-Run read remains authoritative for current
        // state, while the prior Run's terminal event bounds historical output.
        let mut committed = snapshot();
        let original = RunId("run-proof".into());
        let successor = RunId("run-successor".into());
        let terminal = RunState::Ended(EndCause::NaturalEnd);
        committed.messages[1].id = MessageId::assistant(&original, 0);
        committed.messages.push(Message::text(
            MessageId("successor-input".into()),
            Role::User,
            "child report",
        ));
        committed.messages.push(Message::text(
            MessageId::assistant(&successor, 0),
            Role::Assistant,
            "later answer",
        ));
        committed.message_commit_cursors = vec![1, 2, 3, 4];
        committed.events = [
            (1, &original, RunState::Running),
            (2, &original, terminal.clone()),
            (3, &successor, RunState::Running),
            (4, &successor, terminal.clone()),
        ]
        .into_iter()
        .map(|(cursor, run_id, state)| AuditRecord {
            sequence: awaken_agent_contract::encode_run_lifecycle_cursor(cursor, 0)
                .unwrap()
                .0,
            run_id: run_id.clone(),
            kind: AuditKind::RunStateChanged,
            payload: serde_json::json!({ "state": state }),
        })
        .collect();
        committed.runs.push(RunRecord {
            id: successor.clone(),
            thread_id: committed.thread_id.clone(),
            state: terminal.clone(),
        });
        committed.latest_run_id = Some(successor.clone());
        committed.thread_version = 4;
        committed.store_cursor = 4;
        committed.next_commit_ordinal = 2;
        let expected = ["input-proof".to_string()];
        let projected = verify_committed_step(
            &committed,
            &committed.thread_id,
            &original,
            &terminal,
            0,
            &expected,
        )
        .expect("S1 exact terminal supersession");
        assert_eq!(
            projected
                .iter()
                .map(|message| message.id.0.as_str())
                .collect::<Vec<_>>(),
            ["input-proof", "run-proof-assistant-0"],
            "S1/E1 excludes the later Run's input and output"
        );

        let mut cases = Vec::new();
        let mut no_event = committed.clone();
        no_event.events.retain(|event| {
            !(event.run_id == original && event.payload["state"] == serde_json::json!(terminal))
        });
        cases.push(("S2 missing terminal", no_event, 0, expected.clone()));
        let mut duplicate_terminal = committed.clone();
        let mut repeated = duplicate_terminal.events[1].clone();
        repeated.sequence = awaken_agent_contract::encode_run_lifecycle_cursor(2, 1)
            .unwrap()
            .0;
        duplicate_terminal.events.insert(2, repeated);
        cases.push((
            "S2 ambiguous terminal",
            duplicate_terminal,
            0,
            expected.clone(),
        ));
        let mut zero_terminal_commit = committed.clone();
        zero_terminal_commit.events.remove(0);
        zero_terminal_commit.events[0].sequence =
            awaken_agent_contract::encode_run_lifecycle_cursor(0, 1)
                .unwrap()
                .0;
        cases.push((
            "S2 zero terminal commit",
            zero_terminal_commit,
            0,
            expected.clone(),
        ));
        let mut no_cursors = committed.clone();
        no_cursors.message_commit_cursors.clear();
        cases.push(("S2 missing cursors", no_cursors, 0, expected.clone()));
        let mut unordered = committed.clone();
        unordered.message_commit_cursors[2] = 1;
        cases.push(("S2 unordered cursors", unordered, 0, expected.clone()));
        let mut foreign_output = committed.clone();
        foreign_output.messages[1].id = MessageId::assistant(&successor, 0);
        cases.push(("S2 foreign assistant", foreign_output, 0, expected.clone()));
        let mut no_latest_record = committed.clone();
        no_latest_record.runs.pop();
        cases.push((
            "S2 missing successor record",
            no_latest_record,
            0,
            expected.clone(),
        ));
        cases.push((
            "S3 successor input",
            committed.clone(),
            2,
            ["successor-input".to_string()],
        ));
        for (rule, facts, before, inputs) in cases {
            assert!(
                verify_committed_step(
                    &facts,
                    &facts.thread_id,
                    &original,
                    &terminal,
                    before,
                    &inputs,
                )
                .is_err(),
                "{rule}/E2"
            );
        }
    }

    #[test]
    fn session_run_input_prefix_follows_the_replay_decision_table() {
        // Cause/effect graph: C1 expected input ids are non-empty; C2 every id
        // occurs exactly once; C3 positions are contiguous and ordered. Effects:
        // E1 return the exact preceding prefix for first delivery/replay; E2 fail
        // closed before projecting a Step. Decision rules: R1=C1+C2+C3=>E1;
        // R2=!C1|!C2|!C3=>E2. This prevents a response retry from using the
        // caller's current history length as a second truth.
        let committed = snapshot();
        assert_eq!(
            message_prefix_before_exact_inputs(&committed, &["input-proof".into()]).expect("R1/E1"),
            0,
            "R1/E1"
        );
        assert!(
            message_prefix_before_exact_inputs(&committed, &[]).is_err(),
            "R2/E2 empty"
        );
        assert!(
            message_prefix_before_exact_inputs(&committed, &["missing".into()]).is_err(),
            "R2/E2 missing"
        );

        let mut non_contiguous = committed.clone();
        non_contiguous.messages.insert(
            1,
            Message::text(MessageId("gap".into()), Role::System, "gap"),
        );
        assert!(
            message_prefix_before_exact_inputs(
                &non_contiguous,
                &["input-proof".into(), "output-proof".into()]
            )
            .is_err(),
            "R2/E2 non-contiguous"
        );
    }
}
