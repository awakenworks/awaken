// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: Parent-to-child coordination facts and their order, moved from the Managed projector:
// @formal reason: the first eligible receipt of each `send_message` call, the exact link it targets,
// @formal reason: max(source,result) commit anchoring and terminal-report ordering. Pure over the
// @formal reason: published tool calls/results and committed evidence. Adjacent decision tables and
// @formal reason: warm/cold late-result integration tests cover behavior; reducer TCB linked to the
// @formal reason: paired-publication obligation, not a proof of joins or canonicalizer equivalence.
//! Coordination between the primary Thread and its coordinated children.
//!
//! A parent `send_message` call becomes a cross-Thread message once its first
//! eligible tool result accepts it for one exact child. These rules read the
//! calls and results the protocol already published (their public identity is
//! the protocol's), and decide which coordination facts exist and where each
//! sits in the canonical order.
//!
//! Consumer: the protocol adapter's delegation projector and canonical sort.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_execution_contract::{CoordinatedThreadLink, CoordinatedThreadTarget};

use crate::lifecycle::{CoordinatedThread, ThreadStatus};
use crate::{
    CanonicalOrder, FactKey, FactKind, FactProvenance, FactRole, OrderIndex, TimelineFact,
};

/// One published tool event, seen neutrally.
///
/// Consumer: every coordination rule here. The protocol builds these views
/// from its published events in publication order.
#[derive(Clone, Copy, Debug)]
pub enum IssuedToolEvent<'a> {
    /// A built-in tool use (not a custom or MCP one).
    Use {
        event_id: &'a str,
        /// The Runtime call id the public event id qualifies.
        call_id: &'a str,
        /// The (internal) Thread and source Message the id qualifies, when
        /// the id is a qualified tool identity.
        source: Option<(&'a str, &'a str)>,
        name: &'a str,
        input: &'a serde_json::Value,
        /// Shown only on a coordinated child's stream.
        owned: bool,
    },
    /// A built-in tool result naming the public tool-use event it answers.
    Result {
        event_id: &'a str,
        tool_use_id: &'a str,
        content: &'a [ContentBlock],
        is_error: Option<bool>,
        owned: bool,
    },
}

/// The model-facing id of the coordination send tool. The builtin
/// coordination tool extension owns the tool and its id; the timeline only
/// classifies committed calls by it, and the Managed adapter's tests pin the
/// two spellings equal.
pub const SEND_MESSAGE_TOOL_ID: &str = "send_message";

/// Whether a tool call is the coordination send.
#[must_use]
pub fn is_coordination_send(name: &str) -> bool {
    name == SEND_MESSAGE_TOOL_ID
}

fn content_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Disposable index of the first primary-stream result of each requested
/// call and the child Thread it accepted, if any.
pub struct CoordinationReceipts {
    by_call: HashMap<String, Option<String>>,
    #[cfg(test)]
    parsed: usize,
}

impl CoordinationReceipts {
    /// Index the first non-error primary-stream result of every call in
    /// `wanted`. A malformed or rejecting first result records no receipt;
    /// a later valid result can never widen that decision.
    #[must_use]
    pub fn for_calls(events: &[IssuedToolEvent<'_>], wanted: &HashSet<&str>) -> Self {
        let mut by_call = HashMap::new();
        #[cfg(test)]
        let mut parsed = 0;
        if !wanted.is_empty() {
            for event in events {
                let IssuedToolEvent::Result {
                    tool_use_id,
                    content,
                    is_error,
                    owned: false,
                    ..
                } = event
                else {
                    continue;
                };
                if *is_error == Some(true) || !wanted.contains(tool_use_id) {
                    continue;
                }
                by_call.entry(tool_use_id.to_string()).or_insert_with(|| {
                    #[cfg(test)]
                    {
                        parsed += 1;
                    }
                    let receipt =
                        serde_json::from_str::<serde_json::Value>(&content_text(content)).ok()?;
                    if receipt.get("accepted").and_then(serde_json::Value::as_bool) != Some(true) {
                        return None;
                    }
                    receipt
                        .get("session_thread_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            }
        }
        Self {
            by_call,
            #[cfg(test)]
            parsed,
        }
    }

    /// The child Thread a call's receipt accepted.
    #[must_use]
    pub fn get(&self, call: &str) -> Option<&str> {
        self.by_call.get(call)?.as_deref()
    }
}

/// The creation and opening of every newly linked child.
#[must_use]
pub fn link_facts(link: &CoordinatedThreadLink) -> [TimelineFact; 2] {
    let at = |role| {
        FactKey::new(
            link.thread_id.0.as_str(),
            role,
            FactProvenance::LinkOperation {
                operation_id: link.created_by_operation_id.clone(),
            },
        )
    };
    [
        TimelineFact::new(at(FactRole::LinkThreadCreated), FactKind::ThreadCreated),
        TimelineFact::new(at(FactRole::LinkStatusRunning), FactKind::ThreadRunning),
    ]
}

/// Lower accepted parent `send_message` calls into one cross-Thread message
/// each. A spawn is bound by the operation id that created the durable link;
/// a follow-up by the existing Thread id. A failed result never creates a
/// message merely because its target link already exists; a call whose link
/// is not visible yet stays eligible for a later prefix. `published` answers
/// whether a fact's key already names a published event.
pub fn cross_posts(
    events: &[IssuedToolEvent<'_>],
    links: &[CoordinatedThreadLink],
    threads: &mut [CoordinatedThread],
    published: impl Fn(&FactKey) -> bool,
) -> Vec<TimelineFact> {
    if links.is_empty() {
        return Vec::new();
    }
    let primary_sends = || {
        events.iter().filter_map(|event| match event {
            IssuedToolEvent::Use {
                event_id,
                call_id,
                name,
                input,
                owned: false,
                ..
            } if is_coordination_send(name) => Some((*event_id, *call_id, *input)),
            _ => None,
        })
    };
    let wanted = primary_sends().map(|(event_id, _, _)| event_id).collect();
    let receipts = CoordinationReceipts::for_calls(events, &wanted);
    let mut links_by_thread = HashMap::<&str, Vec<&CoordinatedThreadLink>>::new();
    for link in links {
        links_by_thread
            .entry(&link.thread_id.0)
            .or_default()
            .push(link);
    }
    let calls = primary_sends()
        .filter_map(|(event_id, call_id, input)| {
            let receipt_thread = receipts.get(event_id)?;
            let link = links_by_thread
                .get(receipt_thread)?
                .iter()
                .copied()
                .find(|link| {
                    if link.thread_id.0 != receipt_thread {
                        return false;
                    }
                    match (
                        input
                            .get("agent_id")
                            .and_then(serde_json::Value::as_str)
                            .filter(|value| !value.trim().is_empty()),
                        input
                            .get("session_thread_id")
                            .and_then(serde_json::Value::as_str)
                            .filter(|value| !value.trim().is_empty()),
                    ) {
                        (Some(agent_id), None) => {
                            link.target.agent_id() == Some(agent_id)
                                && link
                                    .created_by_operation_id
                                    .strip_suffix(call_id)
                                    .is_some_and(|prefix| prefix.ends_with(':'))
                        }
                        (None, Some(thread_id)) => link.thread_id.0 == thread_id,
                        _ => false,
                    }
                })?;
            let message = input
                .get("message")
                .and_then(serde_json::Value::as_str)?
                .to_owned();
            Some((event_id, link.thread_id.0.as_str(), message))
        })
        .collect::<Vec<_>>();
    let mut facts = Vec::new();
    let mut emitted = HashSet::new();
    for (event_id, thread_id, message) in calls {
        let at = |role| {
            FactKey::new(
                thread_id,
                role,
                FactProvenance::CoordinationCall {
                    event_id: event_id.to_string(),
                },
            )
        };
        let sent = at(FactRole::CoordinationMessageSent);
        // The deterministic public fact is the sole projection receipt.
        if published(&sent) || emitted.contains(&sent) {
            continue;
        }
        let Some(child) = threads.iter_mut().find(|child| child.thread == thread_id) else {
            continue;
        };
        if child.status == ThreadStatus::Terminated {
            continue;
        }
        if child.status != ThreadStatus::Running {
            child.status = ThreadStatus::Running;
            let running = at(FactRole::CoordinationStatusRunning);
            if !published(&running) && emitted.insert(running.clone()) {
                facts.push(TimelineFact::new(running, FactKind::ThreadRunning));
            }
        }
        emitted.insert(sent.clone());
        facts.push(TimelineFact::new(sent, FactKind::MessageSent { message }));
    }
    facts
}

fn accepted_order(
    receipts: &CoordinationReceipts,
    results: &HashMap<&str, CanonicalOrder>,
    orders: &HashMap<&str, CanonicalOrder>,
    source: &str,
    thread: &str,
) -> Option<CanonicalOrder> {
    if receipts.get(source) != Some(thread) {
        return None;
    }
    Some(orders.get(source).copied()?.max(*results.get(source)?))
}

fn retain_earliest(
    orders: &mut HashMap<FactKey, CanonicalOrder>,
    key: FactKey,
    order: CanonicalOrder,
) {
    orders
        .entry(key)
        .and_modify(|current| *current = (*current).min(order))
        .or_insert(order);
}

/// The canonical order of every coordination fact the evidence anchors: a
/// link at its accepted spawn (or Advisor consultation), a follow-up at its
/// accepted call, a report at its committed report. `orders` holds the order
/// of every already-ordered published event, by public id.
#[must_use]
pub fn coordination_orders(
    index: &OrderIndex<'_>,
    events: &[IssuedToolEvent<'_>],
    links: &[CoordinatedThreadLink],
    orders: &HashMap<&str, CanonicalOrder>,
) -> HashMap<FactKey, CanonicalOrder> {
    let mut candidates = HashMap::new();
    if links.is_empty() {
        return candidates;
    }
    let advisor_operation_orders = index.advisor_operation_orders();
    let mut sources_by_operation = HashMap::new();
    for event in events {
        if let IssuedToolEvent::Use {
            event_id,
            call_id,
            source: Some((thread, message)),
            ..
        } = event
            && let Some(operation) = index.tool_operation(thread, message, call_id)
        {
            sources_by_operation.entry(operation).or_insert(*event_id);
        }
    }
    let sends = || {
        events.iter().filter_map(|event| match event {
            IssuedToolEvent::Use { event_id, name, .. } if is_coordination_send(name) => {
                Some(*event_id)
            }
            _ => None,
        })
    };
    let mut wanted = sends().collect::<HashSet<_>>();
    for link in links {
        if matches!(link.target, CoordinatedThreadTarget::Agent { .. })
            && let Some(source) = sources_by_operation.get(&link.created_by_operation_id)
        {
            wanted.insert(source);
        }
    }
    let receipts = CoordinationReceipts::for_calls(events, &wanted);
    let mut result_orders = HashMap::new();
    for event in events {
        if let IssuedToolEvent::Result {
            event_id,
            tool_use_id,
            is_error,
            ..
        } = event
            && *is_error != Some(true)
            && wanted.contains(tool_use_id)
            && let Some(order) = orders.get(event_id)
        {
            result_orders.entry(*tool_use_id).or_insert(*order);
        }
    }
    let mut calls_by_thread = HashMap::<&str, Vec<&str>>::new();
    for source in sends() {
        if let Some(thread) = receipts.get(source) {
            calls_by_thread.entry(thread).or_default().push(source);
        }
    }
    let mut terminals = HashMap::<&str, Vec<&RunLifecycleEvent>>::new();
    for event in index.lifecycle() {
        if matches!(
            event.kind,
            RunLifecycleEventKind::Completed
                | RunLifecycleEventKind::Failed
                | RunLifecycleEventKind::Cancelled
        ) {
            terminals.entry(&event.thread_id.0).or_default().push(event);
        }
    }

    for link in links {
        let thread = link.thread_id.0.as_str();
        let source_order = match &link.target {
            CoordinatedThreadTarget::Agent { .. } => sources_by_operation
                .get(&link.created_by_operation_id)
                .and_then(|source| {
                    accepted_order(&receipts, &result_orders, orders, source, thread)
                }),
            CoordinatedThreadTarget::Advisor { .. } => advisor_operation_orders
                .get(&link.created_by_operation_id)
                .copied(),
        };
        if let Some(source_order) = source_order {
            for role in [FactRole::LinkThreadCreated, FactRole::LinkStatusRunning] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::LinkOperation {
                            operation_id: link.created_by_operation_id.clone(),
                        },
                    ),
                    CanonicalOrder {
                        phase: role.phase(),
                        ..source_order
                    },
                );
            }
        }
        for source in calls_by_thread.get(thread).into_iter().flatten().copied() {
            let Some(source_order) =
                accepted_order(&receipts, &result_orders, orders, source, thread)
            else {
                continue;
            };
            for role in [
                FactRole::CoordinationStatusRunning,
                FactRole::CoordinationMessageSent,
            ] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::CoordinationCall {
                            event_id: source.to_string(),
                        },
                    ),
                    CanonicalOrder {
                        phase: role.phase(),
                        ..source_order
                    },
                );
            }
        }
        for terminal in terminals.get(thread).into_iter().flatten().copied() {
            let Some(order) = index.report_order(terminal) else {
                continue;
            };
            for role in [
                FactRole::ChildMessageReceived,
                FactRole::AdvisorMessageReceived,
            ] {
                retain_earliest(
                    &mut candidates,
                    FactKey::new(
                        thread,
                        role,
                        FactProvenance::AgentReport {
                            run_id: terminal.run_id.0.clone(),
                        },
                    ),
                    order,
                );
            }
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result<'a>(
        event_id: &'a str,
        call: &'a str,
        content: &'a [ContentBlock],
        is_error: bool,
        owned: bool,
    ) -> IssuedToolEvent<'a> {
        IssuedToolEvent::Result {
            event_id,
            tool_use_id: call,
            content,
            is_error: Some(is_error),
            owned,
        }
    }

    #[test]
    fn receipt_index_preserves_first_primary_result_and_never_promotes_later_text() {
        // C1 result belongs to primary vs child; C2 failed vs non-error;
        // C3 first eligible JSON is malformed/rejected/accepted; C4 a later
        // eligible result is accepted; C5 caller did not request this call.
        // R1 !C1 or C2 => skip; R2 first malformed/rejected + C4 => still none;
        // R3 first accepted => its exact Thread; R4 C5 => no stored result.
        let accepted = [ContentBlock::text(
            r#"{"accepted":true,"session_thread_id":"child"}"#,
        )];
        let malformed = [ContentBlock::text("not json")];
        let rejected = [ContentBlock::text(r#"{"accepted":false}"#)];
        let events = vec![
            result("owned", "a", &accepted, false, true),
            result("bad", "a", &malformed, false, false),
            result("later", "a", &accepted, false, false),
            result("error", "b", &accepted, true, false),
            result("good", "b", &accepted, false, false),
            result("rejected", "c", &rejected, false, false),
            result("later-c", "c", &accepted, false, false),
            result("ignored", "unused", &accepted, false, false),
        ];
        let index = CoordinationReceipts::for_calls(&events, &HashSet::from(["a", "b", "c"]));
        assert_eq!(index.get("a"), None);
        assert_eq!(index.get("b"), Some("child"));
        assert_eq!(index.get("c"), None);
        assert_eq!(index.get("unused"), None);
        assert_eq!(index.by_call.len(), 3);
        assert_eq!(
            index.parsed, 3,
            "only first eligible requested receipts are decoded"
        );
        assert!(
            CoordinationReceipts::for_calls(&events, &HashSet::new())
                .by_call
                .is_empty()
        );
    }

    #[test]
    fn accepted_order_requires_the_exact_receipt_and_both_committed_anchors() {
        // C1 primary non-error receipt accepts the exact child; C2 source has
        // an anchor; C3 result has an anchor; C4 result committed later despite
        // an earlier wall clock. R1 all C1-C3 => max COMMIT order; R2 !C1 or
        // !C2 or !C3 => no manufactured coordination order; R3 C4 => result's
        // commit/time stays paired.
        let content = [ContentBlock::text(
            r#"{"accepted":true,"session_thread_id":"child"}"#,
        )];
        let events = [result("result", "source", &content, false, false)];
        let receipts = CoordinationReceipts::for_calls(&events, &HashSet::from(["source"]));
        let source_order = CanonicalOrder {
            source_commit_cursor: 2,
            phase: 50,
            ordinal: 0,
            processed_at_unix_ms: Some(200),
        };
        let result_order = CanonicalOrder {
            source_commit_cursor: 7,
            phase: 50,
            ordinal: 1,
            processed_at_unix_ms: Some(100),
        };
        let orders = HashMap::from([("source", source_order)]);
        let results = HashMap::from([("source", result_order)]);
        let accepted = accepted_order(&receipts, &results, &orders, "source", "child").unwrap();
        assert_eq!(accepted.source_commit_cursor, 7);
        assert_eq!(accepted.processed_at_unix_ms, Some(100));
        assert!(accepted_order(&receipts, &results, &orders, "source", "foreign").is_none());
        assert!(accepted_order(&receipts, &HashMap::new(), &orders, "source", "child").is_none());
        assert!(accepted_order(&receipts, &results, &HashMap::new(), "source", "child").is_none());
    }
}
