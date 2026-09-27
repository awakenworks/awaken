//! Derived positions and primary action references for one published event log.
//! Unlike the canonicalizer's pre-sort Message joins, these positions describe
//! the final ordered vector and are installed with it at the existing cache CAS.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::types::{Event, OutboundKind, StopReason};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::state) struct EventPageIndex {
    positions: HashMap<String, Vec<usize>>,
    pub(super) primary_answerable_ids: HashSet<String>,
    indexed_len: usize,
    /// Last delivery change for each retained id. This is an index into the
    /// sole Event vector, not a replay log or another payload collection.
    delivery_changes: BTreeMap<(u64, usize), String>,
    delivery_positions: HashMap<String, (u64, usize)>,
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

    pub(super) fn inherit_delivery_changes(
        &mut self,
        previous: &Self,
        revision: u64,
        delta: &[Event],
    ) {
        self.delivery_changes = previous
            .delivery_changes
            .iter()
            .filter(|(_, id)| self.positions.contains_key(id.as_str()))
            .map(|(position, id)| (*position, id.clone()))
            .collect();
        self.delivery_positions = previous
            .delivery_positions
            .iter()
            .filter(|(id, _)| self.positions.contains_key(id.as_str()))
            .map(|(id, position)| (id.clone(), *position))
            .collect();
        for (ordinal, event) in delta.iter().enumerate() {
            let position = (revision, ordinal);
            if let Some(old) = self.delivery_positions.insert(event.id.clone(), position) {
                self.delivery_changes.remove(&old);
            }
            self.delivery_changes.insert(position, event.id.clone());
        }
    }

    pub(super) fn delivery_page(
        &self,
        after: (u64, usize),
        limit: usize,
    ) -> Vec<((u64, usize), usize)> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.delivery_changes
            .range((Excluded(after), Unbounded))
            .filter_map(|(cursor, id)| {
                self.positions(id)
                    .and_then(|positions| positions.first())
                    .map(|position| (*cursor, *position))
            })
            .take(limit)
            .collect()
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

    #[test]
    fn delivery_index_retains_only_latest_changes_and_pages_past_large_history() {
        // Causes: C1 300 Events change at revision 2; C2 one old id completes
        // at revision 3; C3 the vector reorders or removes an id; C4 page bound
        // is zero/one/128/MAX and cursor is before/equal/after a revision.
        // Effects: I1 all changes are ordered once and pages terminate;
        // I2 completion is visible even behind the former positional cursor;
        // I3 reorder resolves current positions, removal leaves no ghost;
        // I4 at most one index entry per id, independent of revisions retained.
        // Rules I1=C1+C4, I2=C1+C2+C4, I3=C3, I4=all rows.
        let owners = HashMap::new();
        let mut events = (0..300)
            .map(|n| event(&format!("id-{n}"), OutboundKind::AgentThinking {}))
            .collect::<Vec<_>>();
        let empty = EventPageIndex::default();
        let mut first = EventPageIndex::new("s", &events, &owners);
        first.inherit_delivery_changes(&empty, 2, &events);
        for limit in [0, 1, 128, usize::MAX] {
            let page = first.delivery_page((1, usize::MAX), limit);
            assert_eq!(page.len(), 300.min(limit), "I1 bound {limit}");
            assert!(
                first.delivery_page((2, usize::MAX), limit).is_empty(),
                "I1 equality"
            );
        }
        let mut cursor = (1, usize::MAX);
        let mut visited = Vec::new();
        loop {
            let page = first.delivery_page(cursor, 128);
            if page.is_empty() {
                break;
            }
            cursor = page.last().unwrap().0;
            visited.extend(page.into_iter().map(|(_, position)| position));
        }
        assert_eq!(
            visited,
            (0..300).collect::<Vec<_>>(),
            "I1 complete one-pass pages"
        );
        events[0].processed_at = Some("t1".into());
        let changed = events[0].clone();
        events.rotate_left(1);
        events.remove(3);
        let mut next = EventPageIndex::new("s", &events, &owners);
        next.inherit_delivery_changes(&first, 3, &[changed]);
        let page = next.delivery_page((2, usize::MAX), 128);
        assert_eq!(page.len(), 1, "I2 exactly one enrichment");
        assert_eq!(events[page[0].1].id, "id-0", "I3 current position");
        assert_eq!(
            next.delivery_changes.len(),
            events.len(),
            "I4 bounded retained ids"
        );
        assert_eq!(
            next.delivery_positions.len(),
            events.len(),
            "I4 no historical duplicate entries"
        );
    }
}
