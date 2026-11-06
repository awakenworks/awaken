// @formal surface exclude=application_adapter
// @formal owner crates/platform/session/awaken-session-contract/src/repository/repository_port.rs
// @formal owner crates/platform/session/awaken-session-application-contract/src/live_inbox.rs
// @formal owner crates/platform/session/awaken-session-application/src/session_input.rs
// @formal reason: The existing application owner check precedes every operation on the selected Inbox.
// @formal reason: The existing compiler and its private validated value live with the selected Inputs.
// @formal reason: No new queue, compiler policy, persistence or terminal decision is introduced.
//! Selected Session input planning and workspace-fenced live steering.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_session_application_contract::{
    LiveInboxApplication, LiveInboxApplicationError, LiveInboxSnapshot,
};

use crate::SessionInputRequest;
use crate::mutation::session_owner;
use crate::session_input::{normalize_session_input_mounts, plan_session_input_attachments};
use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::SessionRepositoryReads;
use awaken_session_execution_contract::LiveInboxAccess;
use std::sync::Arc;

mod lowering;

/// Session input operations over the selected store and attempt-local inbox.
/// The service holds the existing read and Inbox ports directly, with no
/// application or peer service back-reference. Its validated plan is consumed
/// unchanged by the existing Repository/Vault lowering path.
///
/// I1: the selected service directly implements the existing consumer port:
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::SessionInputs;
/// use awaken_session_application_contract::{LiveInboxApplication};
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
/// use awaken_session_application_contract::{LiveInboxApplication};
/// fn old_forwarder(app: Arc<SessionApplication>) -> Arc<dyn LiveInboxApplication> {
///     app
/// }
/// ```
pub struct SessionInputs {
    repository: Arc<dyn SessionRepositoryReads>,
    inbox: Arc<dyn LiveInboxAccess>,
    inventory: Option<Arc<dyn awaken_resource_contract::ResourceInventory>>,
}

/// A validated input plan: every derived path resolved and the effective
/// binding set checked, before any Repository or Vault participant.
pub struct PreparedSessionInputs {
    requests: Vec<SessionInputRequest>,
    attachments: Vec<crate::session_resource::SessionInputAttachment>,
    effective_bindings: Vec<awaken_resource_contract::InputBinding>,
}

impl PreparedSessionInputs {
    #[must_use]
    pub fn effective_bindings(&self) -> &[awaken_resource_contract::InputBinding] {
        &self.effective_bindings
    }
}

impl SessionInputs {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepositoryReads>,
        inbox: Arc<dyn LiveInboxAccess>,
        inventory: Option<Arc<dyn awaken_resource_contract::ResourceInventory>>,
    ) -> Self {
        Self {
            repository,
            inbox,
            inventory,
        }
    }

    /// Compile one complete binding plan before Skill/File/Vault/Repository
    /// effects. Construction supplies the selected Inventory; callers cannot
    /// replace it for one request.
    ///
    /// C1 the independently held Inputs compiles the existing value:
    ///
    /// ```
    /// use awaken_session_application::{SessionInputs, SessionInputRequest};
    /// async fn prepare(inputs: &SessionInputs, requests: &[SessionInputRequest]) {
    ///     let _ = inputs.prepare_session_inputs("session", "workspace", requests, &[]).await;
    /// }
    /// ```
    ///
    /// C2 the application cannot retain a second preparation entrance:
    ///
    /// ```compile_fail,E0599
    /// use awaken_session_application::{SessionApplication, SessionInputRequest};
    /// async fn old(app: &SessionApplication, requests: &[SessionInputRequest]) {
    ///     let _ = app.prepare_session_inputs("session", "workspace", requests, &[]).await;
    /// }
    /// ```
    ///
    /// The plan is consumed unchanged by
    /// `PreparedSessionInputs::lower` through the Repository participants.
    pub async fn prepare_session_inputs(
        &self,
        session_id: &str,
        owner_scope: &str,
        requests: &[SessionInputRequest],
        agent_defaults: &[awaken_resource_contract::InputBinding],
    ) -> Result<PreparedSessionInputs, RunError> {
        let normalized = normalize_session_input_mounts(
            self.inventory.as_deref(),
            owner_scope,
            requests,
            agent_defaults,
        )
        .await?;
        // Build the complete logical binding set with a non-durable placeholder
        // Repository id. The shared resolver owns replacement/collision/tree
        // semantics, and this call remains pure; only after it succeeds may the
        // caller enter Registry/Vault participants.
        let attachments = plan_session_input_attachments(session_id, &normalized, agent_defaults)
            .map_err(RunError::bad_request)?;
        let effective_bindings = crate::session_resource::SessionInputResolver::effective_bindings(
            agent_defaults,
            &attachments,
        )
        .map_err(|error| RunError::bad_request(error.to_string()))?;
        Ok(PreparedSessionInputs {
            requests: normalized,
            attachments,
            effective_bindings,
        })
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
            Err(crate::SessionMutationError::Internal(detail)) => {
                Err(LiveInboxApplicationError::InvalidSessionState { detail })
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
