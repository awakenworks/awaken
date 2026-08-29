use std::sync::{Arc, RwLock};

use awaken_session_contract::{
    SessionEnvironmentPhase, SessionRecoveryScan, SessionRepositoryError,
};

use super::SessionApplication;

/// Secret-free cutover facts from one complete final Session scan followed by
/// one canonical global Environment-phase count.
///
/// The generation is process-local. Deployment automation must first observe a
/// baseline from each exact candidate process, then require a strictly newer
/// generation before treating the counts as post-cutover evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SessionEventBatchCutoverValidationSnapshot {
    pub generation: u64,
    pub terminal_with_incomplete_event_batches: u64,
    pub event_batch_failures: u64,
    pub quarantined: u64,
    pub restoring_sessions: u64,
}

/// Read-only process-local projection published by the sole Session supervisor.
pub struct SessionEventBatchCutoverValidationSource {
    latest: RwLock<Option<SessionEventBatchCutoverValidationSnapshot>>,
}

impl SessionEventBatchCutoverValidationSource {
    pub(crate) fn new() -> Self {
        Self {
            latest: RwLock::new(None),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<SessionEventBatchCutoverValidationSnapshot> {
        *self
            .latest
            .read()
            .expect("Session Event-batch cutover validation lock poisoned")
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn new_for_test() -> Self {
        Self::new()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn complete_scan_for_test(
        &self,
        scan: &SessionRecoveryScan,
        event_batch_failures: usize,
        restoring_sessions: u64,
    ) -> SessionEventBatchCutoverValidationSnapshot {
        self.complete_scan(scan, event_batch_failures, restoring_sessions)
    }

    fn complete_scan(
        &self,
        scan: &SessionRecoveryScan,
        event_batch_failures: usize,
        restoring_sessions: u64,
    ) -> SessionEventBatchCutoverValidationSnapshot {
        let terminal_with_incomplete_event_batches = scan
            .sessions
            .iter()
            .filter(|row| row.session.is_terminal() && row.session.has_incomplete_event_batches())
            .count();
        let mut latest = self
            .latest
            .write()
            .expect("Session Event-batch cutover validation lock poisoned");
        let generation = latest.map_or(1, |snapshot| snapshot.generation.saturating_add(1));
        let snapshot = SessionEventBatchCutoverValidationSnapshot {
            generation,
            terminal_with_incomplete_event_batches: count_as_u64(
                terminal_with_incomplete_event_batches,
            ),
            event_batch_failures: count_as_u64(event_batch_failures),
            quarantined: count_as_u64(scan.quarantined.len()),
            restoring_sessions,
        };
        *latest = Some(snapshot);
        snapshot
    }
}

fn count_as_u64(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

impl SessionApplication {
    /// Return the one read-only Event-batch cutover source owned by this
    /// application's lifecycle supervisor.
    #[must_use]
    pub fn session_event_batch_cutover_validation_source(
        &self,
    ) -> Arc<SessionEventBatchCutoverValidationSource> {
        self.event_batch_cutover_validation.clone()
    }

    /// Read the canonical Session recovery scan and global Environment phase
    /// count after every repair stage, then advance the process-local generation.
    /// Either repository failure preserves the preceding snapshot exactly.
    pub(crate) async fn refresh_event_batch_cutover_validation(
        &self,
        event_batch_failures: usize,
    ) -> Result<SessionEventBatchCutoverValidationSnapshot, SessionRepositoryError> {
        let scan = self.session_repository().reconcilable_sessions().await?;
        let restoring_sessions = self
            .session_repository()
            .count_environment_phase(SessionEnvironmentPhase::Restoring)
            .await?;
        Ok(self.event_batch_cutover_validation.complete_scan(
            &scan,
            event_batch_failures,
            restoring_sessions,
        ))
    }
}

#[cfg(test)]
mod tests {
    use awaken_session_contract::{SessionRecoveryQuarantine, SessionRecoveryScan};

    use super::*;

    #[test]
    fn completed_scan_projection_counts_quarantine_and_advances_generation() {
        // Cause/effect graph: C1 no completed scan exists; C2 a complete scan
        // reports one quarantined row; C3 a later complete scan is clean.
        // Effects: E1 pre-scan projection is absent; E2 C2 publishes generation
        // one with the exact count; E3 C3 atomically replaces it at generation
        // two. Scan failure is covered at the repository seam in the adjacent
        // application test because this reducer cannot receive an incomplete
        // scan by construction.
        //
        // | Rule | Prior snapshot | Complete scan | Effect |
        // |---|---|---|---|
        // | Q1 | absent | one quarantine | generation 1, quarantined 1 |
        // | Q2 | Q1 | clean | generation 2, all counts 0 |
        let projection = SessionEventBatchCutoverValidationSource::new();
        assert_eq!(projection.snapshot(), None, "E1");
        let first = projection.complete_scan(
            &SessionRecoveryScan {
                sessions: Vec::new(),
                quarantined: vec![SessionRecoveryQuarantine {
                    session_id: "not-projected".into(),
                    reason: "not-projected".into(),
                }],
            },
            0,
            2,
        );
        assert_eq!(first.generation, 1, "Q1/E2");
        assert_eq!(first.quarantined, 1, "Q1/E2");
        assert_eq!(first.restoring_sessions, 2, "Q1/E2");

        let second = projection.complete_scan(&SessionRecoveryScan::default(), 0, 0);
        assert_eq!(
            second,
            SessionEventBatchCutoverValidationSnapshot {
                generation: 2,
                terminal_with_incomplete_event_batches: 0,
                event_batch_failures: 0,
                quarantined: 0,
                restoring_sessions: 0,
            },
            "Q2/E3"
        );
    }
}
