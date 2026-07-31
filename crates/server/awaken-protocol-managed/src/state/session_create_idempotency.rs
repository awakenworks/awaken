//! Public Session-create retry identity.
//!
//! The repository remains the only payload-match/replay authority. This module
//! only lowers an external owner-scoped idempotency key into the stable opaque
//! Session identity needed to address that existing receipt.

use std::sync::Arc;

use super::{DEFAULT_SCOPE, ManagedState, StateError};
use crate::types::{Session, SessionCreateParams};

impl ManagedState {
    pub async fn create_session_with_initial_events_idempotent(
        self: &Arc<Self>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
        idempotency_key: &str,
    ) -> Result<Session, StateError> {
        let owner_scope = workspace_id.as_deref().unwrap_or(DEFAULT_SCOPE);
        let session_id = format!(
            "sesn_{}",
            awaken_session_contract::stable_fingerprint(&(
                "managed-session-create-idempotency",
                owner_scope,
                idempotency_key,
            ))
        );
        self.create_session_with_initial_events_and_identity(req, workspace_id, Some(session_id))
            .await
    }
}
