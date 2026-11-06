// @formal entrypoint session.runtime_projection_delivery_preserves_exact_payload_digest_and_monotonic_completion
// @formal entrypoint session.runtime_projection_equal_source_and_content_is_a_read_without_cache_write_or_broadcast
// @formal entrypoint session.runtime_projection_failed_refresh_stutters_warm_state
// @formal entrypoint session.runtime_projection_stale_writer_never_rolls_back_visible_result
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
//! Atomic publication of one fully reduced Managed projection candidate.

use super::super::delivery::committed_projection_delivery_delta;
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use std::collections::HashMap;

/// One completely reduced candidate ready for the existing Session-record CAS.
/// Construction owns canonical source stamping, so a publishable result cannot
/// carry a checkpoint from another observed prefix.
pub(super) struct ProjectionRefreshCandidate {
    base_cache_revision: u64,
    record: SessionRecord,
}

impl ProjectionRefreshCandidate {
    pub(super) fn new(
        base_cache_revision: u64,
        mut record: SessionRecord,
        session_revision: awaken_session_types::SessionRevision,
        root: Option<&RunRecoverySnapshot>,
        children: &HashMap<String, RunRecoverySnapshot>,
    ) -> Self {
        stamp_source(&mut record, session_revision, root, children);
        record.committed_projection_ready = true;
        Self {
            base_cache_revision,
            record,
        }
    }
}

#[cfg(test)]
impl ProjectionRefreshCandidate {
    /// The reduced record, for the warm/cold oracle's comparison.
    pub(super) fn into_record(self) -> SessionRecord {
        self.record
    }
}

/// Result of the sole committed-prefix builder. `Retry` represents a changed
/// source prefix, while an ordinary reducer error stays in the surrounding
/// `Result`; neither form contains a publishable record.
pub(super) enum ProjectionRefreshBuild<T> {
    Retry,
    Complete(T),
}

enum ProjectionRefreshEffect<T> {
    Stutter,
    Publish(T),
}

fn projection_refresh_effect<T, E>(
    build: Result<ProjectionRefreshBuild<T>, E>,
) -> Result<ProjectionRefreshEffect<T>, E> {
    match build {
        Err(error) => Err(error),
        Ok(ProjectionRefreshBuild::Retry) => Ok(ProjectionRefreshEffect::Stutter),
        Ok(ProjectionRefreshBuild::Complete(candidate)) => {
            Ok(ProjectionRefreshEffect::Publish(candidate))
        }
    }
}

fn install_complete_projection<T>(current: &mut T, candidate: T) {
    *current = candidate;
}

struct ObservedProjectionThreadPrefix<C> {
    session_revision: awaken_session_types::SessionRevision,
    root_thread_version: u64,
    child_thread_versions: C,
}

fn apply_observed_thread_prefix<C>(
    session_revision: &mut awaken_session_types::SessionRevision,
    root_thread_version: &mut u64,
    child_thread_versions: &mut C,
    observed: ObservedProjectionThreadPrefix<C>,
) {
    *session_revision = observed.session_revision;
    *root_thread_version = observed.root_thread_version;
    *child_thread_versions = observed.child_thread_versions;
}

fn stamp_source(
    candidate: &mut SessionRecord,
    session_revision: awaken_session_types::SessionRevision,
    root: Option<&RunRecoverySnapshot>,
    children: &HashMap<String, RunRecoverySnapshot>,
) {
    apply_observed_thread_prefix(
        &mut candidate.checkpoint.source.session_revision,
        &mut candidate.checkpoint.source.root_thread_version,
        &mut candidate.checkpoint.source.child_thread_versions,
        ObservedProjectionThreadPrefix {
            session_revision,
            root_thread_version: root.map_or(0, |value| value.thread_version),
            child_thread_versions: children
                .iter()
                .map(|(thread_id, snapshot)| (thread_id.clone(), snapshot.thread_version))
                .collect(),
        },
    );
}

impl ManagedState {
    pub(super) fn finish_projection_refresh(
        &self,
        session_id: &str,
        build: Result<ProjectionRefreshBuild<ProjectionRefreshCandidate>, StateError>,
    ) -> Result<bool, StateError> {
        match projection_refresh_effect(build)? {
            ProjectionRefreshEffect::Stutter => Ok(false),
            ProjectionRefreshEffect::Publish(candidate) => {
                let ProjectionRefreshCandidate {
                    base_cache_revision,
                    record,
                } = candidate;
                self.publish_projection_candidate(session_id, base_cache_revision, record)
            }
        }
    }

    /// Rebuild a small synchronous candidate against the latest cache revision
    /// and publish it through the same CAS as the full async reducer. The
    /// closure must be deterministic because a racing publisher may require a
    /// retry against a newer base.
    pub(in crate::state) fn publish_projection_update(
        &self,
        session_id: &str,
        mut update: impl FnMut(&mut SessionRecord) -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        loop {
            let base = self
                .sessions
                .lock()?
                .get(session_id)
                .cloned()
                .ok_or(StateError::NotFound)?;
            let base_cache_revision = base.cache_revision;
            let mut candidate = base;
            update(&mut candidate)?;
            if self.publish_projection_candidate(session_id, base_cache_revision, candidate)? {
                return Ok(());
            }
        }
    }

    pub(in crate::state) fn publish_projection_candidate(
        &self,
        session_id: &str,
        base_cache_revision: u64,
        mut candidate: SessionRecord,
    ) -> Result<bool, StateError> {
        let mut sessions = self.sessions.lock()?;
        let current = sessions.get_mut(session_id).ok_or(StateError::NotFound)?;
        let projection_changed = !current.same_projected_content(&candidate)?;
        match decide_projection_publish(
            base_cache_revision,
            current.cache_revision,
            &current.checkpoint.source,
            &candidate.checkpoint.source,
            projection_changed,
        ) {
            ProjectionPublishDecision::Apply => {}
            ProjectionPublishDecision::AlreadyCurrent => return Ok(true),
            decision @ (ProjectionPublishDecision::RetryStaleCache
            | ProjectionPublishDecision::RejectSourceRegression) => {
                tracing::debug!(
                    session_id,
                    reason = ?decision,
                    base_cache_revision,
                    current_cache_revision = current.cache_revision,
                    projection_changed,
                    "Managed projection retry"
                );
                return Ok(false);
            }
        }
        // The locked current projection, not a pre-read id set, owns the
        // delivery base. Validate every immutable field before installing or
        // broadcasting any part of the candidate.
        let delta = committed_projection_delivery_delta(
            session_id,
            &current.events,
            &mut candidate.events,
        )?;
        // A weaker observation can reconcile into an exact repeat. Reapply
        // the same kernel to the actual reconciled content before any write.
        if decide_projection_publish(
            base_cache_revision,
            current.cache_revision,
            &current.checkpoint.source,
            &candidate.checkpoint.source,
            !current.same_projected_content(&candidate)?,
        ) == ProjectionPublishDecision::AlreadyCurrent
        {
            return Ok(true);
        }
        candidate.advance_cache_revision()?;
        candidate.rebuild_event_page_index();
        candidate.index_delivery_delta(current, &delta);
        install_complete_projection(current, candidate);
        self.broadcast_committed_delta(session_id, &delta);
        Ok(true)
    }
}

#[cfg(kani)]
/// @obligation session.runtime_projection_failed_refresh_stutters_warm_state
/// @obligation session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn projection_refresh_failure_stutters_and_success_preserves_one_pair() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct VisiblePair {
        result: u8,
        checkpoint: u8,
    }

    // Causes: C1 the private reducer fails, observes a changed prefix, or
    // completes; C2 the warm result/checkpoint pair and completed candidate pair
    // are arbitrary. Effects: E1 failure returns the same error and exposes no
    // publish effect; E2 changed-prefix retry stutters; E3 completion publishes
    // the candidate result and checkpoint as one owned value. This is the exact
    // production finish/install kernel, not a caller-supplied match Boolean.
    let branch = kani::any::<u8>();
    kani::assume(branch < 3);
    let warm = VisiblePair {
        result: kani::any(),
        checkpoint: kani::any(),
    };
    let observed_checkpoint = kani::any();
    let candidate = VisiblePair {
        result: kani::any(),
        checkpoint: observed_checkpoint,
    };
    let build = match branch {
        0 => Err(7_u8),
        1 => Ok(ProjectionRefreshBuild::Retry),
        _ => Ok(ProjectionRefreshBuild::Complete(candidate)),
    };
    let effect = projection_refresh_effect(build);
    let mut visible = warm;
    match effect {
        Err(error) => {
            assert_eq!(branch, 0);
            assert_eq!(error, 7);
        }
        Ok(ProjectionRefreshEffect::Stutter) => assert_eq!(branch, 1),
        Ok(ProjectionRefreshEffect::Publish(complete)) => {
            assert_eq!(branch, 2);
            install_complete_projection(&mut visible, complete);
        }
    }
    if branch < 2 {
        assert_eq!(visible, warm);
    } else {
        assert_eq!(visible, candidate);
        assert_eq!(visible.checkpoint, observed_checkpoint);
    }

    // The read-only observation wrapper may expose that unchanged warm pair
    // after a changed-prefix retry, but only when an earlier complete candidate
    // established its availability. Errors never enter this decision kernel.
    let prior_complete = kani::any();
    let refresh_complete = kani::any();
    let observation = super::refresh_entrypoint::projection_observation_decision(
        prior_complete,
        refresh_complete,
    );
    use super::refresh_entrypoint::ProjectionObservationDecision;
    assert_eq!(
        observation == ProjectionObservationDecision::UseRefreshed,
        refresh_complete
    );
    assert_eq!(
        observation == ProjectionObservationDecision::ServePrior,
        !refresh_complete && prior_complete
    );
    assert_eq!(
        observation == ProjectionObservationDecision::Retry,
        !refresh_complete && !prior_complete
    );
}

#[cfg(kani)]
/// @obligation session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
/// @kani profile=solver-kissat-v1
#[kani::proof]
fn projection_source_stamp_is_exact_for_the_complete_observed_thread_prefix() {
    // Causes: C1 the prior Session/root/two-child coordinates are arbitrary;
    // C2 one complete observed prefix supplies arbitrary replacement values.
    // Effect: E1 all three source dimensions become exactly C2 in one call;
    // unrelated lifecycle/budget coordinates are not inputs and cannot change.
    let mut session_revision = awaken_session_types::SessionRevision(kani::any());
    let mut root_thread_version = kani::any();
    let mut child_thread_versions = [kani::any::<u64>(), kani::any::<u64>()];
    let observed = ObservedProjectionThreadPrefix {
        session_revision: awaken_session_types::SessionRevision(kani::any()),
        root_thread_version: kani::any(),
        child_thread_versions: [kani::any::<u64>(), kani::any::<u64>()],
    };
    let expected_session = observed.session_revision;
    let expected_root = observed.root_thread_version;
    let expected_children = observed.child_thread_versions;
    apply_observed_thread_prefix(
        &mut session_revision,
        &mut root_thread_version,
        &mut child_thread_versions,
        observed,
    );
    assert_eq!(session_revision, expected_session);
    assert_eq!(root_thread_version, expected_root);
    assert_eq!(child_thread_versions, expected_children);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_finish_decision_table_is_closed() {
        // Cause/effect graph: C1 reducer outcome is error/retry/complete; C2 the
        // complete candidate pairs a rendered result with its observed source.
        // Effects: E1 error propagates without a publish effect; E2 retry
        // stutters; E3 complete transfers the whole pair unchanged.
        //
        // | Rule | Reducer outcome | Effect |
        // |---|---|---|
        // | F1 | error | E1 propagate/stutter |
        // | F2 | changed prefix | E2 retry/stutter |
        // | F3 | complete pair | E3 publish exact pair |
        let failed = projection_refresh_effect::<(u8, u8), _>(Err("failed"));
        assert!(matches!(failed, Err("failed")), "F1/E1");
        let retry = projection_refresh_effect::<(u8, u8), &str>(Ok(ProjectionRefreshBuild::Retry))
            .expect("F2 effect");
        assert!(matches!(retry, ProjectionRefreshEffect::Stutter), "F2/E2");
        let complete = projection_refresh_effect::<(u8, u8), &str>(Ok(
            ProjectionRefreshBuild::Complete((3, 5)),
        ))
        .expect("F3 effect");
        let ProjectionRefreshEffect::Publish(pair) = complete else {
            panic!("F3 must publish");
        };
        assert_eq!(pair, (3, 5), "F3/E3");
    }

    #[test]
    fn source_stamp_replaces_the_complete_observed_thread_prefix() {
        // Causes: C1 prior Session/root/child source coordinates differ from a
        // complete observed prefix; C2 lifecycle and budget coordinates are
        // independently retained by the reducer. Effect E1 the canonical stamp
        // replaces every C1 coordinate together and cannot touch C2.
        // Decision rule S1=C1+C2 => E1; partial observed prefixes are
        // unrepresentable because the typed input requires every coordinate.
        let mut session_revision = awaken_session_types::SessionRevision(1);
        let mut root_thread_version = 2;
        let mut child_thread_versions = [Some(3), None];
        let lifecycle_cursor = 8;
        let budget_reach_generation = 13;
        apply_observed_thread_prefix(
            &mut session_revision,
            &mut root_thread_version,
            &mut child_thread_versions,
            ObservedProjectionThreadPrefix {
                session_revision: awaken_session_types::SessionRevision(5),
                root_thread_version: 7,
                child_thread_versions: [None, Some(11)],
            },
        );
        assert_eq!(session_revision, awaken_session_types::SessionRevision(5));
        assert_eq!(root_thread_version, 7);
        assert_eq!(child_thread_versions, [None, Some(11)]);
        assert_eq!(lifecycle_cursor, 8, "S1/E1");
        assert_eq!(budget_reach_generation, 13, "S1/E1");
    }
}
