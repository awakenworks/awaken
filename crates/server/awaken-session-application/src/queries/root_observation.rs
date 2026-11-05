// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Reads the selected original owner and complete root before any access proof.
// @formal reason: Requested id, durable partition and immutable private Workspace must agree;
// @formal reason: this consistency boundary is not a current caller/PDP admission proof.
//! One live native root observation shared by Session and linked-content reads.
use crate::{SessionAccessError, SessionMutationError};
use awaken_session_contract::{
    PersistedSession, SessionPrivateBinding, SessionRepositoryError, SessionRepositoryReads,
};
use awaken_session_types::SessionId;

pub(crate) async fn observe_session_root(
    repository: &dyn SessionRepositoryReads,
    owner_scope: &str,
    session_id: &SessionId,
) -> Result<PersistedSession, SessionAccessError> {
    let owner =
        super::session_owner(repository, session_id)
            .await
            .map_err(|error| match error {
                SessionMutationError::NotFound => SessionAccessError::NotFound,
                SessionMutationError::Internal(message) => SessionAccessError::Internal(message),
                error => SessionAccessError::Unavailable(error.to_string()),
            })?;
    if owner != owner_scope {
        return Err(SessionAccessError::NotFound);
    }
    let root = repository
        .get(session_id)
        .await
        .map_err(|error| match error {
            SessionRepositoryError::NotFound => SessionAccessError::NotFound,
            error => SessionAccessError::Unavailable(error.to_string()),
        })?;
    if root.session_id != session_id
        || SessionPrivateBinding::of(&root)
            .is_some_and(|binding| binding.workspace_id() != owner_scope)
    {
        return Err(SessionAccessError::Internal(
            "Session ownership observation has inconsistent identity".into(),
        ));
    }
    Ok(root)
}
