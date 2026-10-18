// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the sole read-only page reducer over canonical Session recovery.
// @formal reason: Dynamic path: cursor validation and successful EOF precede every returned projection;
// @formal reason: later failures discard the prefix, while downstream root CAS and effect fencing retain
// @formal reason: their authority. Adjacent page-failure/cancellation tests and ADR-0065 are executable
// @formal reason: evidence for retention and completion boundaries, not a database snapshot proof.
//! Read-only reduction of the one canonical recovery scan before effects.

use std::collections::BTreeMap;
use std::future::Future;

use awaken_session_repository_contract::SessionRepository;
use {
    awaken_session_state_contract::{SessionRepositoryError, SessionScan, SessionScanQuery},
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
            .scan(SessionScanQuery::Reconcilable {
                after: cursor.as_ref(),
            })
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
