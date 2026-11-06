//! Tri-state observation of durable state: present, absent, or *unknown*.
//!
//! Recovery paths repeatedly shipped the same defect: a read that failed was
//! treated as a read that found nothing, and the runtime then started fresh
//! execution over durable state that actually existed. The shape
//! `Result<Option<T>, E>` invites exactly that — `.ok().flatten()`,
//! `.unwrap_or_default()`, `let Ok(Some(x)) = ... else { start fresh }` — because
//! the type says "error" and "empty" are different things while giving the
//! caller one-token ways to fuse them.
//!
//! [`Observed`] removes the fusion from the vocabulary. A port that returns it
//! reports three outcomes that no combinator can collapse, so "I could not find
//! out" can only become "nothing is there" by a caller writing that decision
//! down in a `match` arm, where a reviewer sees it.

/// Why an observation could not determine whether the state exists.
///
/// The cause is carried, never erased: a fenced read and a dead disk lead to
/// different operator action even though both leave the runtime ignorant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnknownCause {
    /// The backend could not be reached, or its bytes could not be decoded.
    #[error("storage unavailable: {0}")]
    Storage(String),
    /// A newer claim owns this state; this reader is not permitted to see it.
    #[error("read was fenced: {0}")]
    Fenced(String),
    /// The read was abandoned before it answered (deadline, shutdown, drop).
    #[error("read did not complete: {0}")]
    Interrupted(String),
}

/// The outcome of one read of durable state.
///
/// `Unknown` is **not** `Absent`. A storage failure, a fence rejection, or an
/// interrupted read is not evidence that nothing exists.
///
/// There is deliberately **no escape hatch**: no `From<Observed<T>> for
/// Option<T>`, no `ok()`, no `flatten()`, no `unwrap_or_default()`, no
/// `unwrap_or_else()`, no `Default`. There are no combinators at all — `match`
/// is the entire interface. A caller who wants to treat "could not find out" as
/// "nothing there" must name that arm and say so in code.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a dropped observation was never acted on; `Unknown` must be handled explicitly"]
pub enum Observed<T> {
    /// The state exists and this is it.
    Present(T),
    /// The read succeeded and established that no such state exists.
    Absent,
    /// The read did not establish anything. This is not absence.
    Unknown(UnknownCause),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_distinguishable_from_absence() {
        // Cause C1: a read fails rather than establishing emptiness. Effect E1:
        // the observation is Unknown and never compares equal to Absent.
        // Decision rule R1=C1=>E1.
        // Constraints/invariants: the two outcomes stay separate values, so no
        // equality check can launder ignorance into evidence of absence.
        let unknown: Observed<u8> = Observed::Unknown(UnknownCause::Storage("disk".into()));
        assert_ne!(unknown, Observed::Absent, "R1/E1");
        assert_ne!(unknown, Observed::Present(0), "R1/E1");
    }

    #[test]
    fn unknown_retains_its_diagnostic_cause() {
        // Cause C2: a read is rejected by a fence, abandoned at a deadline, or
        // defeated by storage. Effect E2: each renders its own operator-facing
        // reason and the three remain distinct. Decision rule R2=C2=>E2.
        // Constraints/invariants: the cause is preserved verbatim; collapsing
        // the variants would erase the only signal that tells an operator
        // whether to fail over, retry, or replace a claim.
        assert_eq!(
            UnknownCause::Storage("disk".into()).to_string(),
            "storage unavailable: disk",
            "R2/E2"
        );
        assert_eq!(
            UnknownCause::Fenced("stale claim".into()).to_string(),
            "read was fenced: stale claim",
            "R2/E2"
        );
        assert_eq!(
            UnknownCause::Interrupted("deadline".into()).to_string(),
            "read did not complete: deadline",
            "R2/E2"
        );
        assert_ne!(
            UnknownCause::Storage("x".into()),
            UnknownCause::Fenced("x".into()),
            "R2/E2"
        );
    }

    #[test]
    fn present_carries_the_observed_value() {
        // Cause C3: a read finds the state. Effect E3: the value is delivered
        // only through the Present arm. Decision rule R3=C3=>E3.
        // Constraints/invariants: reaching the value requires naming the arm,
        // so no caller obtains it without having considered the other two.
        let observed = Observed::Present("partial");
        let Observed::Present(value) = observed else {
            panic!("R3/E3");
        };
        assert_eq!(value, "partial", "R3/E3");
    }
}
