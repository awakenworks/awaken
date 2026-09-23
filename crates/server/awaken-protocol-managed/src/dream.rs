//! Managed-protocol adapters for the Coordinator-owned Dream application.

use async_trait::async_trait;
use awaken_dream_application::DreamSessionSource;
use awaken_session_dream_contract::DreamUsage;
use chrono::DateTime;

#[async_trait]
impl DreamSessionSource for crate::ManagedState {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Vec<awaken_session_types::SessionId> {
        let mut sessions = self
            .list_sessions_scoped(workspace_id)
            .into_iter()
            .filter(|session| session.status != crate::types::SessionStatus::Running)
            .filter(|session| {
                session
                    .metadata
                    .get("awaken.session.origin")
                    .is_none_or(|origin| origin != "dream")
            })
            .filter_map(|session| {
                let updated = DateTime::parse_from_rfc3339(&session.updated_at)
                    .ok()?
                    .timestamp_millis()
                    .max(0) as u64;
                (updated > updated_after_ms).then_some((updated, session.id))
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| right.cmp(left));
        sessions
            .into_iter()
            .take(limit)
            .map(|(_, id)| awaken_session_types::SessionId::from(id))
            .collect()
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
            .session_application()
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
