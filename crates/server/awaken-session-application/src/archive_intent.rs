// @formal evidence session_execution.terminal_state_never_reopens
//! Read the existing atomic archive receipt; identity belongs to Session state.

use awaken_session_state_contract::{
    PersistedSession, session_unattempted_profiled_archive_record,
};

pub(crate) async fn unattempted_profiled_archive_witness(
    repository: &dyn awaken_session_repository_contract::SessionRepository,
    owner_scope: &str,
    session: &PersistedSession,
) -> Result<bool, awaken_session_state_contract::SessionRepositoryError> {
    let Some(expected) = session_unattempted_profiled_archive_record(owner_scope, session) else {
        return Ok(false);
    };
    Ok(repository
        .idempotency_receipt(&session.session_id, &expected.key)
        .await?
        .is_some_and(|receipt| expected.confirmed_by(&receipt, session.revision)))
}

/// The two original no-Create witnesses, shared by cleanup assignment and
/// Worker retirement. Missing historical materialization is never a witness.
pub(crate) async fn terminal_environment_creation_not_attempted(
    repository: &dyn awaken_session_repository_contract::SessionRepository,
    owner_scope: &str,
    session: &PersistedSession,
) -> Result<bool, awaken_session_state_contract::SessionRepositoryError> {
    Ok((session.is_terminal()
        && awaken_session_state_contract::session_has_no_create_evidence(session))
        || unattempted_profiled_archive_witness(repository, owner_scope, session).await?)
}
