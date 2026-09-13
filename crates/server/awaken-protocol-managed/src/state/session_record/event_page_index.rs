//! Derived positions and primary action references for one published event log.
//! Unlike the canonicalizer's pre-sort Message joins, these positions describe
//! the final ordered vector and are installed with it at the existing cache CAS.

use std::collections::{HashMap, HashSet};

use crate::types::{Event, OutboundKind, StopReason};

#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::state) struct EventPageIndex {
    positions: HashMap<String, Vec<usize>>,
    pub(super) primary_answerable_ids: HashSet<String>,
}

impl EventPageIndex {
    pub(super) fn new(
        session_id: &str,
        events: &[Event],
        owners: &HashMap<String, String>,
    ) -> Self {
        let mut index = Self::default();
        for (position, event) in events.iter().enumerate() {
            // Retain every occurrence: cursor filtering and direction choose
            // the same first visible match as the canonical slice paginator.
            index
                .positions
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
                } => index
                    .primary_answerable_ids
                    .extend(event_ids.iter().cloned()),
                _ => {}
            }
        }
        index
    }

    pub(super) fn positions(&self, id: &str) -> Option<&[usize]> {
        self.positions.get(id).map(Vec::as_slice)
    }
}
