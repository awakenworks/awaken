//! Derived positions and primary action references for one published event log.
//! Unlike the canonicalizer's pre-sort Message joins, these positions describe
//! the final ordered vector and are installed with it at the existing cache CAS.

use std::collections::{HashMap, HashSet};

use crate::types::{Event, OutboundKind, StopReason};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::state) struct EventPageIndex {
    positions: HashMap<String, Vec<usize>>,
    pub(super) primary_answerable_ids: HashSet<String>,
    indexed_len: usize,
    #[cfg(test)]
    examined: usize,
}

impl EventPageIndex {
    pub(super) fn new(
        session_id: &str,
        events: &[Event],
        owners: &HashMap<String, String>,
    ) -> Self {
        let mut index = Self::default();
        index.extend(session_id, events, owners);
        index
    }

    pub(super) fn extend(
        &mut self,
        session_id: &str,
        events: &[Event],
        owners: &HashMap<String, String>,
    ) {
        debug_assert!(self.indexed_len <= events.len());
        for (position, event) in events.iter().enumerate().skip(self.indexed_len) {
            #[cfg(test)]
            {
                self.examined += 1;
            }
            // Retain every occurrence: cursor filtering and direction choose
            // the same first visible match as the canonical slice paginator.
            self.positions
                .entry(event.id.clone())
                .or_default()
                .push(position);
            if owners
                .get(&event.id)
                .is_some_and(|owner| owner != session_id)
            {
                continue;
            }
            match &event.kind {
                OutboundKind::SessionStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                }
                | OutboundKind::SessionThreadStatusIdle {
                    stop_reason: StopReason::RequiresAction { event_ids },
                    ..
                } => self
                    .primary_answerable_ids
                    .extend(event_ids.iter().cloned()),
                _ => {}
            }
        }
        self.indexed_len = events.len();
    }

    pub(super) fn positions(&self, id: &str) -> Option<&[usize]> {
        self.positions.get(id).map(Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: &str, kind: OutboundKind) -> Event {
        Event {
            id: id.into(),
            kind,
            processed_at: None,
        }
    }

    fn idle(id: &str, target: &str) -> Event {
        event(
            id,
            OutboundKind::SessionStatusIdle {
                stop_reason: StopReason::RequiresAction {
                    event_ids: vec![target.into()],
                },
            },
        )
    }

    #[test]
    fn incremental_extension_indexes_only_the_new_tail_and_preserves_owners() {
        // Cause/effect graph: C1 an initial vector contains an ordinary Event
        // and a primary action reference; C2 the candidate appends a duplicate
        // id and a child-owned action reference; C3 extend is called again
        // without another append. Effects: E1 all positions of the duplicate
        // id remain addressable; E2 only the primary reference is answerable;
        // E3 the indexed prefix advances exactly to vector length; E4 C3 is a
        // stutter and cannot duplicate positions or references.
        // Decision table: A1=C1=>E2,E3; A2=C1+C2=>E1-E3;
        // A3=C1+C2+C3=>E1-E4. Reorder/removal is deliberately excluded: its
        // production owner calls `rebuild_event_page_index` immediately.
        let mut events = vec![
            event("same", OutboundKind::AgentThinking {}),
            idle("primary-idle", "root-tool"),
        ];
        events.extend((0..1_000).map(|index| {
            event(
                &format!("unrelated-{index}"),
                OutboundKind::AgentThinking {},
            )
        }));
        let mut owners = HashMap::new();
        let mut index = EventPageIndex::new("session", &events, &owners);
        assert_eq!(index.positions("same"), Some([0].as_slice()), "A1/E3");
        assert_eq!(
            index.primary_answerable_ids,
            HashSet::from(["root-tool".into()]),
            "A1/E2"
        );
        assert_eq!(index.examined, 1_002, "A1/E3 initial one-pass build");

        events.push(event("same", OutboundKind::AgentThinking {}));
        events.push(idle("child-idle", "child-tool"));
        owners.insert("child-idle".into(), "child".into());
        index.extend("session", &events, &owners);
        assert_eq!(
            index.positions("same"),
            Some([0, 1_002].as_slice()),
            "A2/E1"
        );
        assert_eq!(index.indexed_len, events.len(), "A2/E3");
        assert_eq!(index.examined, 1_004, "A2/E3 only two tail Events");
        assert_eq!(
            index.primary_answerable_ids,
            HashSet::from(["root-tool".into()]),
            "A2/E2"
        );

        let stable = index.clone();
        index.extend("session", &events, &owners);
        assert_eq!(index, stable, "A3/E4");
    }
}
