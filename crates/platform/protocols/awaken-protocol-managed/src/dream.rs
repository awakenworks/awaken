// @formal surface requirement=product.dream.dream_job
// @formal reason: This source implements the repository-controlled dream product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Managed-protocol adapters for the Coordinator-owned Dream application.

use async_trait::async_trait;
use awaken_dream_application::DreamSessionSource;
use awaken_session_dream_contract::DreamUsage;

#[async_trait]
impl DreamSessionSource for crate::ManagedState {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Result<Vec<awaken_session_types::SessionId>, String> {
        // Scan the durable Workspace roots, never this replica's disposable
        // wire cache: a Session warm only on another replica is still Dream
        // input, and the neutral root status is the same one GET projects.
        let persisted = self
            .roles
            .roots
            .sessions_by_owner(workspace_id)
            .await
            .map_err(|error| format!("Workspace Sessions are unreadable: {error}"))?;
        Ok(dream_eligible_sessions(persisted, updated_after_ms, limit))
    }

    async fn session_usage(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<DreamUsage>, String> {
        // An owner this store could not read is not the same fact as a Session
        // owned by someone else: the first is reported, the second answers
        // "nothing for you".
        let owner = self
            .resolve_owner(session_id)
            .await
            .map_err(|error| format!("Session owner is unreadable: {error}"))?;
        if owner.as_deref() != Some(workspace_id) {
            return Ok(None);
        }
        let usage = self
            .roles
            .observation
            .session_usage(session_id, None)
            .await
            .map_err(|error| format!("Session usage is unreadable: {error}"))?;
        Ok(Some(DreamUsage {
            cache_creation_input_tokens: usage.cache_creation_tokens,
            cache_read_input_tokens: usage.cache_read_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        }))
    }
}

/// Select Dream input from durable roots: visible, not Running, not itself a
/// Dream Session, and updated after the policy cutoff; newest first.
fn dream_eligible_sessions(
    persisted: Vec<awaken_session_contract::PersistedSession>,
    updated_after_ms: u64,
    limit: usize,
) -> Vec<awaken_session_types::SessionId> {
    let mut sessions = persisted
        .into_iter()
        .filter(|session| !session.is_hidden())
        .filter(|session| {
            session.public_status() != awaken_session_contract::SessionPublicStatus::Running
        })
        .filter(|session| {
            session
                .metadata
                .get("awaken.session.origin")
                .is_none_or(|origin| origin != "dream")
        })
        .filter_map(|session| {
            // An unrecorded update time is historical unknown, never "new".
            let updated = session.updated_at_unix_ms?;
            (updated > updated_after_ms).then_some((updated, session.session_id))
        })
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| right.cmp(left));
    sessions.into_iter().take(limit).map(|(_, id)| id).collect()
}
