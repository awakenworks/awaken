// @formal surface obligation=session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal reason: The five-way position lookup of one projection candidate, moved unchanged from the
// @formal reason: Managed canonical ordering: inbound admission fence, tool occurrence, derived fact,
// @formal reason: coordination plan and parent terminal after the evidence frontier. Pure over the
// @formal reason: committed OrderIndex; it authorizes nothing and an unplaced event stays unpositioned so
// @formal reason: the adapter fails the candidate closed. Warm/cold and timeline scenario tests cover it.
//! The position of every projected event in the Session's total order.
//!
//! A protocol adapter renders each derived fact, inbound receipt and tool
//! occurrence under its own public id, names the committed identity behind
//! each one, and asks this module where it sits. The five lookups (inbound
//! admission, tool occurrence, derived fact, coordination plan and parent
//! terminal) resolve over one committed prefix; a published position is never
//! an input. Overlays, public times and the final sort stay with the adapter.
//!
//! Consumer: the Managed adapter's canonical ordering of one candidate.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::{
    CoordinationEvents, EventIdentity, EventPosition, FactProvenance, ManagedEventType, OrderIndex,
    ProjectionError,
};

/// The committed identity one projected event was minted from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventAnchor<'a> {
    /// A process-local overlay. It owns no committed position.
    Transient,
    /// An accepted inbound command, by its operation id.
    Inbound { operation_id: &'a str },
    /// A tool use or result, by its internal Thread, source and Runtime call.
    Tool {
        thread_id: String,
        source_id: &'a str,
        call_id: &'a str,
    },
    /// A derived fact, by its identity.
    Fact(&'a EventIdentity),
    /// No committed identity is known for this event.
    Unanchored,
}

/// One projected event: the caller's id for it, the wire type it was encoded
/// as and the identity it was minted from.
#[derive(Clone, Debug)]
pub struct AnchoredEvent<'a, Id> {
    pub id: Id,
    /// A transcript fold's identity authenticates the type it was encoded as.
    pub wire_type: &'a str,
    pub anchor: EventAnchor<'a>,
}

fn retain_earliest<K: Hash + Eq>(
    orders: &mut HashMap<K, EventPosition>,
    key: K,
    order: EventPosition,
) {
    orders
        .entry(key)
        .and_modify(|current| *current = (*current).min(order))
        .or_insert(order);
}

/// The position of one tool occurrence or derived fact. An assistant response
/// fold is positioned only as the type it was minted for, a Message fold only
/// while the projection still carries a type of its role.
fn keyed_order(
    index: &OrderIndex<'_>,
    event: &AnchoredEvent<'_, impl Sized>,
    present_types: &HashSet<&str>,
) -> Result<Option<EventPosition>, ProjectionError> {
    Ok(match &event.anchor {
        EventAnchor::Tool {
            thread_id,
            source_id,
            call_id,
        } => index.tool_order(thread_id, source_id, call_id)?,
        EventAnchor::Fact(key) => {
            let authenticated = match (&key.role, &key.provenance) {
                (ManagedEventType::Transcript(role), FactProvenance::AssistantResponse { .. }) => {
                    event.wire_type == role.as_wire_type()
                }
                (ManagedEventType::Transcript(role), _) => {
                    present_types.contains(role.as_wire_type())
                }
                _ => true,
            };
            authenticated.then(|| index.fact_order(key)).flatten()
        }
        EventAnchor::Transient | EventAnchor::Inbound { .. } | EventAnchor::Unanchored => None,
    })
}

/// Position every committed event of one candidate.
///
/// Inbound receipts sit at their admission fence, tool occurrences at their
/// committed Message or Awaiting audit, derived facts at the commit their
/// identity names, coordination facts where the coordination plan put them,
/// and parent-terminal facts at the terminal source once every other fact of
/// the prefix is placed. An event the prefix cannot place has no entry; the
/// caller fails the candidate closed.
pub fn position_events<Id: Clone + Eq + Hash>(
    index: &OrderIndex<'_>,
    coordination: &CoordinationEvents,
    events: &[AnchoredEvent<'_, Id>],
) -> Result<HashMap<Id, EventPosition>, ProjectionError> {
    let inbound =
        index
            .inbound_orders()
            .fold(HashMap::new(), |mut orders, (operation_id, order)| {
                retain_earliest(&mut orders, operation_id, order);
                orders
            });
    let present_types = events
        .iter()
        .map(|event| event.wire_type)
        .collect::<HashSet<_>>();
    let mut orders = HashMap::<Id, EventPosition>::new();
    let mut coordination_facts = Vec::new();
    let mut parent_terminal_facts = Vec::new();
    for event in events {
        match &event.anchor {
            EventAnchor::Transient => {}
            EventAnchor::Inbound { operation_id } => {
                if let Some(order) = inbound.get(operation_id) {
                    orders.insert(event.id.clone(), *order);
                }
            }
            EventAnchor::Fact(key) => match &key.provenance {
                FactProvenance::LinkOperation { .. }
                | FactProvenance::CoordinationCall { .. }
                | FactProvenance::AgentReport { .. } => {
                    coordination_facts.push((event.id.clone(), *key));
                }
                FactProvenance::ParentTerminal => {
                    parent_terminal_facts.push((event.id.clone(), *key));
                }
                _ => {
                    if let Some(order) = keyed_order(index, event, &present_types)? {
                        orders.insert(event.id.clone(), order);
                    }
                }
            },
            EventAnchor::Tool { .. } | EventAnchor::Unanchored => {
                if let Some(order) = keyed_order(index, event, &present_types)? {
                    orders.insert(event.id.clone(), order);
                }
            }
        }
    }
    for (id, key) in &coordination_facts {
        if let Some(order) = coordination.coordination_orders.get(key) {
            orders.insert(id.clone(), *order);
        }
    }
    let coordination_frontier = coordination
        .coordination_orders
        .values()
        .map(|order| order.source_commit_cursor)
        .max()
        .unwrap_or_default();

    // Initial activation can fail before Runtime owns a cleanup cursor. In
    // that case terminality follows the complete immutable prefix already
    // ordered above. The derived maximum is disposable ordering, never a
    // second persisted cursor or Session state.
    let frontier = orders
        .values()
        .map(|order| order.source_commit_cursor)
        .chain([index.evidence_frontier(), coordination_frontier])
        .max()
        .unwrap_or_default();
    if let Some(terminal_source) = index.parent_terminal_source(frontier) {
        for (id, key) in &parent_terminal_facts {
            if let Some(order) = index.parent_terminal_order(key, terminal_source) {
                orders.insert(id.clone(), order);
            }
        }
    }
    Ok(orders)
}
