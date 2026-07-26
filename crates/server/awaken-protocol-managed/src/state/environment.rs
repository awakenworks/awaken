//! Session-owned runtime environment identity persistence.
//!
//! The runtime materializes an opaque binding, while the Session aggregate owns
//! its durable identity. This module contains only that narrow root-CAS command;
//! Environment authoring and immutable snapshot compilation remain in
//! `routes::environments`.

use super::*;

impl ManagedState {
    /// Persist the runtime's opaque Session-environment identity after an
    /// execution edge has materialized it through the same root mutation path as
    /// every other Session fact. A CAS retry reloads and reapplies only this
    /// binding, so it cannot roll back a concurrent Resource/MCP transition.
    pub(crate) async fn persist_session_environment_binding(
        &self,
        session_id: &str,
    ) -> Result<(), StateError> {
        let Some(binding) = self
            .runtime
            .session_environment_binding(session_id)
            .await
            .map_err(StateError::Run)?
        else {
            return Ok(());
        };
        for attempt in 0..Self::ROOT_CAS_ATTEMPTS {
            let owner_scope = self
                .sessions_repo
                .owner(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            let mut session = self
                .sessions_repo
                .get(session_id)
                .await
                .ok_or(StateError::NotFound)?;
            if session.environment_binding.as_deref() == Some(binding.as_str()) {
                return Ok(());
            }
            session.environment_binding = Some(binding.clone());
            match self
                .commit_session_snapshot(&owner_scope, session, "bind-environment", Vec::new())
                .await
            {
                Err(StateError::Conflict) if attempt + 1 < Self::ROOT_CAS_ATTEMPTS => continue,
                result => return result.map(|_| ()),
            }
        }
        Err(StateError::Conflict)
    }
}
