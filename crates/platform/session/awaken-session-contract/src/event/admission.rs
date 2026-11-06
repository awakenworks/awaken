// @formal entrypoint session_event_batch.exact_retry_replays_and_changed_retry_conflicts
// @formal entrypoint session_event_batch.final_non_admission_excludes_late_acceptance
// @formal entrypoint session_event_batch.idempotency_key_appends_at_most_once
/// The sole closed/exact/conflict classification used by durable admission and
/// application retry lookup. It classifies retained Session-root facts only;
/// root CAS atomicity and persistence remain owned by the repository transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventBatchAdmissionClass {
    Absent,
    Accepted,
    Closed,
    Conflict,
}

#[must_use]
pub const fn classify_session_event_batch_admission(
    existing: bool,
    fingerprint_matches: bool,
    admission_closed: bool,
) -> SessionEventBatchAdmissionClass {
    if !existing {
        SessionEventBatchAdmissionClass::Absent
    } else if !fingerprint_matches {
        SessionEventBatchAdmissionClass::Conflict
    } else if admission_closed {
        SessionEventBatchAdmissionClass::Closed
    } else {
        SessionEventBatchAdmissionClass::Accepted
    }
}

/// The caller's requested transition against one retained retry identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventBatchAdmissionAction {
    Accept,
    Close,
}

/// Complete mutation plan consumed by the production Session-root resolver.
/// Only the two `Append*` variants authorize a new aggregate member.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventBatchAdmissionPlan {
    AppendAccepted,
    AppendClosed,
    ReplayAccepted,
    ReplayClosed,
    RejectClosed,
    Conflict,
}

impl SessionEventBatchAdmissionPlan {
    #[must_use]
    pub const fn appends(self) -> bool {
        matches!(self, Self::AppendAccepted | Self::AppendClosed)
    }
}

/// Resolve all admission facts before the application constructs a mutation.
/// A retained identity can only replay, reject, or conflict; therefore a root
/// CAS loser that reloads the winner cannot append the same key again.
#[must_use]
pub const fn resolve_session_event_batch_admission(
    existing: bool,
    fingerprint_matches: bool,
    admission_closed: bool,
    action: SessionEventBatchAdmissionAction,
) -> SessionEventBatchAdmissionPlan {
    use SessionEventBatchAdmissionAction::{Accept, Close};
    use SessionEventBatchAdmissionClass::{Absent, Accepted, Closed, Conflict};
    use SessionEventBatchAdmissionPlan::{
        AppendAccepted, AppendClosed, Conflict as RejectConflict, RejectClosed, ReplayAccepted,
        ReplayClosed,
    };

    match (
        classify_session_event_batch_admission(existing, fingerprint_matches, admission_closed),
        action,
    ) {
        (Absent, Accept) => AppendAccepted,
        (Absent, Close) => AppendClosed,
        (Accepted, Accept | Close) => ReplayAccepted,
        (Closed, Accept) => RejectClosed,
        (Closed, Close) => ReplayClosed,
        (Conflict, Accept | Close) => RejectConflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_resolution_covers_every_class_and_action() {
        // Cause/effect decision table: retained class x requested action owns
        // every production decision. A1 absent+Accept=>append accepted;
        // A2 absent+Close=>append closed; A3 accepted+either=>replay accepted;
        // A4 closed+Accept=>reject closed; A5 closed+Close=>replay closed;
        // A6 conflict+either=>conflict. Only A1/A2 may append.
        let rows = [
            (
                SessionEventBatchAdmissionClass::Absent,
                SessionEventBatchAdmissionAction::Accept,
                SessionEventBatchAdmissionPlan::AppendAccepted,
            ),
            (
                SessionEventBatchAdmissionClass::Absent,
                SessionEventBatchAdmissionAction::Close,
                SessionEventBatchAdmissionPlan::AppendClosed,
            ),
            (
                SessionEventBatchAdmissionClass::Accepted,
                SessionEventBatchAdmissionAction::Accept,
                SessionEventBatchAdmissionPlan::ReplayAccepted,
            ),
            (
                SessionEventBatchAdmissionClass::Accepted,
                SessionEventBatchAdmissionAction::Close,
                SessionEventBatchAdmissionPlan::ReplayAccepted,
            ),
            (
                SessionEventBatchAdmissionClass::Closed,
                SessionEventBatchAdmissionAction::Accept,
                SessionEventBatchAdmissionPlan::RejectClosed,
            ),
            (
                SessionEventBatchAdmissionClass::Closed,
                SessionEventBatchAdmissionAction::Close,
                SessionEventBatchAdmissionPlan::ReplayClosed,
            ),
            (
                SessionEventBatchAdmissionClass::Conflict,
                SessionEventBatchAdmissionAction::Accept,
                SessionEventBatchAdmissionPlan::Conflict,
            ),
            (
                SessionEventBatchAdmissionClass::Conflict,
                SessionEventBatchAdmissionAction::Close,
                SessionEventBatchAdmissionPlan::Conflict,
            ),
        ];
        for (class, action, expected) in rows {
            let (existing, matches, closed) = match class {
                SessionEventBatchAdmissionClass::Absent => (false, false, false),
                SessionEventBatchAdmissionClass::Accepted => (true, true, false),
                SessionEventBatchAdmissionClass::Closed => (true, true, true),
                SessionEventBatchAdmissionClass::Conflict => (true, false, false),
            };
            let actual = resolve_session_event_batch_admission(existing, matches, closed, action);
            assert_eq!(actual, expected, "{class:?} x {action:?}");
            assert_eq!(actual.appends(), !existing, "{class:?} x {action:?}");
        }
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_ACTION: [SessionEventBatchAdmissionAction; 2] = [
        SessionEventBatchAdmissionAction::Accept,
        SessionEventBatchAdmissionAction::Close,
    ];

    const fn listed_action(action: SessionEventBatchAdmissionAction) -> usize {
        match action {
            SessionEventBatchAdmissionAction::Accept => 0,
            SessionEventBatchAdmissionAction::Close => 1,
        }
    }

    #[test]
    fn every_admission_action_is_listed() {
        for (index, action) in EVERY_ACTION.into_iter().enumerate() {
            assert_eq!(listed_action(action), index);
        }
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x SessionEventBatchAdmissionAction(2) = 16
    /// @obligation session_event_batch.exact_retry_replays_and_changed_retry_conflicts
    /// @obligation session_event_batch.final_non_admission_excludes_late_acceptance
    /// @obligation session_event_batch.idempotency_key_appends_at_most_once
    #[test]
    fn session_event_batch_admission_resolution_is_total_closed_and_append_once() {
        // Decision table causes: C1 retry identity exists; C2 fingerprint matches;
        // C3 retained receipt is closed; C4 request is Accept/Close. Effects:
        // E1 only !C1 appends; E2 exact accepted always replays; E3 exact closed
        // rejects Accept and replays Close; E4 changed fingerprint conflicts.
        // All sixteen rows, including irrelevant C2/C3 facts when C1 is false.
        let mut cases = 0;
        for existing in [false, true] {
            for fingerprint_matches in [false, true] {
                for admission_closed in [false, true] {
                    for action in EVERY_ACTION {
                        let accept = action == SessionEventBatchAdmissionAction::Accept;
                        let actual = resolve_session_event_batch_admission(
                            existing,
                            fingerprint_matches,
                            admission_closed,
                            action,
                        );
                        let class = classify_session_event_batch_admission(
                            existing,
                            fingerprint_matches,
                            admission_closed,
                        );

                        assert_eq!(actual.appends(), !existing);
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::AppendAccepted,
                            !existing && accept
                        );
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::AppendClosed,
                            !existing && !accept
                        );
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::ReplayAccepted,
                            class == SessionEventBatchAdmissionClass::Accepted
                        );
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::RejectClosed,
                            class == SessionEventBatchAdmissionClass::Closed && accept
                        );
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::ReplayClosed,
                            class == SessionEventBatchAdmissionClass::Closed && !accept
                        );
                        assert_eq!(
                            actual == SessionEventBatchAdmissionPlan::Conflict,
                            class == SessionEventBatchAdmissionClass::Conflict
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 16);
    }
}
