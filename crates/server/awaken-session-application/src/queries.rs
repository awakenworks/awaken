// @formal surface exclude=application_adapter
// @formal owner crates/contract/awaken-session-repository-contract/src/repository_port.rs
// @formal owner crates/contract/awaken-session-execution-contract/src/session/projection_source.rs
// @formal reason: Reads the selected repository and committed projection ports without issuing
// @formal reason: mutations or realization. The same handles are selected by process composition.
//! Session queries over the existing durable root and committed read roles.

use crate::{SessionMutationError, SessionRootReads, mutation::session_owner};
use awaken_executable_agent_contract::{
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
};
use awaken_session_configuration_contract::AgentCapabilities;
use awaken_session_repository_contract::SessionRepositoryReads;
use awaken_session_execution_contract::{CapabilityRuntime, SessionProjectionSource};
use awaken_session_state_contract::{
    DeletedSessionEventAdmissions, PersistedSession, SessionRepositoryError, SessionScan,
    SessionScanQuery,
};
use awaken_session_types::RunError;
use awaken_session_types::SessionId;
use std::sync::Arc;

#[path = "projection_evidence.rs"]
mod projection_evidence;

/// Read use cases with their own constructor-injected ports. No execution,
/// realization or mutation service is reachable through this service.
pub struct SessionQueries {
    repository: Arc<dyn SessionRepositoryReads>,
    projection: Arc<dyn SessionProjectionSource>,
    capabilities: Arc<dyn CapabilityRuntime>,
    profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
}

impl SessionQueries {
    /// Exact pins never fall through to mutable current catalog state.
    pub(crate) fn selected_session_profile(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: Option<u64>,
    ) -> Option<ExecutableAgentSessionProfile> {
        self.profiles
            .as_deref()
            .and_then(|source| match source_revision {
                Some(revision) => {
                    source.session_profile_at_revision_in(workspace_id, agent_id, revision)
                }
                None => source.session_profile_in(workspace_id, agent_id),
            })
    }

    pub(crate) fn capabilities_for(&self, thread: &str) -> Result<AgentCapabilities, RunError> {
        self.capabilities.as_ref().capabilities_for(thread)
    }

    #[must_use]
    pub fn new(
        repository: Arc<dyn SessionRepositoryReads>,
        projection: Arc<dyn SessionProjectionSource>,
        capabilities: Arc<dyn CapabilityRuntime>,
        profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
    ) -> Self {
        Self {
            repository,
            projection,
            capabilities,
            profiles,
        }
    }

    #[must_use]
    pub fn has_agent_profile_source(&self) -> bool {
        self.profiles.is_some()
    }

    #[must_use]
    pub fn agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.profiles
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn set_profiles(&mut self, profiles: Arc<dyn ExecutableAgentProfileSource>) {
        self.profiles = Some(profiles);
    }
}

#[async_trait::async_trait]
impl SessionRootReads for SessionQueries {
    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionMutationError> {
        session_owner(self.repository.as_ref(), session_id).await
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.repository
            .scan(SessionScanQuery::CredentialSourceReferences {
                workspace_id,
                source_id,
            })
            .await
            .map(SessionScan::into_sessions)
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError> {
        self.repository
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }

    async fn session(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        self.repository.get(session_id).await
    }

    async fn sessions_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.repository
            .scan(awaken_session_state_contract::SessionScanQuery::Workspace {
                workspace_id: owner_scope,
            })
            .await
            .map(awaken_session_state_contract::SessionScan::into_sessions)
    }
}
