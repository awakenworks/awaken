// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This source implements the repository-controlled session product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! The provenance of one external participant of a Session command.

/// Whether one external participant was created by the current Session command
/// or was adopted from exact durable truth left by an earlier attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionParticipantProvenance {
    Applied,
    Replayed,
}
