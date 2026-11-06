// @formal surface exclude=application_adapter
// @formal owner crates/platform/session/awaken-session-contract/src/repository/repository_port.rs
// @formal owner crates/platform/session/awaken-session-execution-contract/src/session/projection_source.rs
// @formal reason: Reads the selected repository and committed projection ports without issuing
// @formal reason: mutations or realization. The same handles are selected by process composition.
//! Session queries over the existing durable root and committed read roles.

use crate::{SessionMutationError, SessionRootReads, mutation::session_owner};
use awaken_executable_agent_contract::{
    ExecutableAgentProfileSource, ExecutableAgentSessionProfile,
};
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::AgentCapabilities;
use awaken_session_contract::SessionRepositoryReads;
use awaken_session_contract::{
    DeletedSessionEventAdmissions, PersistedSession, SessionRepositoryError, SessionScan,
    SessionScanQuery,
};
use awaken_session_execution_contract::{
    CapabilityRuntime, SessionExecutor, SessionProjectionSource, ThreadReads,
};
use awaken_session_types::SessionId;
use std::sync::Arc;

mod file_authorization;
mod frozen_projection;
mod root_observation;
pub(crate) use root_observation::observe_session_root;
#[path = "projection_evidence.rs"]
mod projection_evidence;

/// Read use cases with their own constructor-injected ports. No execution,
/// realization or mutation service is reachable through this service.
///
/// Read-owner source controls: F0 the old App projection/transcript entries are
/// removed independently; F1 canonical Query methods and the original Runtime
/// raw reader remain callable. The runtime decision tables beside the query
/// tests own Scope, prefix/error ordering and effects; these compile-only
/// examples prove source capability, not branch admission or physical cleanup.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::frozen_session_projection;
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::session_transcript;
/// ```
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::committed_messages;
/// ```
///
/// ```
/// use awaken_session_application::SessionQueries;
/// use awaken_session_contract::PersistedSession;
/// use awaken_session_execution_contract::ThreadReads;
/// use awaken_runtime_contract::runs::{RunError};
/// async fn frozen(q: &SessionQueries, s: &PersistedSession)
///     -> Result<awaken_session_execution_contract::FrozenSessionProjection, RunError> {
///     q.frozen_session_projection("w".into(), s, true).await
/// }
/// async fn scoped(q: &SessionQueries) -> Result<Vec<awaken_agent_contract::Message>, RunError> {
///     q.session_transcript("w", "s").await
/// }
/// async fn original_raw(r: &dyn ThreadReads)
///     -> Result<Vec<awaken_agent_contract::Message>, RunError> {
///     r.committed_messages("thread").await
/// }
/// ```
pub struct SessionQueries {
    repository: Arc<dyn SessionRepositoryReads>,
    projection: Arc<dyn SessionProjectionSource>,
    thread_reads: Arc<dyn ThreadReads>,
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

    /// Read the complete exact publication through the selected profile source.
    /// A profile cannot stand in for this snapshot, and an absent exact value
    /// never falls through to the mutable current publication.
    pub(crate) fn executable_snapshot_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_runtime_contract::ExecutableAgentSnapshot> {
        self.profiles.as_ref().and_then(|source| {
            source.executable_snapshot_at_revision_in(workspace_id, agent_id, source_revision)
        })
    }

    /// Project only this original baseline and exact snapshot; a missing pin
    /// remains absent rather than falling through to today's current Agent.
    pub(crate) fn inspect_configuration(
        &self,
        workspace: &str,
        baseline: &awaken_session_configuration_contract::SessionBaseline,
    ) -> awaken_session_configuration_contract::SessionConfigurationInspection {
        let mut view =
            awaken_session_configuration_contract::SessionConfigurationInspection::from_baseline(
                baseline,
            );
        view.publication = baseline
            .agent_revision
            .and_then(|revision| {
                self.executable_snapshot_at_revision(workspace, &baseline.agent_id, revision)
            })
            .map(awaken_session_configuration_contract::publication_inspection);
        view
    }

    /// Preserve the selected source's own delegate-availability implementation.
    pub(crate) fn unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.profiles
            .as_ref()
            .and_then(|source| source.unavailable_delegate_in(workspace_id, agent_id))
    }

    pub(crate) fn capabilities_for(&self, thread: &str) -> Result<AgentCapabilities, RunError> {
        self.capabilities.as_ref().capabilities_for(thread)
    }

    /// Derive the read handles from one already-selected executor at construction.
    /// The borrowed executor is never retained; commands cannot be reached from
    /// this component. The raw reader and validated prefix adapter cannot be
    /// injected independently through another public constructor.
    ///
    /// C1 a complete immutable selection produces a Query that outlives this
    /// construction borrow. C2 the former four-argument constructor cannot
    /// combine an independent projection with another capability owner.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use awaken_session_application::SessionQueries;
    /// use awaken_session_contract::SessionRepositoryReads;
    /// use awaken_session_execution_contract::SessionExecutor;
    /// fn selected(repo: Arc<dyn SessionRepositoryReads>, executor: SessionExecutor) -> SessionQueries {
    ///     let query = SessionQueries::new(repo, &executor, None);
    ///     drop(executor);
    ///     query
    /// }
    /// ```
    ///
    /// ```compile_fail,E0061
    /// use std::sync::Arc;
    /// use awaken_session_application::SessionQueries;
    /// use awaken_session_contract::SessionRepositoryReads;
    /// use awaken_session_execution_contract::{CapabilityRuntime, SessionProjectionSource};
    /// fn separate(repo: Arc<dyn SessionRepositoryReads>, projection: Arc<dyn SessionProjectionSource>,
    ///             capabilities: Arc<dyn CapabilityRuntime>) {
    ///     SessionQueries::new(repo, projection, capabilities, None);
    /// }
    /// ```
    #[must_use]
    pub fn new(
        repository: Arc<dyn SessionRepositoryReads>,
        selected: &SessionExecutor,
        profiles: Option<Arc<dyn ExecutableAgentProfileSource>>,
    ) -> Self {
        Self {
            repository,
            projection: selected.projection_handle(),
            thread_reads: selected.thread_reads.clone(),
            capabilities: selected.capability.clone(),
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
}

#[async_trait::async_trait]
impl SessionRootReads for SessionQueries {
    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionMutationError> {
        session_owner(self.repository.as_ref(), session_id).await
    }

    async fn workspace_content_is_public(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<bool, SessionRepositoryError> {
        self.repository
            .workspace_content_is_public(owner_scope, session_id)
            .await
    }

    async fn authorize(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
    ) -> Result<crate::SessionAccess, crate::SessionAccessError> {
        let root = observe_session_root(self.repository.as_ref(), owner_scope, session_id).await?;
        if awaken_session_contract::SessionPrivateBinding::of(&root).is_some() {
            return Err(crate::SessionAccessError::NotFound);
        }
        Ok(crate::SessionAccess::verified(owner_scope, session_id))
    }

    async fn authorize_private(
        &self,
        binding: &awaken_session_contract::SessionPrivateBinding,
        session_id: &SessionId,
    ) -> Result<crate::SessionAccess, crate::SessionAccessError> {
        let root =
            observe_session_root(self.repository.as_ref(), binding.workspace_id(), session_id)
                .await?;
        if awaken_session_contract::SessionPrivateBinding::of(&root) != Some(binding) {
            return Err(crate::SessionAccessError::NotFound);
        }
        Ok(crate::SessionAccess::verified(
            binding.workspace_id(),
            session_id,
        ))
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
            .scan(awaken_session_contract::SessionScanQuery::Workspace {
                workspace_id: owner_scope,
            })
            .await
            .map(|scan| {
                scan.into_sessions()
                    .into_iter()
                    .filter(|root| {
                        awaken_session_contract::SessionPrivateBinding::of(root).is_none()
                    })
                    .collect()
            })
    }

    async fn private_sessions(
        &self,
        binding: &awaken_session_contract::SessionPrivateBinding,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.repository
            .scan(SessionScanQuery::Workspace {
                workspace_id: binding.workspace_id(),
            })
            .await
            .map(|scan| {
                scan.into_sessions()
                    .into_iter()
                    .filter(|root| {
                        awaken_session_contract::SessionPrivateBinding::of(root) == Some(binding)
                    })
                    .collect()
            })
    }
}
