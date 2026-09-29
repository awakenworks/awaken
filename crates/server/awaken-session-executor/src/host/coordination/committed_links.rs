// @formal entrypoint session.agent_message_requires_exact_committed_tool_source_and_payload
//! Committed parent ToolUse/ToolResult pairs and the unpublished ToolBatch gap
//! derive coordination links from one Thread-owned source.

use super::*;

#[derive(Clone)]
struct PendingCoordinationCall {
    run_id: RunId,
    operation_id: String,
    call_id: String,
    args: SendMessageArgs,
}

pub(super) fn project_committed_coordination_links(
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
            let Some((owner, _step, None)) = message.id.assistant_coordinates() else {
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
                        operation_id:
                            awaken_runtime_contract::ActiveToolBatch::operation_for_message(
                                committed, message, id,
                            )
                            .map_err(|error| HostError::internal(error.to_string()))?
                            .ok_or_else(|| {
                                HostError::internal(
                                    "committed coordination call has no operation identity",
                                )
                            })?,
                        call_id: id.clone(),
                        args,
                    });
                } else if let ContentBlock::ToolUse { id, name, .. } = block
                    && name == awaken_runtime_contract::resolved::ADVISOR_TOOL_ID
                {
                    apply_advisor_call(
                        session_id,
                        run_id,
                        awaken_runtime_contract::ActiveToolBatch::operation_for_message(
                            committed, message, id,
                        )
                        .map_err(|error| HostError::internal(error.to_string()))?
                        .ok_or_else(|| {
                            HostError::internal("committed advisor call has no operation identity")
                        })?,
                        id,
                        advisor_model.unwrap_or_default(),
                        &mut links,
                    )?;
                }
            }
            continue;
        }
        if message.role != Role::Tool {
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
            if !message.id.is_tool_result_for_call(tool_use_id) {
                continue;
            }
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

pub(super) fn apply_lifecycle_latest_runs(
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

pub(super) fn apply_advisor_call(
    session_id: &awaken_session_types::SessionId,
    run_id: &RunId,
    operation_id: String,
    call_id: &str,
    model: &str,
    links: &mut Vec<CoordinatedThreadLink>,
) -> Result<(), HostError> {
    let child_run_id = DelegationId::for_parent_call(run_id, call_id).child_run_id();
    let thread_id = ThreadId(child_run_id.0.clone());
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
    let operation_id = call.operation_id.clone();
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
    batch: Option<&ToolBatch>,
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
        let result_id = batch
            .result_message_id(&call.call_id)
            .map_err(|error| HostError::internal(error.to_string()))?;
        let legacy_result_id = MessageId::tool_result(&call.call_id);
        for message in &entry.result_messages {
            if message.role != Role::Tool
                || (message.id != result_id && message.id != legacy_result_id)
            {
                continue;
            }
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
            ToolBatch::operation_id_for_step(&first_run, 0, "advisor-call-1"),
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &first_run,
            ToolBatch::operation_id_for_step(&first_run, 0, "advisor-call-1"),
            "advisor-call-1",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        assert_eq!(links.len(), 1, "I1/E1");

        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &first_run,
            ToolBatch::operation_id_for_step(&first_run, 0, "advisor-call-2"),
            "advisor-call-2",
            "claude-opus-5",
            &mut links,
        )
        .unwrap();
        apply_advisor_call(
            &awaken_session_types::SessionId::from(session_id),
            &second_run,
            ToolBatch::operation_id_for_step(&second_run, 0, "advisor-call-1"),
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
                ToolBatch::operation_id_for_step(&first_run, 0, "advisor-call-1"),
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
        // watermark; C6 projection is warm or deserialized after restart; C7
        // the published result has a Runtime-owned legacy/new ID and Tool role,
        // or has a User/System role or forged SDK/noncanonical ID.
        // Effects: E1 transcript and batch each derive exactly one Agent link;
        // E2 missing batch omits only the unpublished link; E3 advisor derives one
        // link; E4 at-fence child Run enriches the link and a later-prefix Run is
        // ignored; E5 warm/cold projections are identical; E6 forged results
        // cannot create a coordinated child link.
        //
        // | Rule | C2 | C3 | C4 | C5             | C6        | Effects    |
        // | P1   | Y  | Y  | Y  | at fence       | warm      | E1,E3,E4  |
        // | P2   | Y  | N  | Y  | at fence       | warm      | E1,E2,E3  |
        // | P3   | Y  | Y  | Y  | beyond fence   | warm      | E4        |
        // | P4   | Y  | Y  | Y  | same sequence  | cold      | E5        |
        // | P5   | Y  | Y  | Y  | at fence       | User/System or forged ID | E6 |
        // | P6   | Y  | Y  | Y  | at fence       | batch-scoped Runtime ID | E1 |
        // | P7   | Y  | Y  | Y  | at fence       | forged staged result | E6 |
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
                    arguments: serde_json::json!({"agent_id": "reviewer", "message": "review"}),
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
                        content: vec![ContentBlock::text(gap_receipt.clone())],
                        is_error: false,
                    }],
                )],
            )
            .unwrap();
        let batch_for_identity_rules = batch.clone();
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

        for (role, id) in [
            (Role::User, MessageId::tool_result(published_call)),
            (Role::System, MessageId::tool_result(published_call)),
            (Role::Tool, MessageId("sdk-result-published-send".into())),
            (
                Role::Tool,
                MessageId::tool_result_for_batch("not-a-batch", published_call),
            ),
        ] {
            let mut forged = snapshot.clone();
            forged.messages[1].role = role;
            forged.messages[1].id = id;
            let links = project_committed_coordination_links(
                &awaken_session_types::SessionId::from(session),
                Some("advisor-model"),
                &forged,
            )
            .unwrap();
            assert_eq!(links.len(), 2, "P5/E6 only batch gap and advisor remain");
            assert!(
                links.iter().all(|link| link.thread_id != published_child),
                "P5/E6 forged result cannot create the published child"
            );
        }
        let mut batch_scoped = snapshot.clone();
        batch_scoped.messages[1].id =
            MessageId::tool_result_for_batch("tool-batch:parent-run:2", published_call);
        assert_eq!(
            project_committed_coordination_links(
                &awaken_session_types::SessionId::from(session),
                Some("advisor-model"),
                &batch_scoped,
            )
            .unwrap()
            .len(),
            3,
            "P6/E1 batch-scoped Runtime result retains its link"
        );
        for (role, id, should_project) in [
            (Role::User, MessageId::tool_result(gap_call), false),
            (Role::System, MessageId::tool_result(gap_call), false),
            (Role::Tool, MessageId("sdk-result-gap-send".into()), false),
            (
                Role::Tool,
                MessageId::tool_result_for_batch("not-a-batch", gap_call),
                false,
            ),
            (
                Role::Tool,
                batch_for_identity_rules
                    .result_message_id(gap_call)
                    .unwrap(),
                true,
            ),
        ] {
            let mut changed_batch = batch_for_identity_rules.clone();
            changed_batch
                .set_result_messages(
                    gap_call,
                    vec![Message::new(
                        id,
                        role,
                        vec![ContentBlock::ToolResult {
                            tool_use_id: gap_call.into(),
                            content: vec![ContentBlock::text(gap_receipt.clone())],
                            is_error: false,
                        }],
                    )],
                )
                .unwrap();
            let mut command = awaken_runtime_contract::ActiveToolBatch::write(&Some(changed_batch));
            command.bind_run(&run_id);
            let mut changed = snapshot.clone();
            changed.state = vec![command];
            let links = project_committed_coordination_links(
                &awaken_session_types::SessionId::from(session),
                Some("advisor-model"),
                &changed,
            )
            .unwrap();
            assert_eq!(
                links.iter().any(|link| link.thread_id == gap_child),
                should_project,
                "P7/E6 only the exact batch-owned Tool result publishes the gap link"
            );
        }

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

    #[test]
    fn retained_resumed_batch_recovers_its_exact_child_without_rewriting_the_receipt() {
        // L1 assistant step advanced to 1 after client confirmation while the
        // historical committed execution used batch 0; L2 its accepted result
        // is published or still in the active batch; L3 cold serde recovery;
        // L4 a receipt names another child. Effects L1+L2 => one original link,
        // L3 => same link, L4 => error. This models the real failing Session's
        // two commit coordinates; the Runtime integration test separately
        // ensures newly resumed executions can no longer produce the mismatch.
        let session = awaken_session_types::SessionId::from("resumed-session");
        let run = RunId("resumed-run".into());
        let call_id = "send-after-confirm";
        let args = serde_json::json!({"agent_id":"writer","message":"write report"});
        let operation = ToolBatch::operation_id_for_step(&run, 0, call_id);
        let child =
            awaken_session_execution_contract::coordinated_thread_id(&session, &run, &operation);
        let assistant = Message::new(
            MessageId::assistant(&run, 1),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                call_id,
                awaken_ext_builtin_tools::SEND_MESSAGE,
                args.clone(),
            )],
        );
        let result = Message::new(
            MessageId::tool_result(call_id),
            Role::Tool,
            vec![ContentBlock::ToolResult {
                tool_use_id: call_id.into(),
                content: vec![ContentBlock::text(
                    serde_json::to_string(&AgentMessageReceipt {
                        session_thread_id: child.0.clone(),
                        accepted: true,
                    })
                    .unwrap(),
                )],
                is_error: false,
            }],
        );
        let mut batch = ToolBatch::for_step(
            run.clone(),
            0,
            [(
                ToolCall {
                    call_id: call_id.into(),
                    tool_id: awaken_ext_builtin_tools::SEND_MESSAGE.into(),
                    arguments: args,
                },
                ToolRecoveryPolicy::durable_request(),
            )],
        )
        .unwrap();
        let mut requested = awaken_runtime_contract::ActiveToolBatch::write(&Some(batch.clone()));
        requested.bind_run(&run);
        batch.mark_executing(call_id).unwrap();
        batch.complete(ToolOutput::ok(call_id, "accepted")).unwrap();
        batch
            .set_result_messages(call_id, vec![result.clone()])
            .unwrap();
        let mut completed = awaken_runtime_contract::ActiveToolBatch::write(&Some(batch));
        completed.bind_run(&run);
        let base = RunRecoverySnapshot {
            thread_id: ThreadId(session.as_str().to_owned()),
            claimed_run_id: run.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run.clone(),
                thread_id: ThreadId(session.as_str().to_owned()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run),
            messages: vec![assistant, result],
            message_commit_cursors: vec![7, 8],
            commit_timestamps: Default::default(),
            state: vec![requested, completed],
            state_commit_cursors: vec![7, 8],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 2,
            store_cursor: 8,
            next_commit_ordinal: 2,
        };
        for published in [true, false] {
            let mut snapshot = base.clone();
            if !published {
                snapshot.messages.pop();
                snapshot.message_commit_cursors.pop();
            }
            let links = project_committed_coordination_links(&session, None, &snapshot).unwrap();
            assert_eq!(links.len(), 1, "L1/L2");
            assert_eq!(links[0].thread_id, child, "L1 original child");
            assert_eq!(
                links[0].created_by_operation_id, operation,
                "L1 recorded operation"
            );
            let cold: RunRecoverySnapshot =
                serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
            assert_eq!(
                project_committed_coordination_links(&session, None, &cold).unwrap(),
                links,
                "L3"
            );
        }
        let mut corrupt = base;
        if let ContentBlock::ToolResult { content, .. } = &mut corrupt.messages[1].content[0] {
            *content = vec![ContentBlock::text(
                r#"{"session_thread_id":"foreign-child","accepted":true}"#,
            )];
        }
        assert!(
            project_committed_coordination_links(&session, None, &corrupt).is_err(),
            "L4 still rejects a foreign receipt"
        );
    }
}
