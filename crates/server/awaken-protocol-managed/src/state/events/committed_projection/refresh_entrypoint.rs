//! Prefix-stable entrypoint for the warm/cold committed projection.

use super::*;
use crate::state::constants::COMMITTED_PREFIX_READ_CONFLICT;

const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProjectionObservationDecision {
    UseRefreshed,
    ServePrior,
    Retry,
}

pub(super) const fn projection_observation_decision(
    prior_complete: bool,
    refresh_complete: bool,
) -> ProjectionObservationDecision {
    if refresh_complete {
        ProjectionObservationDecision::UseRefreshed
    } else if prior_complete {
        ProjectionObservationDecision::ServePrior
    } else {
        ProjectionObservationDecision::Retry
    }
}

impl ManagedState {
    /// Refresh this process's disposable event projection from the Runtime's one
    /// durable transcript and Run lifecycle feed. A Session cache hit is not proof
    /// that it contains commits accepted through another protocol or Coordinator.
    pub(crate) async fn refresh_committed_events(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        self.ensure_session_record(session_id).await?;
        self.refresh_committed_projection(session_id).await
    }

    /// Refresh one read-only Event/Thread observation without requiring a quiet
    /// writer interval. Once this process has published one complete committed
    /// prefix, a racing append may make the new private candidate retry while
    /// the prior published prefix remains coherent and safe to return. Cold
    /// reads still use the strict bounded retry loop; source errors and stalled
    /// reads remain failures rather than being hidden as stale success. A page
    /// cursor is also a minimum observation: a prior prefix that lacks it cannot
    /// decide that the cursor is invalid, since another replica may have issued it.
    pub(crate) async fn refresh_committed_events_for_observation(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<(), StateError> {
        self.ensure_session_record(session_id).await?;
        let (cursor, _) = awaken_agent_contract::page::normalize_history_page_request(cursor, None);
        let prior_complete = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|record| {
                record.committed_projection_ready
                    && cursor.is_none_or(|cursor| record.event_positions(cursor).is_some())
            });
        if !prior_complete {
            return self.refresh_committed_projection(session_id).await;
        }

        let refresh = tokio::time::timeout(
            READ_BUDGET,
            self.refresh_committed_projection_once(session_id),
        )
        .await;
        let refresh_complete = match refresh {
            Ok(result) => result?,
            Err(_) => return Err(Self::committed_prefix_read_conflict(session_id, 1)),
        };
        match projection_observation_decision(prior_complete, refresh_complete) {
            ProjectionObservationDecision::UseRefreshed => Ok(()),
            ProjectionObservationDecision::ServePrior => {
                tracing::debug!(
                    session_id,
                    "Managed observation served the prior committed projection"
                );
                Ok(())
            }
            ProjectionObservationDecision::Retry => {
                Err(Self::committed_prefix_read_conflict(session_id, 1))
            }
        }
    }

    /// Rehydration and ordinary reads share this seqlock-style retry. A changed
    /// Session or committed Thread prefix discards the mixed read instead of
    /// publishing a projection assembled from incomparable source versions.
    pub(in crate::state) async fn refresh_committed_projection(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        // One ordinary turn may commit more than four source versions while a
        // reader is in flight. Bound elapsed read work, not the number of valid
        // concurrent commits. Every retry still rebuilds and validates the full
        // candidate; the delay gives the writer time without a database hot loop.
        let mut attempts = 0_u32;
        let refreshed = tokio::time::timeout(READ_BUDGET, async {
            loop {
                attempts += 1;
                if self.refresh_committed_projection_once(session_id).await? {
                    return Ok(());
                }
                tokio::time::sleep(RETRY_DELAY).await;
            }
        })
        .await;
        if let Ok(result) = refreshed {
            return result;
        }
        Err(Self::committed_prefix_read_conflict(session_id, attempts))
    }

    fn committed_prefix_read_conflict(session_id: &str, attempts: u32) -> StateError {
        tracing::warn!(
            session_id,
            attempts,
            "Managed projection read budget exhausted"
        );
        StateError::Run(RunError::unavailable(COMMITTED_PREFIX_READ_CONFLICT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_fallback_decision_table_is_closed() {
        // C1 a prior complete prefix is absent/present; C2 this refresh
        // retries/completes. E1 completion always publishes fresh; E2 only a
        // complete prior prefix permits stale observation; E3 cold retry stays
        // unavailable. All four Boolean rows are explicit.
        assert_eq!(
            projection_observation_decision(false, false),
            ProjectionObservationDecision::Retry,
            "R1/E3"
        );
        assert_eq!(
            projection_observation_decision(true, false),
            ProjectionObservationDecision::ServePrior,
            "R2/E2"
        );
        for prior in [false, true] {
            assert_eq!(
                projection_observation_decision(prior, true),
                ProjectionObservationDecision::UseRefreshed,
                "R3-R4/E1"
            );
        }
    }
}
