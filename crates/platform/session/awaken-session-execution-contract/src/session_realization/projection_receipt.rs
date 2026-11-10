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

/// The stable effect code of a [`ProjectionReceiptRefusal::Stale`] refusal.
pub const STALE_PROJECTION_CODE: &str = "session_projection_stale";

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
                RunError::unavailable_classified(STALE_PROJECTION_CODE, refusal.to_string())
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

/// Install `directive`'s frozen projection through `synchronizer`; `Ok(true)`
/// means the directive is superseded and the Session is already realized.
///
/// A Run-free preparation exists only to leave the Session realized. When its
/// directive is older than a projection this Worker already installed under the
/// same lease generation, a newer Control directive has realized the Session:
/// the preparation is superseded, not failed. Reporting it would expire the
/// lease that newer realization is using and record a failure the Session never
/// had. A stale refusal with no such newer holder, or one raised for a Run,
/// stays a loud failure.
pub async fn synchronize_or_supersede(
    synchronizer: &dyn crate::SessionProjectionSynchronizer,
    session_id: &crate::SessionId,
    source_run_id: Option<&awaken_agent_contract::agent::run::Id>,
    directive: &crate::SessionRealizationDirective,
    request_started: awaken_lease_contract::authority::AuthorityProofStarted,
) -> Result<bool, crate::SessionRealizationEffectFailure> {
    let lease = &directive.authority.lease;
    let Err(error) = synchronizer
        .synchronize_session_projection(
            session_id,
            &directive.projection,
            &directive.authority.clone().observed(request_started),
            directive.action.prepares_session(),
        )
        .await
    else {
        return Ok(false);
    };
    if source_run_id.is_none()
        && error.run_error().code == STALE_PROJECTION_CODE
        && synchronizer
            .holds_newer_projection(session_id, lease, directive.projection.revision)
            .await
    {
        return Ok(true);
    }
    Err(error)
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
            awaken_runtime_contract::runs::RunErrorKind::Unavailable,
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

    /// Refuses every projection (`Some(true)`: stale, `Some(false)`: another
    /// effect error), holding a newer one or not.
    struct Refusing {
        newer: bool,
        refusal: Option<bool>,
    }

    #[async_trait::async_trait]
    impl crate::SessionProjectionSynchronizer for Refusing {
        async fn execute_environment_continuation(
            &self,
            _: &crate::SessionRealizationPermit,
            _: &FrozenSessionProjection,
            _: &awaken_session_configuration_contract::resource::SessionRealizationLease,
            _: awaken_session_contract::SessionEnvironmentInstruction,
        ) -> Result<crate::SessionContinuationReceipt, crate::SessionRealizationEffectFailure>
        {
            unreachable!("supersession executes no continuation")
        }

        fn observe_realization_request(
            &self,
        ) -> Result<
            awaken_lease_contract::authority::AuthorityProofStarted,
            crate::SessionRealizationEffectFailure,
        > {
            unreachable!("supersession samples no authority")
        }
        async fn synchronize_session_projection(
            &self,
            _: &crate::SessionId,
            _: &FrozenSessionProjection,
            _: &crate::ObservedSessionRealizationLease,
            _: bool,
        ) -> Result<(), crate::SessionRealizationEffectFailure> {
            self.refusal.map_or(Ok(()), |stale| {
                Err(if stale {
                    RunError::from(ProjectionReceiptRefusal::Stale)
                } else {
                    RunError::unavailable("environment")
                }
                .into())
            })
        }
        async fn holds_newer_projection(
            &self,
            _: &crate::SessionId,
            _: &awaken_session_configuration_contract::resource::SessionRealizationLease,
            _: SessionRevision,
        ) -> bool {
            self.newer
        }
        async fn invalidate_session_realization_lease(
            &self,
            _: &crate::SessionId,
            _: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        ) -> Result<(), crate::SessionRealizationEffectFailure> {
            unreachable!("supersession invalidates nothing")
        }
    }

    #[tokio::test]
    async fn older_run_free_preparation_is_superseded_only_by_a_newer_held_projection() {
        // Decision table S0-S4. C1 the refused directive is Run-free or a Run's;
        // C2 the refusal is the stale-projection code; C3 this Worker holds a
        // newer projection under the same lease generation.
        // S0 installed => Ok(false); S1 Run-free + stale + newer held =>
        // superseded Ok(true) (no failure, no lease expiry); S2 no newer held =>
        // genuine stale, still an error; S3 a Run's directive stays an error;
        // S4 any other effect error stays an error.
        let session = crate::SessionId::from("session");
        let run = awaken_agent_contract::agent::run::Id("run".into());
        let mut projection = crate::session_realization::tests::legacy_resource_projection(false);
        projection.revision = SessionRevision(3);
        let directive = crate::SessionRealizationDirective {
            projection,
            authority: crate::SessionRealizationLeaseReceipt {
                lease: awaken_session_configuration_contract::resource::SessionRealizationLease {
                    owner: "worker".into(),
                    runtime_incarnation: "boot".into(),
                    epoch: 3,
                    expires_at_unix_ms: 10_000,
                },
                remaining_ms: std::num::NonZeroU64::MIN,
            },
            action: crate::SessionRealizationAction::Complete,
        };
        let started = awaken_lease_contract::authority::AuthorityProofStarted::from_clock_sample(
            awaken_time::mono::now(),
            std::time::Duration::ZERO,
        );
        let drive = async |newer, refusal, source| {
            let synchronizer = Refusing { newer, refusal };
            synchronize_or_supersede(&synchronizer, &session, source, &directive, started).await
        };
        assert!(matches!(drive(false, None, None).await, Ok(false)), "S0");
        assert!(
            matches!(drive(true, Some(true), None).await, Ok(true)),
            "S1"
        );
        assert!(drive(false, Some(true), None).await.is_err(), "S2");
        assert!(drive(true, Some(true), Some(&run)).await.is_err(), "S3");
        assert!(drive(true, Some(false), None).await.is_err(), "S4");
    }
}
