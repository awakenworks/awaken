// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the sole read-only page reducer over canonical Session recovery.
// @formal reason: Dynamic path: cursor validation and successful EOF precede every returned projection;
// @formal reason: later failures discard the prefix, while downstream root CAS and effect fencing retain
// @formal reason: their authority. Adjacent page-failure/cancellation tests and ADR-0065 are executable
// @formal reason: evidence for retention and completion boundaries, not a database snapshot proof.
//! Projection-only reduction of canonical recovery maintenance before effects.

use std::collections::BTreeMap;
use std::future::Future;

use awaken_session_repository_contract::SessionRepository;
use {
    awaken_session_state_contract::{SessionRepositoryError, SessionScan},
    awaken_session_types::SessionRecoveryQuarantine,
};

use super::SessionRecoveryCandidates;

/// Consume each fixed-size page before requesting the next. Reducers may only
/// read or build disposable projections: no effect or published observation is
/// permitted until this function proves EOF. A later error drops the prefix.
/// Candidate identities and exact diagnostics may grow with their output size;
/// full Session aggregates are retained for at most one repository page.
pub(super) async fn fold_reconcilable_sessions<T, E, F, Fut, M>(
    repository: &dyn SessionRepository,
    mut value: T,
    mut fold: F,
    map_repository_error: M,
) -> Result<(T, Vec<SessionRecoveryQuarantine>), E>
where
    M: Fn(SessionRepositoryError) -> E,
    F: FnMut(T, SessionScan) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut quarantined = BTreeMap::new();
    let mut cursor = None;
    loop {
        let page = repository
            .reconciliation_scan(cursor.as_ref())
            .await
            .map_err(&map_repository_error)?;
        let next_cursor = page.next_cursor.clone();
        if let (Some(current), Some(next)) = (cursor.as_ref(), next_cursor.as_ref())
            && next.session_id() <= current.session_id()
        {
            return Err(map_repository_error(SessionRepositoryError::Corrupt(
                "Session reconciliation cursor did not advance".into(),
            )));
        }
        for isolation in &page.quarantined {
            quarantined.insert(isolation.session_id.clone(), isolation.clone());
        }
        value = fold(value, page).await?;
        let Some(next_cursor) = next_cursor else {
            return Ok((value, quarantined.into_values().collect()));
        };
        cursor = Some(next_cursor);
    }
}

pub(super) async fn scan_recovery_candidates(
    repository: &dyn SessionRepository,
    audit_resource_references: bool,
    now_unix_ms: u64,
) -> Result<SessionRecoveryCandidates, SessionRepositoryError> {
    let (mut candidates, quarantined) = fold_reconcilable_sessions(
        repository,
        SessionRecoveryCandidates::default(),
        |mut candidates, page| async move {
            candidates.append_page(page, audit_resource_references, now_unix_ms);
            Ok(candidates)
        },
        std::convert::identity,
    )
    .await?;
    candidates.quarantined = quarantined;
    Ok(candidates)
}

/// The application's recovery scan at its clock's `now`, the instant the
/// continuation hint is decided at.
pub(super) async fn recovery_candidates(
    application: &super::SessionApplication,
    audit_resource_references: bool,
) -> Result<SessionRecoveryCandidates, SessionRepositoryError> {
    let now = application.clock.now().as_epoch_ms();
    scan_recovery_candidates(
        application.session_repository(),
        audit_resource_references,
        now,
    )
    .await
}

/// Disposable diagnostics owned by the existing, sequential lifecycle loop.
/// It neither schedules work nor changes Session truth. Only a completed scan
/// updates it; a failed/cancelled prefix cannot consume or erase a signal.
#[derive(Default)]
pub(super) struct ContinuationDiagnostics {
    observed: BTreeMap<
        awaken_session_types::SessionId,
        awaken_session_state_contract::SessionEnvironmentRejection,
    >,
}

impl ContinuationDiagnostics {
    pub(super) fn observe(
        &mut self,
        candidates: &SessionRecoveryCandidates,
    ) -> Vec<(
        awaken_session_types::SessionId,
        awaken_session_state_contract::SessionEnvironmentRejection,
    )> {
        let current: BTreeMap<_, _> = candidates
            .sessions
            .iter()
            .filter_map(|candidate| {
                candidate
                    .interests
                    .continuation_rejection
                    .map(|rejection| (candidate.session_id.clone(), rejection))
            })
            .collect();
        let signals = current
            .iter()
            .filter(|(session, rejection)| self.observed.get(*session) != Some(*rejection))
            .map(|(session, rejection)| (session.clone(), *rejection))
            .collect();
        // Replace the whole successful EOF view, dropping disappeared and
        // recovered roots. Size is bounded by this scan's rejected Sessions.
        self.observed = current;
        signals
    }

    /// Reuse the canonical EOF scan. Other status/phase scans never receive
    /// this loop-owned value, so they cannot publish an out-of-order view.
    pub(super) async fn scan(
        &mut self,
        application: &super::SessionApplication,
        audit_resource_references: bool,
    ) -> Result<SessionRecoveryCandidates, SessionRepositoryError> {
        let candidates = recovery_candidates(application, audit_resource_references).await?;
        self.signal(&candidates);
        Ok(candidates)
    }

    fn signal(&mut self, candidates: &SessionRecoveryCandidates) {
        for (session_id, rejection) in self.observe(candidates) {
            tracing::warn!(
                session = %session_id,
                reason = %rejection,
                "Session Environment continuation requires intervention"
            );
        }
    }
}
