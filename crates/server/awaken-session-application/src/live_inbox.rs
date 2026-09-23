//! Workspace-fenced application boundary for attempt-local live steering.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_execution_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxSnapshot,
};

use crate::SessionApplication;

impl SessionApplication {
    async fn require_live_inbox_owner(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), LiveInboxApplicationError> {
        match self.owner(session_id).await {
            Ok(owner) if owner == workspace_id => Ok(()),
            // Unknown and foreign Sessions are intentionally indistinguishable.
            Ok(_) | Err(crate::SessionMutationError::NotFound) => {
                Err(LiveInboxApplicationError::NotFound)
            }
            Err(error) => Err(LiveInboxApplicationError::AuthorityUnavailable {
                detail: error.to_string(),
            }),
        }
    }
}

#[async_trait::async_trait]
impl LiveInboxApplication for SessionApplication {
    async fn snapshot(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        Ok(self
            .runtime()
            .live_inbox_snapshot(session_id.as_str())
            .await)
    }

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_queue(session_id.as_str(), content)
            .await
            .map_err(Into::into)
    }

    async fn remove(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        message_id: u64,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_remove(session_id.as_str(), message_id)
            .await
            .map_err(Into::into)
    }

    async fn replace(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        message_id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_replace(session_id.as_str(), message_id, content)
            .await
            .map_err(Into::into)
    }

    async fn reorder(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_reorder(session_id.as_str(), order)
            .await
            .map_err(Into::into)
    }
}
