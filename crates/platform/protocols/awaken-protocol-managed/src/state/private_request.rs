// @formal surface exclude=application_adapter
// @formal owner crates/platform/session/awaken-session-application/src/queries.rs
// @formal reason: Selects the native ownership role for a trusted request; it authenticates no caller.
//! One request's private authority over the existing Managed application.

use std::{convert::Infallible, sync::Arc};

use awaken_session_application::{SessionAccess, SessionAccessError};
use awaken_session_contract::SessionPrivateBinding;
use awaken_session_types::SessionId;
use axum::{extract::FromRequestParts, http::request::Parts};

use super::ManagedState;

/// In-process request marker issued only by a trusted, current private ingress.
/// It has no decoder and must never be constructed from a Session request body.
#[derive(Clone)]
pub struct AdmittedPrivateSessionRequest(SessionPrivateBinding);

impl AdmittedPrivateSessionRequest {
    /// Retain the exact binding from already completed private admission.
    #[must_use]
    pub fn from_admitted(binding: SessionPrivateBinding) -> Self {
        Self(binding)
    }

    pub(crate) fn binding(&self) -> &SessionPrivateBinding {
        &self.0
    }
}

/// The ordinary state and the private view share all of the same application
/// authorities. Scope guards and handlers extract from the same trusted marker.
/// The Coordinator can mount exported native Session handlers with this
/// extractor; the projected state stays within the protocol adapter.
pub struct ManagedRequestState(pub(crate) Arc<ManagedState>);

impl FromRequestParts<Arc<ManagedState>> for ManagedRequestState {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ManagedState>,
    ) -> Result<Self, Self::Rejection> {
        let state = match parts.extensions.get::<AdmittedPrivateSessionRequest>() {
            Some(admitted) => Arc::new(admitted_private_session_view(state, admitted.0.clone())),
            None if parts
                .extensions
                .get::<crate::HideSessionEventDefinitions>()
                .is_some() =>
            {
                let mut view = (**state).clone();
                view.agent_definition_presentation =
                    crate::event_presentation::AgentDefinitionPresentation::Omit;
                Arc::new(view)
            }
            None => state.clone(),
        };
        Ok(Self(state))
    }
}

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
    view.agent_definition_presentation =
        crate::event_presentation::AgentDefinitionPresentation::Manifest;
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
