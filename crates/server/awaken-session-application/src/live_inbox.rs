//! Workspace-fenced application boundary for attempt-local live steering.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_contract::{LiveInboxApplication, LiveInboxApplicationError, LiveInboxSnapshot};

use crate::SessionApplication;

impl SessionApplication {
    async fn require_live_inbox_owner(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<(), LiveInboxApplicationError> {
        match self.owner(session_id).await {
            Some(owner) if owner == workspace_id => Ok(()),
            // Unknown and foreign Sessions are intentionally indistinguishable.
            Some(_) | None => Err(LiveInboxApplicationError::NotFound),
        }
    }
}

#[async_trait::async_trait]
impl LiveInboxApplication for SessionApplication {
    async fn snapshot(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        Ok(self.runtime().live_inbox_snapshot(session_id).await)
    }

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_queue(session_id, content)
            .await
            .map_err(Into::into)
    }

    async fn remove(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: u64,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_remove(session_id, message_id)
            .await
            .map_err(Into::into)
    }

    async fn replace(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_replace(session_id, message_id, content)
            .await
            .map_err(Into::into)
    }

    async fn reorder(
        &self,
        workspace_id: &str,
        session_id: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxApplicationError> {
        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        self.runtime()
            .live_inbox_reorder(session_id, order)
            .await
            .map_err(Into::into)
    }
}
