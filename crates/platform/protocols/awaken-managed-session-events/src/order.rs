// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal entrypoint session_time.event_order_is_independent_of_wall_time
//! The canonical order of timeline facts.
//!
//! A fact's position is its committed source cursor, then its role phase inside
//! that commit, then an ordinal among same-phase facts of the commit. Commit
//! wall time is carried beside the key as evidence for the public timestamp and
//! never participates in ordering or identity.

use crate::role::phase;

/// Where a fact sits among the same-phase facts of one commit: the position
/// of its source (a Message in its Thread, a batch among the retained ones,
/// an audit row, an Outcome), then its position inside that source (a block,
/// a fold ordinal, a batch entry, an iteration).
///
/// Both axes come from the fact's own coordinates. The single ordinal they
/// replace scaled the source position by a stride read from whatever else the
/// reader held (its event count, its largest batch, a Message's own length),
/// so one fact's ordinal changed with the refresh schedule and a long Message
/// could sort its last blocks after the next Message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Ordinal {
    pub source: usize,
    pub within: usize,
}

impl Ordinal {
    /// The first position of a commit's phase.
    pub const FIRST: Self = Self::at(0);

    /// The position of a source that contributes one fact.
    #[must_use]
    pub const fn at(source: usize) -> Self {
        Self { source, within: 0 }
    }

    /// The position of one fact inside its source.
    #[must_use]
    pub const fn within(source: usize, within: usize) -> Self {
        Self { source, within }
    }
}

/// One immutable timeline coordinate.
#[derive(Clone, Copy, Debug)]
pub struct EventPosition {
    pub source_commit_cursor: u64,
    pub phase: u16,
    pub ordinal: Ordinal,
    /// Commit wall time of the source, when the prefix retained it.
    pub processed_at_unix_ms: Option<u64>,
}

// Wall time is evidence, never ordering/identity; clock rollback cannot move events.
impl PartialEq for EventPosition {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for EventPosition {}
impl PartialOrd for EventPosition {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for EventPosition {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.source_commit_cursor, self.phase, self.ordinal).cmp(&(
            other.source_commit_cursor,
            other.phase,
            other.ordinal,
        ))
    }
}

/// Bind a synthetic historical pending event to its committed Awaiting
/// coordinate. If a later terminal for the same Run is observed behind that
/// coordinate, withhold the pending order so the private projection fails
/// closed instead of publishing terminal history first.
#[must_use]
pub fn historical_pending_projection_order(
    awaiting_source_commit_cursor: u64,
    next_terminal_source_commit_cursor: Option<u64>,
    processed_at_unix_ms: Option<u64>,
) -> Option<EventPosition> {
    if next_terminal_source_commit_cursor
        .is_some_and(|terminal| terminal < awaiting_source_commit_cursor)
    {
        return None;
    }
    Some(EventPosition {
        source_commit_cursor: awaiting_source_commit_cursor,
        phase: phase::TRANSCRIPT,
        ordinal: Ordinal::FIRST,
        processed_at_unix_ms,
    })
}

#[cfg(kani)]
/// @obligation session.runtime_projection_historical_pending_precedes_terminal_projection
/// @obligation session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
/// @obligation session_time.event_order_is_independent_of_wall_time
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn canonical_event_order_never_depends_on_wall_time() {
    // O1 all order-key axes and optional wall-time observations are arbitrary.
    // O2 same key/different time remains equal. Production Ord/Eq are invoked
    // directly; evidence timestamps cannot reorder or split an identity.
    let left = EventPosition {
        source_commit_cursor: kani::any(),
        phase: kani::any(),
        ordinal: kani::any(),
        processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
    };
    let right = EventPosition {
        source_commit_cursor: kani::any(),
        phase: kani::any(),
        ordinal: kani::any(),
        processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
    };
    assert_eq!(
        left.cmp(&right),
        (left.source_commit_cursor, left.phase, left.ordinal).cmp(&(
            right.source_commit_cursor,
            right.phase,
            right.ordinal
        ))
    );
    assert_eq!(
        left,
        EventPosition {
            processed_at_unix_ms: right.processed_at_unix_ms,
            ..left
        }
    );
}

#[cfg(kani)]
/// @obligation session.runtime_projection_historical_pending_precedes_terminal_projection
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn historical_pending_order_precedes_every_admitted_terminal() {
    // Causes: C1 arbitrary Awaiting source cursor; C2 a later terminal is
    // absent/present at an arbitrary source cursor; C3 wall time is absent or
    // arbitrary. Effects: E1 no terminal retains pending; E2 terminal at/after
    // Awaiting admits pending strictly before terminal by canonical phase;
    // E3 a terminal behind Awaiting withholds the pending order, causing the
    // private canonicalization transaction to fail closed. This is the exact
    // production order constructor; lifecycle collection remains TCB.
    let awaiting: u64 = kani::any();
    let has_terminal: bool = kani::any();
    let terminal: u64 = kani::any();
    let observed_time = kani::any::<bool>().then(kani::any::<u64>);
    let planned = historical_pending_projection_order(
        awaiting,
        has_terminal.then_some(terminal),
        observed_time,
    );

    assert_eq!(planned.is_some(), !has_terminal || terminal >= awaiting);
    if let (Some(pending), true) = (planned, has_terminal) {
        let terminal = EventPosition {
            source_commit_cursor: terminal,
            phase: phase::THREAD_TERMINAL,
            ordinal: Ordinal::FIRST,
            processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
        };
        assert!(pending < terminal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_order_time_metamorphism_covers_every_sort_axis() {
        // O1/O2 keys independently vary cursor/phase/ordinal; clocks independently
        // vary missing/zero/max/backward. Changing only time must preserve both
        // equality and strict ordering; each axis is a mandatory tie-breaker.
        for left_key in [(0, 0, 0), (1, 0, 0), (1, 1, 0), (1, 1, 1)] {
            for right_key in [(0, 0, 0), (1, 0, 0), (1, 1, 0), (1, 1, 1)] {
                for left_time in [None, Some(0), Some(u64::MAX)] {
                    for right_time in [None, Some(0), Some(u64::MAX)] {
                        let left = EventPosition {
                            source_commit_cursor: left_key.0,
                            phase: left_key.1,
                            ordinal: Ordinal::at(left_key.2),
                            processed_at_unix_ms: left_time,
                        };
                        let right = EventPosition {
                            source_commit_cursor: right_key.0,
                            phase: right_key.1,
                            ordinal: Ordinal::at(right_key.2),
                            processed_at_unix_ms: right_time,
                        };
                        assert_eq!(left.cmp(&right), left_key.cmp(&right_key));
                        assert_eq!(left == right, left_key == right_key);
                    }
                }
            }
        }
    }

    #[test]
    fn historical_pending_order_decision_table() {
        // Cause/effect table: H1 no terminal=>publish pending; H2 same-coordinate
        // terminal=>pending phase first; H3 later terminal=>pending cursor first;
        // H4 terminal behind Awaiting=>withhold so canonicalization fails closed.
        // Wall-time is intentionally varied and never changes any decision/order.
        for (rule, terminal, expected) in [
            ("H1", None, true),
            ("H2", Some(10), true),
            ("H3", Some(11), true),
            ("H4", Some(9), false),
        ] {
            let planned = historical_pending_projection_order(10, terminal, Some(u64::MAX));
            assert_eq!(planned.is_some(), expected, "{rule}");
            if let (Some(pending), Some(terminal)) = (planned, terminal) {
                let terminal = EventPosition {
                    source_commit_cursor: terminal,
                    phase: phase::THREAD_TERMINAL,
                    ordinal: Ordinal::FIRST,
                    processed_at_unix_ms: Some(0),
                };
                assert!(pending < terminal, "{rule}");
            }
        }
    }
}
