//! Managed-protocol adapters for the Coordinator-owned Dream application.

use async_trait::async_trait;
use awaken_dream_application::DreamSessionSource;
use awaken_session_contract::DreamUsage;
use chrono::DateTime;

#[async_trait]
impl DreamSessionSource for crate::ManagedState {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Vec<String> {
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
        sessions.into_iter().take(limit).map(|(_, id)| id).collect()
    }

    async fn session_usage(&self, workspace_id: &str, session_id: &str) -> Option<DreamUsage> {
        if self
            .resolve_owner(session_id)
            .await
            .ok()
            .flatten()
            .as_deref()
            != Some(workspace_id)
        {
            return None;
        }
        let usage = self
            .session_application()
            .session_usage(session_id)
            .await
            .ok()?;
        Some(DreamUsage {
            cache_creation_input_tokens: usage.cache_creation_tokens,
            cache_read_input_tokens: usage.cache_read_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        })
    }
}
