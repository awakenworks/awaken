//! Neutral projection events and the protocol `Transcoder` seam.
//!
//! This is the single shape every public protocol adapter projects from. A
//! committed step (the messages committed during a turn or resume, plus the
//! terminal state) is folded into a sequence of neutral [`AgentEvent`]s by
//! [`fold_messages`] / [`fold_step`]; each protocol then implements one
//! [`Transcoder`] that maps those events to its own wire vocabulary. The fold is
//! shared; only the transcoder differs per protocol (static Strategy).

use serde_json::Value;

use crate::agent::content::ContentBlock;
use crate::agent::message::{Message, Role};
use crate::agent::run::{EndCause, RunState};
use crate::event::{AgentEvent, Delta, Fact, ToolDisposition};

/// Transcode neutral events into a protocol's wire events — one impl per protocol,
/// the only per-protocol part of the projection pipeline. Two tiers (ADR-0058,
/// Axis 9): [`fact`] is **exhaustive** (the compiler forces every protocol to take
/// a stance on each committed whole-unit), [`delta`] is **opt-in** (default no-op;
/// a protocol overrides only the live increments it renders). One instance sees
/// both the live deltas and the later committed facts, so wire ids stay consistent
/// without cross-transcoder reconciliation. `&mut self` carries per-stream state
/// (open-text guard, id minting).
///
/// [`fact`]: Transcoder::fact
/// [`delta`]: Transcoder::delta
pub trait Transcoder {
    /// The protocol's wire event type.
    type Output;

    /// Transcode one committed whole-unit / lifecycle fact (exhaustive tier).
    fn fact(&mut self, fact: &Fact) -> Vec<Self::Output>;

    /// Transcode one live streaming increment (opt-in tier; default no-op).
    fn delta(&mut self, _delta: &Delta) -> Vec<Self::Output> {
        Vec::new()
    }

    /// Dispatch one neutral event to its tier.
    fn transcode(&mut self, event: &AgentEvent) -> Vec<Self::Output> {
        match event {
            AgentEvent::Fact(fact) => self.fact(fact),
            AgentEvent::Delta(delta) => self.delta(delta),
        }
    }

    /// Transcode a sequence of committed facts in order.
    fn transcode_facts(&mut self, facts: &[Fact]) -> Vec<Self::Output> {
        facts.iter().flat_map(|fact| self.fact(fact)).collect()
    }
}

/// Fold a committed step's messages into per-message neutral events (no
/// `RunStarted`, no terminal). `pending` is `(tool_use_id, client_executed)` of
/// the tool the run awaiting on, when it awaiting — it classifies that tool's call.
pub fn fold_messages(new_messages: &[Message], pending: Option<(&str, bool)>) -> Vec<Fact> {
    let mut out = Vec::new();
    for message in new_messages {
        match message.role {
            Role::Assistant => {
                // Reasoning precedes the answer: a folded `Thinking` block projects
                // as a contentless `AssistantThinking` marker before the message.
                if message
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
                {
                    out.push(Fact::AssistantThinking);
                }
                let text: Vec<ContentBlock> = message
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::Text { .. }))
                    .cloned()
                    .collect();
                // Emit only when there is *visible* text — an assistant message whose
                // only text block is empty is a useless empty wire event, and the
                // history fold ([`text_is_empty`]) already drops it. Both folds share
                // the one predicate so the "matching the streaming projection"
                // invariant holds by construction, not by two divergent inline tests.
                if !text_is_empty(&message.content) {
                    out.push(Fact::AssistantMessage {
                        id: message.id.0.clone(),
                        content: text,
                    });
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { id, name, input } = block {
                        let disposition = match pending {
                            Some((pid, client)) if pid == id.as_str() => {
                                if client {
                                    ToolDisposition::PendingClient
                                } else {
                                    ToolDisposition::PendingBuiltin
                                }
                            }
                            _ => ToolDisposition::Executed,
                        };
                        out.push(Fact::ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            input: input.clone(),
                            disposition,
                        });
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    {
                        out.push(Fact::ToolResult {
                            id: tool_use_id.clone(),
                            content: content.clone(),
                            is_error: false,
                        });
                    }
                }
            }
            Role::User | Role::System => {}
        }
    }
    out
}

/// A tool call the assistant made, borrowed from the committed message during a
/// [`fold_history`] walk. Adapters shape it into their own tool-part vocabulary.
pub struct ToolUseRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
}

/// The static-history counterpart to [`Transcoder`]: a sink that receives the
/// committed messages of a thread, oldest-first, already walked and correlated.
/// The fold ([`fold_history`]) owns the shared logic — skip-empty, tool-call
/// extraction, and per-message tool-result indexing; each adapter implements this
/// to shape a message into its own wire form. This is why two adapters that
/// project tool results differently (AI SDK folds a result into the assistant's
/// tool part; AG-UI emits a standalone `tool` message) still share one fold.
pub trait HistorySink {
    /// A user or system message; `content` is its blocks (never all-empty — an
    /// empty message is skipped by the fold). The adapter extracts text/parts.
    fn user_or_system(&mut self, id: &str, role: Role, content: &[ContentBlock]);
    /// An assistant turn: its `content` blocks (text; tool-use blocks live here
    /// too) and the tool calls pre-extracted. Skipped by the fold only when the
    /// text is empty *and* there are no tool calls.
    fn assistant(&mut self, id: &str, content: &[ContentBlock], tools: &[ToolUseRef<'_>]);
    /// A tool result answering `tool_use_id`, its `content` blocks. `message_id`+
    /// `sub` locate it inside the neutral tool message (`sub` 0 = first result);
    /// an adapter emitting a standalone message keys on these, one folding it into
    /// the assistant part keys on `tool_use_id`.
    fn tool_result(
        &mut self,
        message_id: &str,
        sub: usize,
        tool_use_id: &str,
        content: &[ContentBlock],
    );
}

/// True when a block list carries no text (text blocks only) — the fold's
/// skip-empty test, matching the streaming projection.
fn text_is_empty(content: &[ContentBlock]) -> bool {
    !content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()))
}

/// Walk a thread's committed messages (oldest-first) into `sink`, owning the
/// shared read-model logic so each adapter only shapes each message. An empty
/// user/system/assistant message is dropped, as the streaming projection drops it.
pub fn fold_history(messages: &[Message], sink: &mut impl HistorySink) {
    for message in messages {
        match message.role {
            Role::User | Role::System => {
                if text_is_empty(&message.content) {
                    continue;
                }
                sink.user_or_system(&message.id.0, message.role, &message.content);
            }
            Role::Assistant => {
                let tools: Vec<ToolUseRef<'_>> = message
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => Some(ToolUseRef {
                            id: id.as_str(),
                            name: name.as_str(),
                            input,
                        }),
                        _ => None,
                    })
                    .collect();
                if text_is_empty(&message.content) && tools.is_empty() {
                    continue;
                }
                sink.assistant(&message.id.0, &message.content, &tools);
            }
            Role::Tool => {
                let mut sub = 0usize;
                for block in &message.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                    } = block
                    {
                        sink.tool_result(&message.id.0, sub, tool_use_id, content);
                        sub += 1;
                    }
                }
            }
        }
    }
}

/// Fold a full committed step (with boundaries): `RunStarted`, the message
/// events, then a terminal event derived from `state`.
pub fn fold_step(
    new_messages: &[Message],
    state: &RunState,
    pending: Option<(&str, bool)>,
) -> Vec<Fact> {
    let mut out = vec![Fact::RunStarted];
    out.extend(fold_messages(new_messages, pending));
    out.push(terminal(state, pending));
    out
}

/// The `Awaiting` terminal event naming the pending tool. For callers that carry a
/// protocol stop reason rather than a [`RunState`].
pub fn terminal_awaiting(pending_tool_use_id: Option<&str>) -> Fact {
    Fact::Awaiting {
        pending_tool_use_id: pending_tool_use_id.map(str::to_string),
    }
}

/// The terminal projection event for a state. A fault projects as `RunFailed`
/// carrying its classification code, so hosts can tell a failed run from a
/// finished one without reading the committed state.
pub fn terminal(state: &RunState, pending: Option<(&str, bool)>) -> Fact {
    match state {
        // Not an end: a run committed mid-flight projects as its
        // in-progress signal. Hosts normally project only awaiting/ended phases.
        RunState::Running => Fact::RunStarted,
        RunState::Awaiting => Fact::Awaiting {
            pending_tool_use_id: pending.map(|p| p.0.to_string()),
        },
        RunState::Ended(EndCause::MaxSteps) => Fact::RunFinished { exhausted: true },
        RunState::Ended(EndCause::Error(failure)) => Fact::RunFailed {
            code: failure.code().to_string(),
            message: failure.message(),
        },
        // G26: indeterminate remote execution is explicit; it is never silently
        // converted to success.
        RunState::Ended(EndCause::Indeterminate) => Fact::RunFailed {
            code: "indeterminate".to_string(),
            message: "execution outcome could not be determined".to_string(),
        },
        RunState::Ended(_) => Fact::RunFinished { exhausted: false },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::Id;
    use crate::agent::run::Failure;
    use crate::event::classify::{Tier, classify};

    // PRODUCER AUTHORITY (ADR-0058 Axis 6): the fold produces *only* committed
    // whole-units / lifecycle facts — never a live `Delta`. This ties the producer
    // (fold) to the router (classify): every event any fold emits must classify as
    // the `Fact` tier. If a future edit made the fold emit a `Delta`, this fails.
    #[test]
    fn the_fold_emits_only_fact_tier_events() {
        let messages = vec![
            Message::text(Id("u1".into()), Role::User, "hi"),
            Message::new(
                Id("a1".into()),
                Role::Assistant,
                vec![
                    ContentBlock::text("sure"),
                    ContentBlock::tool_use("c1", "run", serde_json::json!({})),
                ],
            ),
            Message::new(
                Id("t1".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    "c1",
                    vec![ContentBlock::text("ok")],
                )],
            ),
        ];
        for state in [
            RunState::Running,
            RunState::Awaiting,
            RunState::Ended(EndCause::NaturalEnd),
            RunState::Ended(EndCause::MaxSteps),
            RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
        ] {
            for fact in fold_step(&messages, &state, Some(("c1", false))) {
                assert_eq!(
                    classify(&AgentEvent::Fact(fact.clone())).tier,
                    Tier::Fact,
                    "fold emitted a non-Fact-tier event: {fact:?}"
                );
            }
        }
    }

    #[test]
    fn error_terminal_projects_run_failed_with_the_fault_code() {
        let inference = RunState::Ended(EndCause::Error(Failure::Inference {
            code: "unauthorized".to_string(),
            message: "bad api key".to_string(),
        }));
        assert_eq!(
            terminal(&inference, None),
            Fact::RunFailed {
                code: "unauthorized".to_string(),
                message: "bad api key".to_string(),
            }
        );

        let capability = RunState::Ended(EndCause::Error(Failure::CapabilityBound));
        assert!(matches!(
            terminal(&capability, None),
            Fact::RunFailed { code, .. } if code == "capability_bound"
        ));
    }

    #[test]
    fn classifies_pending_client_tool() {
        let msg = Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "submit".into(),
                input: serde_json::json!({}),
            }],
        };
        let events = fold_messages(&[msg], Some(("c1", true)));
        assert_eq!(
            events,
            vec![Fact::ToolCall {
                id: "c1".into(),
                name: "submit".into(),
                input: serde_json::json!({}),
                disposition: ToolDisposition::PendingClient,
            }]
        );
    }

    #[test]
    fn step_wraps_with_start_and_terminal() {
        let msg = Message::text(Id("a1".into()), Role::Assistant, "hi");
        let events = fold_step(&[msg], &RunState::Ended(EndCause::NaturalEnd), None);
        assert_eq!(events.first(), Some(&Fact::RunStarted));
        assert_eq!(events.last(), Some(&Fact::RunFinished { exhausted: false }));
    }

    // G26: indeterminate remote execution is explicit; it is never silently
    // projected as success.
    #[test]
    fn indeterminate_projects_as_run_failed_not_run_finished() {
        let state = RunState::Ended(EndCause::Indeterminate);
        let event = terminal(&state, None);
        assert!(
            matches!(&event, Fact::RunFailed { code, .. } if code == "indeterminate"),
            "Indeterminate must project to RunFailed, got {event:?}"
        );
    }

    #[test]
    fn indeterminate_round_trips_through_serde() {
        let cause = EndCause::Indeterminate;
        let json = serde_json::to_string(&cause).unwrap();
        let parsed: EndCause = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, EndCause::Indeterminate);
    }

    // --- terminal(): the remaining state rows of the decision table ---

    #[test]
    fn running_state_projects_as_run_started_not_run_ended() {
        assert_eq!(terminal(&RunState::Running, None), Fact::RunStarted);
    }

    #[test]
    fn max_steps_projects_as_exhausted_run_finished() {
        assert_eq!(
            terminal(&RunState::Ended(EndCause::MaxSteps), None),
            Fact::RunFinished { exhausted: true }
        );
    }

    #[test]
    fn non_exhausting_ends_project_as_unexhausted_run_finished() {
        for cause in [
            EndCause::NaturalEnd,
            EndCause::Cancelled,
            EndCause::Stopped("budget".into()),
        ] {
            assert_eq!(
                terminal(&RunState::Ended(cause.clone()), None),
                Fact::RunFinished { exhausted: false },
                "{cause:?} must project as a non-exhausted finish"
            );
        }
    }

    #[test]
    fn awaiting_state_carries_the_pending_tool_id_or_none() {
        assert_eq!(
            terminal(&RunState::Awaiting, Some(("call-9", false))),
            Fact::Awaiting {
                pending_tool_use_id: Some("call-9".into())
            }
        );
        assert_eq!(
            terminal(&RunState::Awaiting, None),
            Fact::Awaiting {
                pending_tool_use_id: None
            }
        );
    }

    #[test]
    fn state_conflict_error_projects_its_code() {
        let state = RunState::Ended(EndCause::Error(Failure::StateConflict));
        assert!(matches!(
            terminal(&state, None),
            Fact::RunFailed { code, .. } if code == "state_conflict"
        ));
    }

    #[test]
    fn terminal_awaiting_helper_maps_the_id() {
        assert_eq!(
            terminal_awaiting(Some("c1")),
            Fact::Awaiting {
                pending_tool_use_id: Some("c1".into())
            }
        );
        assert_eq!(
            terminal_awaiting(None),
            Fact::Awaiting {
                pending_tool_use_id: None
            }
        );
    }

    // --- fold_messages(): role x pending x content rows ---

    fn assistant(id: &str, blocks: Vec<ContentBlock>) -> Message {
        Message::new(Id(id.into()), Role::Assistant, blocks)
    }

    #[test]
    fn assistant_text_only_emits_one_assistant_message() {
        let msg = assistant("a1", vec![ContentBlock::text("hello")]);
        let events = fold_messages(&[msg], None);
        assert_eq!(
            events,
            vec![Fact::AssistantMessage {
                id: "a1".into(),
                content: vec![ContentBlock::text("hello")],
            }]
        );
    }

    #[test]
    fn assistant_with_no_content_at_all_emits_nothing() {
        // No text blocks and no tool-use blocks => no events.
        let msg = assistant("a1", vec![]);
        assert!(fold_messages(&[msg], None).is_empty());
    }

    // INVARIANT: the two projections agree on the skip-empty test. An assistant
    // message whose only block is an empty-string Text is a useless empty wire
    // event, so BOTH the streaming fold (fold_messages) and the static-history
    // fold (fold_history) drop it. They share the one `text_is_empty` predicate,
    // so this parity holds by construction — flipping it flips this named test.
    #[test]
    fn assistant_all_empty_text_is_dropped_by_both_projections() {
        let msg = assistant("a1", vec![ContentBlock::text("")]);
        let events = fold_messages(&[msg], None);
        assert!(
            events.is_empty(),
            "streaming projection drops an all-empty-text assistant message: {events:?}"
        );

        let mut sink = RecordingSink::default();
        fold_history(&[assistant("a1", vec![ContentBlock::text("")])], &mut sink);
        assert!(
            sink.calls.is_empty(),
            "history fold drops the same all-empty-text assistant message"
        );
    }

    #[test]
    fn assistant_text_then_tool_call_preserves_order() {
        let msg = assistant(
            "a1",
            vec![
                ContentBlock::text("thinking"),
                ContentBlock::tool_use("c1", "run", serde_json::json!({})),
            ],
        );
        let events = fold_messages(&[msg], None);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], Fact::AssistantMessage { .. }));
        assert_eq!(
            events[1],
            Fact::ToolCall {
                id: "c1".into(),
                name: "run".into(),
                input: serde_json::json!({}),
                disposition: ToolDisposition::Executed,
            }
        );
    }

    #[test]
    fn pending_builtin_and_non_matching_pending_classify_correctly() {
        let msg = assistant(
            "a1",
            vec![ContentBlock::tool_use("c1", "run", serde_json::json!({}))],
        );
        // client=false => PendingBuiltin.
        let builtin = fold_messages(std::slice::from_ref(&msg), Some(("c1", false)));
        assert!(matches!(
            &builtin[0],
            Fact::ToolCall { disposition, .. } if *disposition == ToolDisposition::PendingBuiltin
        ));
        // A pending id that does not match this call => Executed.
        let executed = fold_messages(&[msg], Some(("other", true)));
        assert!(matches!(
            &executed[0],
            Fact::ToolCall { disposition, .. } if *disposition == ToolDisposition::Executed
        ));
    }

    #[test]
    fn tool_role_projects_tool_result_with_is_error_false() {
        let msg = Message::new(
            Id("t1".into()),
            Role::Tool,
            vec![ContentBlock::tool_result(
                "c1",
                vec![ContentBlock::text("ok")],
            )],
        );
        let events = fold_messages(&[msg], None);
        assert_eq!(
            events,
            vec![Fact::ToolResult {
                id: "c1".into(),
                content: vec![ContentBlock::text("ok")],
                is_error: false,
            }]
        );
    }

    #[test]
    fn user_and_system_messages_project_nothing() {
        let u = Message::text(Id("u1".into()), Role::User, "hi");
        let s = Message::text(Id("s1".into()), Role::System, "sys");
        assert!(fold_messages(&[u, s], None).is_empty());
    }

    // --- fold_history(): the shared static-history fold ---

    #[derive(Default)]
    struct RecordingSink {
        calls: Vec<String>,
    }
    impl HistorySink for RecordingSink {
        fn user_or_system(&mut self, id: &str, role: Role, content: &[ContentBlock]) {
            self.calls.push(format!(
                "us:{id}:{role:?}:{}",
                crate::agent::content::extract_text(content)
            ));
        }
        fn assistant(&mut self, id: &str, content: &[ContentBlock], tools: &[ToolUseRef<'_>]) {
            let names: Vec<&str> = tools.iter().map(|t| t.name).collect();
            self.calls.push(format!(
                "as:{id}:{}:{names:?}",
                crate::agent::content::extract_text(content)
            ));
        }
        fn tool_result(
            &mut self,
            message_id: &str,
            sub: usize,
            tool_use_id: &str,
            _content: &[ContentBlock],
        ) {
            self.calls
                .push(format!("tr:{message_id}:{sub}:{tool_use_id}"));
        }
    }

    #[test]
    fn history_fold_skips_empty_indexes_tool_results_and_extracts_tools() {
        let messages = vec![
            // Empty user message: skipped.
            Message::new(Id("u0".into()), Role::User, vec![ContentBlock::text("")]),
            // Real user message.
            Message::text(Id("u1".into()), Role::User, "ask"),
            // Empty assistant with no tools: skipped.
            Message::new(
                Id("a0".into()),
                Role::Assistant,
                vec![ContentBlock::text("")],
            ),
            // Assistant with a tool call but no text: still emitted.
            assistant(
                "a1",
                vec![ContentBlock::tool_use("c1", "run", serde_json::json!({}))],
            ),
            // Tool message with two results: sub 0 and 1.
            Message::new(
                Id("t1".into()),
                Role::Tool,
                vec![
                    ContentBlock::tool_result("c1", vec![ContentBlock::text("r0")]),
                    ContentBlock::tool_result("c2", vec![ContentBlock::text("r1")]),
                ],
            ),
        ];
        let mut sink = RecordingSink::default();
        fold_history(&messages, &mut sink);
        assert_eq!(
            sink.calls,
            vec![
                "us:u1:User:ask".to_string(),
                "as:a1::[\"run\"]".to_string(),
                "tr:t1:0:c1".to_string(),
                "tr:t1:1:c2".to_string(),
            ]
        );
    }

    // --- Transcoder::transcode_facts default method + tier dispatch ---

    struct Marker;
    impl Transcoder for Marker {
        type Output = &'static str;
        fn fact(&mut self, fact: &Fact) -> Vec<&'static str> {
            match fact {
                Fact::RunStarted => vec!["start"],
                Fact::RunFinished { .. } => vec!["fin"],
                _ => vec![],
            }
        }
        fn delta(&mut self, _delta: &Delta) -> Vec<&'static str> {
            vec!["delta"]
        }
    }

    #[test]
    fn transcode_facts_flat_maps_in_order() {
        let facts = vec![
            Fact::RunStarted,
            Fact::AssistantMessage {
                id: "a".into(),
                content: vec![],
            },
            Fact::RunFinished { exhausted: false },
        ];
        assert_eq!(Marker.transcode_facts(&facts), vec!["start", "fin"]);
    }

    #[test]
    fn transcode_dispatches_by_tier() {
        // A Fact routes to fact(); a Delta routes to the opt-in delta().
        assert_eq!(
            Marker.transcode(&AgentEvent::Fact(Fact::RunStarted)),
            vec!["start"]
        );
        assert_eq!(
            Marker.transcode(&AgentEvent::Delta(Delta::TextDelta { delta: "x".into() })),
            vec!["delta"]
        );
    }
}
