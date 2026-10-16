//! Coordination events derived directly from one committed root Thread
//! prefix: each message a parent sent to a coordinated child, and where each
//! sits in the canonical order.
//!
//! The Managed adapter supplies only its existing public tool-id codec. Tool
//! visibility, receipt selection and ordering stay in this pure domain fold;
//! no rendered Event or warm Session record is an input.

use std::collections::HashMap;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_application_contract::Pending;
use awaken_session_execution_contract::CoordinatedThreadLink;

use crate::{
    DerivedSessionEvent, EventIdentity, EventPosition, IssuedToolEvent, OrderIndex,
    coordination_orders, cross_posts, is_coordination_send, plan_folds,
};

/// Borrowed committed evidence the coordination events are derived from.
pub struct CoordinationEvidence<'a> {
    pub session_id: &'a str,
    pub root: Option<&'a RunRecoverySnapshot>,
    pub root_blocked: bool,
    pub root_response_coordinates: &'a HashMap<String, (String, usize, usize)>,
    pub links: &'a [CoordinatedThreadLink],
    pub lifecycle: &'a [RunLifecycleEvent],
    pub historical_pending: &'a HashMap<RunLifecycleCursor, Pending>,
    pub order_index: &'a OrderIndex<'a>,
}

/// The coordination events of one committed prefix: the messages sent to
/// coordinated children and the canonical position of each coordination
/// call and result. Derived once; reused by child activity and the final
/// canonical ordering.
pub struct CoordinationEvents {
    pub cross_posts: Vec<DerivedSessionEvent>,
    pub coordination_orders: HashMap<EventIdentity, EventPosition>,
}

enum SourceEvent {
    Use {
        event_id: String,
        call_id: String,
        source_id: String,
        name: String,
        input: serde_json::Value,
    },
    Result {
        event_id: String,
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },
}

/// Derive the coordination events once from committed source evidence.
/// `public_tool_id` is the Managed adapter's existing identity codec; changing
/// its bytes would change previously issued coordination FactKeys.
pub fn derive_coordination_events(
    prefix: &CoordinationEvidence<'_>,
    public_tool_id: impl Fn(&str, &str, &str) -> String,
) -> Result<CoordinationEvents, crate::ProjectionError> {
    let mut source_events = Vec::new();
    let mut orders = HashMap::new();
    let mut projected_sources = std::collections::HashSet::<(String, String)>::new();
    if prefix.root.is_some() {
        // A committed Awaiting audit can make a tool use public before its
        // Assistant Message commits. Select from the same as-of-commit list
        // as the adapter's synthetic-tool renderer, then retain its
        // Run-qualified identity when the later Message is folded.
        for tool in prefix.order_index.synthetic_awaiting_tools()? {
            if tool.thread_id != prefix.session_id {
                continue;
            }
            let pending = &tool.pending;
            projected_sources.insert((tool.run_id.0.clone(), pending.tool_use_id.clone()));
            if !is_coordination_send(&pending.name) {
                continue;
            }
            let event_id = public_tool_id(prefix.session_id, &tool.run_id.0, &pending.tool_use_id);
            if let Some(order) = prefix.order_index.tool_order(
                prefix.session_id,
                &tool.run_id.0,
                &pending.tool_use_id,
            ) {
                orders.insert(event_id.clone(), order);
            }
            source_events.push(SourceEvent::Use {
                event_id,
                call_id: pending.tool_use_id.clone(),
                source_id: tool.run_id.0.clone(),
                name: pending.name.clone(),
                input: pending.input.clone(),
            });
        }
    }
    if let Some(root) = prefix.root.filter(|_| !prefix.root_blocked) {
        let pending_source_run_id = prefix
            .lifecycle
            .iter()
            .rev()
            .find(|event| {
                event.thread_id.0 == prefix.session_id
                    && event.kind == RunLifecycleEventKind::Awaiting
            })
            .map(|event| &event.run_id);
        let transcript = prefix
            .order_index
            .transcript_evidence(prefix.session_id)
            .expect("root committed snapshot is in the same order index");
        let mut seen_messages = std::collections::HashSet::new();
        for fold in plan_folds(
            prefix.session_id,
            &root.messages,
            prefix.root_response_coordinates,
            prefix.historical_pending,
            prefix.lifecycle,
            pending_source_run_id,
        ) {
            let message = fold.message;
            if !seen_messages.insert(message.id.0.as_str()) || message.id.is_agent_thread_report() {
                continue;
            }
            match message.role {
                Role::Assistant => {
                    let projected = transcript.project_message(
                        message,
                        &projected_sources,
                        fold.source_run_id.map(|run| run.0.as_str()),
                        &fold.pending_history(),
                        None,
                    );
                    for block in &projected.message.content {
                        let ContentBlock::ToolUse { id, name, input } = block else {
                            continue;
                        };
                        projected_sources.insert((message.id.0.clone(), id.clone()));
                        if !is_coordination_send(name) {
                            continue;
                        }
                        let event_id = public_tool_id(prefix.session_id, &message.id.0, id);
                        if let Some(order) =
                            prefix
                                .order_index
                                .tool_order(prefix.session_id, &message.id.0, id)
                        {
                            orders.insert(event_id.clone(), order);
                        }
                        source_events.push(SourceEvent::Use {
                            event_id,
                            call_id: id.clone(),
                            source_id: message.id.0.clone(),
                            name: name.clone(),
                            input: input.clone(),
                        });
                    }
                }
                Role::Tool => append_result_events(
                    prefix,
                    message,
                    &projected_sources,
                    &mut source_events,
                    &mut orders,
                    &public_tool_id,
                )?,
                Role::User | Role::System => {}
            }
        }
    }
    let events = source_events
        .iter()
        .map(|event| match event {
            SourceEvent::Use {
                event_id,
                call_id,
                source_id,
                name,
                input,
            } => IssuedToolEvent::Use {
                event_id,
                call_id,
                source: Some((prefix.session_id, source_id)),
                name,
                input,
                owned: false,
            },
            SourceEvent::Result {
                event_id,
                tool_use_id,
                content,
                is_error,
            } => IssuedToolEvent::Result {
                event_id,
                tool_use_id,
                content,
                is_error: Some(*is_error),
                owned: false,
            },
        })
        .collect::<Vec<_>>();
    let order_refs = orders
        .iter()
        .map(|(id, order)| (id.as_str(), *order))
        .collect();
    Ok(CoordinationEvents {
        cross_posts: cross_posts(&events, prefix.links),
        coordination_orders: coordination_orders(
            prefix.order_index,
            &events,
            prefix.links,
            &order_refs,
        ),
    })
}

fn append_result_events(
    prefix: &CoordinationEvidence,
    message: &awaken_agent_contract::agent::message::Message,
    projected_sources: &std::collections::HashSet<(String, String)>,
    source_events: &mut Vec<SourceEvent>,
    orders: &mut HashMap<String, EventPosition>,
    public_tool_id: &impl Fn(&str, &str, &str) -> String,
) -> Result<(), crate::ProjectionError> {
    let transcript = prefix
        .order_index
        .transcript_evidence(prefix.session_id)
        .expect("the result belongs to the same committed root prefix");
    let mut sources = prefix
        .order_index
        .tool_result_sources(prefix.session_id, message)?
        .into_iter();
    for (block_ordinal, block) in message.content.iter().enumerate() {
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = block
        else {
            continue;
        };
        if transcript
            .advisor_blocks()
            .is_result(&message.id.0, block_ordinal)
        {
            continue;
        }
        let source = sources
            .next()
            .filter(|source| source.call_id == *tool_use_id)
            .ok_or_else(|| {
                crate::ProjectionError::corrupt(
                    "coordination result has no exact committed ToolUse source",
                )
            })?;
        if !projected_sources.contains(&(source.source_id.clone(), source.call_id.clone())) {
            continue;
        }
        let public_tool_use_id = public_tool_id(prefix.session_id, &source.source_id, tool_use_id);
        let event_id = format!("committed-result:{}:{block_ordinal}", message.id.0);
        if let Some(order) =
            prefix
                .order_index
                .message_call_order(prefix.session_id, &message.id.0, tool_use_id)
        {
            orders.insert(event_id.clone(), order);
        }
        source_events.push(SourceEvent::Result {
            event_id,
            tool_use_id: public_tool_use_id,
            content: content.clone(),
            is_error: *is_error,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message};
    use awaken_agent_contract::agent::run::{Id as RunId, Record, RunState};
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_session_execution_contract::CoordinatedThreadTarget;

    #[test]
    fn committed_receipt_alone_selects_the_cross_post_and_its_anchor() {
        // Causes: C1 a committed send call at cursor 10; C2 a committed result
        // at 30 whose first receipt accepts the exact linked child; C3 result
        // error, foreign child receipt, or blocked root. Effects: E1 C1 alone
        // emits no post; E2 C1+C2 emits one post at 30 with the unchanged
        // qualified call identity; E3 each C3 variant emits none. Decision
        // rows R1=C1=>E1; R2=C1+C2=>E2; R3=C1+C2+C3=>E3.
        let run = RunId("root-run".into());
        let call = Message::new(
            MessageId::assistant(&run, 0),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "call",
                "send_message",
                serde_json::json!({"agent_id":"researcher","message":"investigate"}),
            )],
        );
        let result = |child: &str, is_error| {
            let mut message = Message::new(
                MessageId("accepted-result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    "call",
                    vec![ContentBlock::text(
                        serde_json::json!({"accepted":true,"session_thread_id":child}).to_string(),
                    )],
                )],
            );
            if let ContentBlock::ToolResult {
                is_error: error, ..
            } = &mut message.content[0]
            {
                *error = is_error;
            }
            message
        };
        let base = RunRecoverySnapshot {
            thread_id: ThreadId("session".into()),
            claimed_run_id: run.clone(),
            runs: vec![Record {
                id: run.clone(),
                thread_id: ThreadId("session".into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run),
            messages: vec![call.clone()],
            message_commit_cursors: vec![10],
            commit_timestamps: [(10, 1_000), (30, 3_000)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 30,
            next_commit_ordinal: 0,
        };
        let operation =
            awaken_tool_contract::ActiveToolBatch::operation_for_message(&base, &call, "call")
                .unwrap()
                .expect("committed Assistant call owns an operation");
        let link = CoordinatedThreadLink {
            session_id: awaken_session_types::SessionId::from("session"),
            thread_id: ThreadId("child".into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: operation,
            latest_run_id: None,
        };
        let links = [link];
        let children = HashMap::new();
        let historical_pending = HashMap::new();
        for (rule, result, blocked, expected) in [
            ("R1", None, false, false),
            ("R2", Some(result("child", false)), false, true),
            ("R3 error", Some(result("child", true)), false, false),
            ("R3 foreign", Some(result("foreign", false)), false, false),
            ("R3 blocked", Some(result("child", false)), true, false),
        ] {
            let mut root = base.clone();
            if let Some(result) = result {
                root.messages.push(result);
                root.message_commit_cursors.push(30);
            }
            let index = OrderIndex::new(crate::OrderEvidence {
                updates: None,
                session_id: "session",
                batches: &[],
                closed_intervals: &[],
                budget_reach: &[],
                parent_terminal: crate::ParentTerminalEvidence::default(),
                lifecycle: &[],
                root: Some(&root),
                children: &children,
                links: &links,
                outcomes: &[],
                historical_pending: &historical_pending,
            });
            let projected = derive_coordination_events(
                &CoordinationEvidence {
                    session_id: "session",
                    root: Some(&root),
                    root_blocked: blocked,
                    root_response_coordinates: &HashMap::new(),
                    links: &links,
                    lifecycle: &[],
                    historical_pending: &historical_pending,
                    order_index: &index,
                },
                |thread, source, call| format!("{thread}|{source}|{call}"),
            )
            .unwrap();
            assert_eq!(projected.cross_posts.len(), usize::from(expected), "{rule}");
            if expected {
                let post = &projected.cross_posts[0];
                assert_eq!(post.key.thread, "child", "{rule}/E2 owner");
                assert!(
                    matches!(
                        &post.key.provenance,
                        crate::FactProvenance::CoordinationCall { event_id }
                            if event_id == &format!("session|{}|call", call.id.0)
                    ),
                    "{rule}/E2 unchanged identity"
                );
                assert_eq!(
                    projected.coordination_orders[&post.key].source_commit_cursor, 30,
                    "{rule}/E2 result commit anchor"
                );
            }
        }
    }

    #[test]
    fn audited_synthetic_call_keeps_its_run_identity_until_the_result_commit() {
        // Causes: S1 a committed Awaiting audit at 10 names send_message but
        // no ToolUse Message exists as of 10; S2 a matching accepted result
        // commits at 30. Effects: E1 S1 exposes the Run-qualified call, not a
        // guessed Message-qualified call; E2 S1+S2 selects one cross-post at
        // 30. Rules R1=S1=>E1; R2=S1+S2=>E1+E2.
        let run = RunId("run".into());
        let cursor = awaken_agent_contract::encode_run_lifecycle_cursor(10, 0).unwrap();
        let lifecycle = [RunLifecycleEvent {
            cursor,
            source_commit_cursor: 10,
            thread_id: ThreadId("session".into()),
            run_id: run.clone(),
            kind: RunLifecycleEventKind::Awaiting,
            state: RunState::Awaiting,
            await_reason: None,
        }];
        let pending = Pending {
            tool_use_id: "call".into(),
            name: "send_message".into(),
            input: serde_json::json!({"agent_id":"researcher","message":"investigate"}),
            client_executed: false,
        };
        let historical_pending = HashMap::from([(cursor, pending)]);
        let root = RunRecoverySnapshot {
            thread_id: ThreadId("session".into()),
            claimed_run_id: run.clone(),
            runs: vec![Record {
                id: run.clone(),
                thread_id: ThreadId("session".into()),
                state: RunState::Awaiting,
            }],
            latest_run_id: Some(run),
            messages: vec![Message::new(
                MessageId("result".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    "call",
                    vec![ContentBlock::text(
                        r#"{"accepted":true,"session_thread_id":"child"}"#,
                    )],
                )],
            )],
            message_commit_cursors: vec![30],
            commit_timestamps: [(10, 1_000), (30, 3_000)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 30,
            next_commit_ordinal: 0,
        };
        let links = [CoordinatedThreadLink {
            session_id: awaken_session_types::SessionId::from("session"),
            thread_id: ThreadId("child".into()),
            target: CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: "run:call".into(),
            latest_run_id: None,
        }];
        let children = HashMap::new();
        let index = OrderIndex::new(crate::OrderEvidence {
            updates: None,
            session_id: "session",
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: crate::ParentTerminalEvidence::default(),
            lifecycle: &lifecycle,
            root: Some(&root),
            children: &children,
            links: &links,
            outcomes: &[],
            historical_pending: &historical_pending,
        });
        let plan = derive_coordination_events(
            &CoordinationEvidence {
                session_id: "session",
                root: Some(&root),
                root_blocked: false,
                root_response_coordinates: &HashMap::new(),
                links: &links,
                lifecycle: &lifecycle,
                historical_pending: &historical_pending,
                order_index: &index,
            },
            |thread, source, call| format!("{thread}|{source}|{call}"),
        )
        .unwrap();
        assert_eq!(plan.cross_posts.len(), 1, "R2/E2");
        let key = &plan.cross_posts[0].key;
        assert!(
            matches!(
                &key.provenance,
                crate::FactProvenance::CoordinationCall { event_id }
                    if event_id == "session|run|call"
            ),
            "R1/E1"
        );
        assert_eq!(
            plan.coordination_orders[key].source_commit_cursor, 30,
            "R2/E2"
        );
    }
}
