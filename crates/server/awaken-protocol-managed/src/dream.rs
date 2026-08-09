//! Managed-protocol adapters for the Coordinator-owned Dream application.

use async_trait::async_trait;
use awaken_dream_application::DreamSessionSource;
use awaken_session_contract::DreamUsage;
use chrono::DateTime;

pub use awaken_dream_application::BUILT_IN_DREAM_AGENT_ID;

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
            .filter(|session| session.status != "running")
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

    fn session_usage(&self, workspace_id: &str, session_id: &str) -> Option<DreamUsage> {
        let session = self
            .list_sessions_scoped(workspace_id)
            .into_iter()
            .find(|session| session.id == session_id)?;
        Some(DreamUsage {
            cache_creation_input_tokens: session.usage.cache_creation_input_tokens,
            cache_read_input_tokens: session.usage.cache_read_input_tokens,
            input_tokens: session.usage.input_tokens,
            output_tokens: session.usage.output_tokens,
        })
    }
}
