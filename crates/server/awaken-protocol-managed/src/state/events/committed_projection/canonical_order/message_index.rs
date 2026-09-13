//! Request-local joins from committed Message identities to retained Events.

use super::*;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use std::collections::HashMap;

pub(super) struct MessageEventIndex<'a> {
    events: &'a [Event],
    by_id: HashMap<&'a str, Vec<usize>>,
    tools_by_source: HashMap<(&'a str, &'a str), Vec<(usize, &'a str)>>,
    roles: HashMap<&'static str, &'a OutboundKind>,
    #[cfg(test)]
    examined: std::cell::Cell<usize>,
}

impl<'a> MessageEventIndex<'a> {
    pub(super) fn new(events: &'a [Event]) -> Self {
        let mut index = Self {
            events,
            by_id: HashMap::new(),
            tools_by_source: HashMap::new(),
            roles: HashMap::new(),
            #[cfg(test)]
            examined: std::cell::Cell::new(0),
        };
        for (position, event) in events.iter().enumerate() {
            // Keep every retained occurrence. An index must not silently replace
            // a same-id Event carrying another type with the last occurrence.
            index.by_id.entry(&event.id).or_default().push(position);
            index.roles.insert(event.kind.type_str(), &event.kind);
            if let Some(source) = decode_managed_tool_event_id(&event.id) {
                index
                    .tools_by_source
                    .entry((source.thread_id, source.source_id))
                    .or_default()
                    .push((position, source.call_id));
            }
        }
        index
    }

    fn matches(
        &self,
        public_thread: &str,
        message: &Message,
        candidates: &HashMap<String, (usize, Option<&str>)>,
    ) -> HashMap<usize, usize> {
        let mut matches = HashMap::new();
        let mut calls = HashMap::new();
        for (ordinal, block) in message.content.iter().enumerate() {
            let call = match block {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            };
            if let Some(call) = call {
                calls.entry(call).or_insert(ordinal);
            }
        }
        if let Some(events) = self
            .tools_by_source
            .get(&(public_thread, message.id.0.as_str()))
        {
            for &(position, call_id) in events {
                #[cfg(test)]
                self.examined.set(self.examined.get() + 1);
                if let Some(&ordinal) = calls.get(call_id) {
                    matches.insert(position, ordinal);
                }
            }
        }
        for (id, &(ordinal, role)) in candidates {
            if let Some(events) = self.by_id.get(id.as_str()) {
                for &position in events {
                    #[cfg(test)]
                    self.examined.set(self.examined.get() + 1);
                    if role.is_none_or(|role| role == self.events[position].kind.type_str()) {
                        // The qualified tool source wins over generic/Assistant
                        // identity just as the former tool.or(message) did.
                        matches.entry(position).or_insert(ordinal);
                    }
                }
            }
        }
        matches
    }

    pub(super) fn index_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
        snapshot: &RunRecoverySnapshot,
        orders: &mut HashMap<String, CanonicalEventOrder>,
    ) {
        if snapshot.message_commit_cursors.len() != snapshot.messages.len() {
            return;
        }
        let run_ids = snapshot
            .runs
            .iter()
            .map(|run| run.id.clone())
            .collect::<Vec<_>>();
        let coordinates =
            ManagedState::assistant_response_coordinates(&snapshot.messages, &run_ids);
        let owner = public_thread_id(session_id, thread_id);
        for (message_index, (message, cursor)) in snapshot
            .messages
            .iter()
            .zip(snapshot.message_commit_cursors.iter().copied())
            .enumerate()
        {
            // Reuse the one identity encoder and per-Message ordinal bound. The
            // candidate set depends on message blocks/types, never history size.
            let bound = message_projection_ordinal_bound(message);
            let mut candidates = HashMap::with_capacity(bound.saturating_mul(self.roles.len()));
            for role in self.roles.keys() {
                for ordinal in 0..bound {
                    candidates.insert(
                        managed_multiagent_event_id(
                            session_id,
                            thread_id,
                            role,
                            ManagedMultiagentEventProvenance::Message {
                                message_id: &message.id.0,
                                ordinal,
                            },
                        ),
                        (ordinal, None),
                    );
                }
            }
            if let Some((run_id, step, response)) = coordinates.get(&message.id.0) {
                for (role, kind) in &self.roles {
                    let ordinal = match kind {
                        OutboundKind::AgentMessage { .. } => usize::from(
                            message
                                .content
                                .iter()
                                .any(|block| matches!(block, ContentBlock::Thinking { .. })),
                        ),
                        _ => 0,
                    };
                    candidates
                        .entry(managed_assistant_event_id(
                            session_id, thread_id, run_id, *step, *response, role,
                        ))
                        .or_insert((ordinal, Some(*role)));
                }
            }
            for (position, ordinal) in self.matches(&owner, message, &candidates) {
                retain_earliest_order(
                    orders,
                    self.events[position].id.clone(),
                    CanonicalEventOrder {
                        processed_at_unix_ms: snapshot.commit_timestamps.get(&cursor).copied(),
                        source_commit_cursor: cursor,
                        phase: 50,
                        ordinal: message_index
                            .saturating_mul(self.events.len().saturating_add(1))
                            .saturating_add(ordinal),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::managed_tool_event_id;
    use awaken_agent_contract::agent::message::{Id as MessageId, Role};

    fn event(id: String, kind: OutboundKind) -> Event {
        Event {
            id,
            kind,
            processed_at: None,
        }
    }
    fn tool(id: String) -> Event {
        event(
            id,
            OutboundKind::AgentToolUse {
                name: "read".into(),
                input: serde_json::json!({}),
                evaluated_permission: None,
                evaluation: None,
                session_thread_id: None,
            },
        )
    }

    #[test]
    fn matching_visits_only_indexed_sources_and_preserves_tool_precedence() {
        // C1 exact Thread+Message+call; C2 other Thread/Message; C3 duplicate
        // call blocks; C4 generic id also matches; C5 unrelated history grows.
        // R1 C1 => first matching block; R2 C2 => excluded; R3 C3 => first
        // block retains the original .position semantics; R4 C1+C4 => tool
        // ordinal wins (a conservative overlap row; current encoders separate
        // the prefixes); R5 C5 => Event inspections stay constant after the
        // once-per-candidate index build. The counter observes real inspected
        // candidates, so reintroducing a history scan is a measurable regression.
        let message = Message::new(
            MessageId("message".into()),
            Role::Assistant,
            vec![
                ContentBlock::text("text"),
                ContentBlock::tool_use("call", "read", serde_json::json!({})),
                ContentBlock::tool_result("call", vec![]),
            ],
        );
        for unrelated in [0, 10, 1000] {
            let matched = managed_tool_event_id("primary", "message", "call");
            let mut events = vec![
                tool(matched.clone()),
                tool(managed_tool_event_id("other-thread", "message", "call")),
                tool(managed_tool_event_id("primary", "other-message", "call")),
                tool("mtool_v1_broken".into()),
            ];
            events.extend((0..unrelated).map(|i| {
                tool(managed_tool_event_id(
                    "primary",
                    &format!("old-{i}"),
                    "call",
                ))
            }));
            let index = MessageEventIndex::new(&events);
            let candidates = HashMap::from([(matched, (0, None))]);
            assert_eq!(
                index.matches("primary", &message, &candidates),
                HashMap::from([(0, 1)])
            );
            assert_eq!(
                index.examined.get(),
                2,
                "R5: one tool and one generic candidate"
            );
        }
    }

    #[test]
    fn matching_keeps_duplicate_ids_and_exact_assistant_type_qualification() {
        // C1 same retained id appears under two types; C2 Assistant-qualified
        // vs generic provenance. R6 C1+qualified => only the matching type;
        // R7 C1+generic => both, preserving generic-before-Assistant behavior.
        // A single-value id map would overwrite the good earlier occurrence.
        let events = vec![
            event(
                "same".into(),
                OutboundKind::AgentMessage { content: vec![] },
            ),
            event("same".into(), OutboundKind::AgentThinking {}),
        ];
        let message = Message::text(MessageId("message".into()), Role::Assistant, "value");
        let index = MessageEventIndex::new(&events);
        let qualified = HashMap::from([("same".into(), (1, Some("agent.message")))]);
        assert_eq!(
            index.matches("primary", &message, &qualified),
            HashMap::from([(0, 1)])
        );
        let generic = HashMap::from([("same".into(), (0, None))]);
        assert_eq!(
            index.matches("primary", &message, &generic),
            HashMap::from([(0, 0), (1, 0)])
        );
    }
    #[test]
    fn snapshot_index_retains_source_coordinates_and_withholds_unfenced_messages() {
        // R8 exact message/cursor alignment => original opaque id and immutable
        // commit/time/order coordinates. R9 missing cursor => no invented order.
        // Exercise the real candidate encoder path, not just supplied lookup ids.
        let run = awaken_agent_contract::agent::run::Id("run".into());
        let message = Message::text(MessageId("legacy-message".into()), Role::Assistant, "reply");
        let id = managed_multiagent_event_id(
            "session",
            "session",
            "agent.message",
            ManagedMultiagentEventProvenance::Message {
                message_id: &message.id.0,
                ordinal: 0,
            },
        );
        let events = vec![event(
            id.clone(),
            OutboundKind::AgentMessage { content: vec![] },
        )];
        let mut snapshot = RunRecoverySnapshot {
            thread_id: awaken_agent_contract::agent::thread::Id("session".into()),
            claimed_run_id: run.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run.clone(),
                thread_id: awaken_agent_contract::agent::thread::Id("session".into()),
                state: awaken_agent_contract::agent::run::RunState::Running,
            }],
            latest_run_id: Some(run),
            messages: vec![message],
            message_commit_cursors: vec![7],
            commit_timestamps: [(7, 1800000000123)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 7,
            next_commit_ordinal: 1,
        };
        let index = MessageEventIndex::new(&events);
        let mut orders = HashMap::new();
        index.index_snapshot("session", "session", &snapshot, &mut orders);
        assert_eq!(orders.len(), 1);
        let order = orders[&id];
        assert_eq!(
            (order.source_commit_cursor, order.phase, order.ordinal),
            (7, 50, 0)
        );
        assert_eq!(order.processed_at_unix_ms, Some(1800000000123));
        snapshot.message_commit_cursors.clear();
        orders.clear();
        index.index_snapshot("session", "session", &snapshot, &mut orders);
        assert!(
            orders.is_empty(),
            "R9 no fallback from missing source evidence"
        );
    }
}
