// @formal surface exclude=application_adapter
// @formal owner crates/contract/awaken-session-repository-contract/src/repository_port.rs
// @formal owner crates/contract/awaken-session-execution-contract/src/session/live_inbox.rs
// @formal reason: The existing application owner check precedes every operation on the selected Inbox.
// @formal reason: No new queue, authorization policy, persistence or terminal decision is introduced.
//! Workspace-fenced application boundary for attempt-local live steering.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_execution_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxSnapshot,
};

use crate::mutation::session_owner;
use awaken_session_repository_contract::SessionRepositoryReads;
use awaken_session_execution_contract::LiveInboxRuntime;
use std::sync::Arc;

/// Session input operations over the selected store and attempt-local inbox.
/// The service holds the existing ports directly, with no application or peer
/// service back-reference. Other input compilation use cases remain to migrate.
///
/// I1: the selected service directly implements the existing consumer port:
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::SessionInputs;
/// use awaken_session_execution_contract::LiveInboxApplication;
/// fn selected(inputs: Arc<SessionInputs>) -> Arc<dyn LiveInboxApplication> {
///     inputs
/// }
/// ```
///
/// I2: the application facade cannot retain a second implementation or forward
/// that role. The paired positive example verifies these imports and coercion.
///
/// ```compile_fail,E0277
/// use std::sync::Arc;
/// use awaken_session_application::SessionApplication;
/// use awaken_session_execution_contract::LiveInboxApplication;
/// fn old_forwarder(app: Arc<SessionApplication>) -> Arc<dyn LiveInboxApplication> {
///     app
/// }
/// ```
pub struct SessionInputs {
    repository: Arc<dyn SessionRepositoryReads>,
    inbox: Arc<dyn LiveInboxRuntime>,
}

impl SessionInputs {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepositoryReads>,
        inbox: Arc<dyn LiveInboxRuntime>,
    ) -> Self {
        Self { repository, inbox }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn replace_repository(&mut self, repository: Arc<dyn SessionRepositoryReads>) {
        self.repository = repository;
    }

    async fn require_live_inbox_owner(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), LiveInboxApplicationError> {
        match session_owner(self.repository.as_ref(), session_id).await {
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
impl LiveInboxApplication for SessionInputs {
    async fn snapshot(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<LiveInboxSnapshot, LiveInboxApplicationError> {
        let inbox_runtime = self.inbox.as_ref();

        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        inbox_runtime
            .live_inbox_snapshot(session_id.as_str())
            .await
            .map_err(Into::into)
    }

    async fn queue(
        &self,
        workspace_id: &str,
        session_id: &awaken_session_types::SessionId,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxApplicationError> {
        let inbox_runtime = self.inbox.as_ref();

        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        inbox_runtime
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
        let inbox_runtime = self.inbox.as_ref();

        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        inbox_runtime
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
        let inbox_runtime = self.inbox.as_ref();

        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        inbox_runtime
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
        let inbox_runtime = self.inbox.as_ref();

        self.require_live_inbox_owner(workspace_id, session_id)
            .await?;
        inbox_runtime
            .live_inbox_reorder(session_id.as_str(), order)
            .await
            .map_err(Into::into)
    }
}
