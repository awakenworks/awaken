// @formal entrypoint session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
//! Admission of complete aggregate snapshots: the one ordering law every
//! holder of a frozen Session root projection applies before it replaces the
//! projection it holds — the Coordinator's dispatch projection and a Worker's
//! installed realization alike.

use crate::{FrozenSessionProjection, RunError, SessionRevision};

/// The aggregate identity of one frozen root projection: its revision and the
/// digest of the aggregate-owned coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenProjectionReceipt {
    pub revision: SessionRevision,
    pub fingerprint: String,
}

/// Why a frozen root projection may not replace the held one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionReceiptRefusal {
    /// The projection is older than the held one: a delayed Control read.
    #[error("stale frozen Session root projection; refresh from Control")]
    Stale,
    /// The projection disagrees with the held one at the same revision.
    #[error("conflicting frozen Session root projection at the same revision")]
    Conflict,
}

/// A stale projection is retryable; never terminally fail a Run merely
/// because another request installed a newer snapshot. A conflict is an
/// invariant violation. The codes are the realization's.
impl From<ProjectionReceiptRefusal> for RunError {
    fn from(refusal: ProjectionReceiptRefusal) -> Self {
        match refusal {
            ProjectionReceiptRefusal::Stale => {
                RunError::unavailable_classified("session_projection_stale", refusal.to_string())
            }
            ProjectionReceiptRefusal::Conflict => RunError::classified(
                "session_realization_invariant_violated",
                refusal.to_string(),
            ),
        }
    }
}

impl FrozenProjectionReceipt {
    /// The receipt of `p`. Only aggregate-owned coordinates enter equality.
    /// Executable publication, materialized context and decoded Resource
    /// receipts are derived by adapters; their exact publication/manifest
    /// checks remain authoritative.
    pub fn from_projection(
        p: &FrozenSessionProjection,
    ) -> Result<Self, awaken_agent_contract::CanonicalJsonHashError> {
        let fingerprint = awaken_agent_contract::canonical_json_sha256(
            "session-root-projection-v1",
            &(
                &p.workspace_id,
                &p.environment,
                &p.environment_materialization,
                p.resource_revision,
                &p.tools,
                &p.mcp,
            ),
        )?;
        Ok(Self {
            revision: p.revision,
            fingerprint,
        })
    }

    /// Admit `incoming` over the held receipt `current`: `Ok(true)` replaces
    /// (first or newer), `Ok(false)` is an exact replay, and an older or a
    /// same-revision conflicting projection is refused.
    pub fn admit(
        current: Option<&Self>,
        incoming: &Self,
    ) -> Result<bool, ProjectionReceiptRefusal> {
        projection_revision_admission(
            current.map(|receipt| receipt.revision.0),
            incoming.revision.0,
            current.is_none_or(|receipt| receipt.fingerprint == incoming.fingerprint),
        )
        .ok_or_else(|| {
            if current.is_some_and(|receipt| incoming.revision.0 < receipt.revision.0) {
                ProjectionReceiptRefusal::Stale
            } else {
                ProjectionReceiptRefusal::Conflict
            }
        })
    }
}

// This production kernel is also the bounded proof boundary. Neither a lease
// renewal nor a Resource revision is an aggregate ordering coordinate. The
// fingerprint is aggregate identity, so divergence at one revision conflicts.
fn projection_revision_admission(current: Option<u64>, incoming: u64, exact: bool) -> Option<bool> {
    use awaken_session_types::{MonotoneAdmission, SourceOrder, monotone_admission};
    match monotone_admission(SourceOrder::of_revision(current, incoming), exact) {
        MonotoneAdmission::Advance => Some(true),
        MonotoneAdmission::Replay => Some(false),
        MonotoneAdmission::Divergent | MonotoneAdmission::Reject => None,
    }
}

#[cfg(kani)]
/// @obligation session_environment.stale_projection_never_rolls_back_or_relabels_resident_or_retiring_owner
/// @proof-input-debt free_symbol projection_revision_admission exact
#[kani::proof]
fn projection_revision_admission_is_monotonic_and_exact() {
    // C1 prior receipt exists; C2 incoming root revision orders before/equal/after;
    // C3 aggregate coordinates agree. R1 first/newer admit replacement, R2 equal
    // and exact is replay, R3 older or equal-conflicting reject. Full u64 domain,
    // including zero/MAX, without arithmetic or ordering effect/generation strings.
    let current: Option<u64> = kani::any();
    let incoming: u64 = kani::any();
    let exact: bool = kani::any();
    let decision = projection_revision_admission(current, incoming, exact);
    assert_eq!(decision == Some(true), current.is_none_or(|r| incoming > r));
    assert_eq!(
        decision == Some(false),
        current.is_some_and(|r| incoming == r && exact)
    );
    assert_eq!(
        decision.is_none(),
        current.is_some_and(|r| incoming < r || incoming == r && !exact)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(revision: u64, fingerprint: &str) -> FrozenProjectionReceipt {
        FrozenProjectionReceipt {
            revision: SessionRevision(revision),
            fingerprint: fingerprint.into(),
        }
    }

    #[test]
    fn projection_admission_follows_its_decision_table() {
        // R1 first or newer replaces; R2 an exact replay keeps; R3 an older
        // projection is a retryable stale read; R4 a same-revision divergent
        // projection is an invariant conflict. The codes are the realization's.
        assert_eq!(
            FrozenProjectionReceipt::admit(None, &receipt(3, "a")),
            Ok(true),
            "R1"
        );
        let held = receipt(3, "a");
        assert_eq!(
            FrozenProjectionReceipt::admit(Some(&held), &receipt(4, "b")),
            Ok(true),
            "R1"
        );
        assert_eq!(
            FrozenProjectionReceipt::admit(Some(&held), &receipt(3, "a")),
            Ok(false),
            "R2"
        );
        let stale =
            FrozenProjectionReceipt::admit(Some(&held), &receipt(2, "a")).expect_err("R3 stale");
        assert_eq!(stale, ProjectionReceiptRefusal::Stale, "R3");
        let stale = RunError::from(stale);
        assert_eq!(
            stale.kind,
            awaken_session_types::RunErrorKind::Unavailable,
            "R3 retryable"
        );
        assert_eq!(stale.code, "session_projection_stale", "R3 code");
        let conflict =
            FrozenProjectionReceipt::admit(Some(&held), &receipt(3, "b")).expect_err("R4 conflict");
        assert_eq!(conflict, ProjectionReceiptRefusal::Conflict, "R4");
        assert_eq!(
            RunError::from(conflict).code,
            "session_realization_invariant_violated",
            "R4 code"
        );
    }
}
