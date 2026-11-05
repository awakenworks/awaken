// @formal surface exclude=application_adapter
// @formal owner crates/server/awaken-session-application/src/queries.rs
// @formal reason: Selects the native ownership role for a trusted request; it authenticates no caller.
//! One request's private authority over the existing Managed application.

use awaken_session_application::{SessionAccess, SessionAccessError};
use awaken_session_contract::SessionPrivateBinding;
use awaken_session_types::SessionId;

use super::ManagedState;

/// Derive a view for one private request already admitted by trusted ingress.
///
/// The host must check current identity, Org membership, application binding
/// and PDP authority before each request. A decoded binding authenticates
/// nobody. Do not install a view as a shared substitute for those checks.
/// The view shares the original application, caches, event channels and id
/// sequence; selecting it never changes another request's authority.
#[must_use]
pub fn admitted_private_session_view(
    state: &ManagedState,
    binding: SessionPrivateBinding,
) -> ManagedState {
    let mut view = state.clone();
    view.private_binding = Some(binding);
    view
}

pub(crate) async fn authorize_session(
    state: &ManagedState,
    owner: &str,
    session_id: &SessionId,
) -> Result<SessionAccess, SessionAccessError> {
    match &state.private_binding {
        Some(binding) if binding.workspace_id() == owner => {
            state
                .roles
                .roots
                .authorize_private(binding, session_id)
                .await
        }
        Some(_) => Err(SessionAccessError::NotFound),
        None => state.roles.roots.authorize(owner, session_id).await,
    }
}
