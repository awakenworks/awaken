// @formal entrypoint session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast
// @formal entrypoint session.runtime_projection_stale_writer_never_rolls_back_visible_result
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! The one monotone admission kernel for installing a newer projection over
//! an older one.
//!
//! Consumers: the Managed committed-projection publisher (component-wise
//! Session revision / Thread version / lifecycle coordinates over a derived
//! wire rendering) and the Runtime Host's frozen Session-root receipt (one
//! [`crate::SessionRevision`] over an aggregate fingerprint). Each used to
//! hand-write the same newer/equal/older ladder. It lives with the Session
//! revision it orders, the one contract both already depend on.

/// Where a candidate's source coordinates stand against the installed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOrder {
    /// Nothing installed yet, or every coordinate is at least as new and one
    /// is newer.
    Newer,
    /// The same coordinates.
    Equal,
    /// Older in some coordinate, or incomparable: never guessed forward.
    Behind,
}

impl SourceOrder {
    /// Order two scalar revisions; `None` is "nothing installed".
    #[must_use]
    pub const fn of_revision(current: Option<u64>, incoming: u64) -> Self {
        match current {
            None => Self::Newer,
            Some(current) if incoming > current => Self::Newer,
            Some(current) if incoming == current => Self::Equal,
            Some(_) => Self::Behind,
        }
    }

    /// Order component-wise coordinates from their dominance and equality.
    #[must_use]
    pub const fn of_dominance(candidate_dominates: bool, source_equal: bool) -> Self {
        match (candidate_dominates, source_equal) {
            (false, _) => Self::Behind,
            (true, true) => Self::Equal,
            (true, false) => Self::Newer,
        }
    }
}

/// The admission of one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonotoneAdmission {
    /// Install the candidate.
    Advance,
    /// Same source, same content: an idempotent replay with no effect.
    Replay,
    /// Same source, different content. Whether that is a legitimate re-render
    /// (derived content) or a conflict (content is source identity) is the
    /// caller's classified decision, never a silent overwrite here.
    Divergent,
    /// The candidate is behind what is installed.
    Reject,
}

/// Monotone admission over classified facts.
#[must_use]
pub const fn monotone_admission(order: SourceOrder, same_content: bool) -> MonotoneAdmission {
    match order {
        SourceOrder::Newer => MonotoneAdmission::Advance,
        SourceOrder::Equal if same_content => MonotoneAdmission::Replay,
        SourceOrder::Equal => MonotoneAdmission::Divergent,
        SourceOrder::Behind => MonotoneAdmission::Reject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotone_admission_decision_table() {
        // | Rule | Order | Same content | Admission |
        // |---|---|---|---|
        // | M1 | Newer | any | Advance |
        // | M2 | Equal | yes | Replay |
        // | M3 | Equal | no | Divergent |
        // | M4 | Behind | any | Reject |
        for same in [false, true] {
            assert_eq!(
                monotone_admission(SourceOrder::Newer, same),
                MonotoneAdmission::Advance,
                "M1"
            );
            assert_eq!(
                monotone_admission(SourceOrder::Behind, same),
                MonotoneAdmission::Reject,
                "M4"
            );
        }
        assert_eq!(
            monotone_admission(SourceOrder::Equal, true),
            MonotoneAdmission::Replay,
            "M2"
        );
        assert_eq!(
            monotone_admission(SourceOrder::Equal, false),
            MonotoneAdmission::Divergent,
            "M3"
        );
    }

    #[test]
    fn projection_convergence_source_order_classifies_revisions_and_dominance() {
        // Revisions: nothing installed or larger => Newer; equal => Equal;
        // smaller => Behind, including the u64 extremes.
        for (current, incoming, order) in [
            (None, 0, SourceOrder::Newer),
            (None, u64::MAX, SourceOrder::Newer),
            (Some(3), 4, SourceOrder::Newer),
            (Some(3), 3, SourceOrder::Equal),
            (Some(3), 2, SourceOrder::Behind),
            (Some(u64::MAX), 0, SourceOrder::Behind),
            (Some(0), u64::MAX, SourceOrder::Newer),
        ] {
            assert_eq!(
                SourceOrder::of_revision(current, incoming),
                order,
                "{current:?}->{incoming}"
            );
        }
        // Dominance: not dominating is Behind even when "equal" is claimed.
        assert_eq!(SourceOrder::of_dominance(false, false), SourceOrder::Behind);
        assert_eq!(SourceOrder::of_dominance(false, true), SourceOrder::Behind);
        assert_eq!(SourceOrder::of_dominance(true, true), SourceOrder::Equal);
        assert_eq!(SourceOrder::of_dominance(true, false), SourceOrder::Newer);
    }
}
