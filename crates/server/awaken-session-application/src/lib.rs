//! Coordinator-owned Session application state and ports.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialSourceId, CredentialUsage,
};
use awaken_environment_contract::EnvItem;
use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, McpTarget, RunError,
    SessionEnvironmentBindingSink, SessionLifecycleSink, SessionRuntime,
};

/// Secret-free credential selection used while compiling a Session.
#[async_trait::async_trait]
pub trait SessionCredentialSource: Send + Sync {
    async fn has_vault(&self, id: &str) -> Result<bool, String>;

    async fn mcp_credential_source_for_url(
        &self,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String>;

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<CredentialAccess, String>;

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: CredentialUsage,
        policy: CredentialExecutionPolicy,
    ) -> Result<CredentialAccess, String>;
}

/// Write-only ingress for repository material. Implementations seal material
/// before returning the opaque source id used by Session compilation.
#[async_trait::async_trait]
pub trait RepositoryCredentialIngress: Send + Sync {
    async fn enter_repository_token(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<CredentialSourceId, String>;

    async fn rotate_repository_token(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<(), String>;
}

/// Exact executable Environment projection resolved for one Session creation.
pub struct ResolvedSessionEnvironment {
    pub snapshot: awaken_session_contract::EnvironmentSnapshot,
    pub self_hosted: bool,
}

/// Coordinator projection consumed by Session compilation.  Environment HTTP
/// routes may use the same concrete adapter, but their wire types do not cross
/// this port.
#[async_trait::async_trait]
pub trait SessionEnvironmentSource: Send + Sync {
    async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn enqueue_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError>;
}

/// Canonical application object shared by wire adapters.
pub struct SessionApplication {
    pub runtime: Arc<dyn SessionRuntime>,
    pub mcp_realizer: Arc<dyn McpAttachmentRealizer>,
    pub credential_source: Option<Arc<dyn SessionCredentialSource>>,
    pub repository_credential_ingress: Option<Arc<dyn RepositoryCredentialIngress>>,
    pub environments: Arc<dyn SessionEnvironmentSource>,
    pub config_source:
        Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    pub resource_catalog: Option<Arc<dyn awaken_resource_contract::ResourceCatalog>>,
    pub resource_purge_scheduler: Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    pub sessions_repo: Arc<dyn ManagedSessionRepository>,
    pub lifecycle_sink: Option<Arc<dyn SessionLifecycleSink>>,
    pub runtime_incarnation: String,
    lifecycle_supervisor_started: AtomicBool,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn McpAttachmentRealizer>,
        sessions_repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> Self {
        runtime.install_environment_binding_sink(Arc::new(RepositoryEnvironmentBindingSink::new(
            sessions_repo.clone(),
        )));
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Self {
            runtime,
            mcp_realizer,
            credential_source: None,
            repository_credential_ingress: None,
            environments,
            config_source: None,
            resource_catalog: None,
            resource_purge_scheduler: None,
            sessions_repo,
            lifecycle_sink: None,
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                APPLICATION_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            lifecycle_supervisor_started: AtomicBool::new(false),
        }
    }

    /// Claim the one lifecycle supervisor for this application instance.
    pub fn claim_lifecycle_supervisor(&self) -> bool {
        claim_once(&self.lifecycle_supervisor_started)
    }

    pub fn replace_repository(&mut self, repo: Arc<dyn ManagedSessionRepository>) {
        self.runtime.install_environment_binding_sink(Arc::new(
            RepositoryEnvironmentBindingSink::new(repo.clone()),
        ));
        self.sessions_repo = repo;
    }
}

fn claim_once(fence: &AtomicBool) -> bool {
    !fence.swap(true, Ordering::AcqRel)
}

/// Repository-backed implementation installed at the runtime materialization boundary.
pub struct RepositoryEnvironmentBindingSink {
    repo: Arc<dyn ManagedSessionRepository>,
}

impl RepositoryEnvironmentBindingSink {
    #[must_use]
    pub fn new(repo: Arc<dyn ManagedSessionRepository>) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentBindingSink for RepositoryEnvironmentBindingSink {
    async fn owns(&self, session_id: &str) -> bool {
        self.repo.owner(session_id).await.is_some()
    }

    async fn persist(&self, session_id: &str, binding: &str) -> Result<(), RunError> {
        const CAS_ATTEMPTS: usize = 3;
        for attempt in 0..CAS_ATTEMPTS {
            let owner = self.repo.owner(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            let mut session = self.repo.get(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            if session.environment.binding() == Some(binding) {
                return Ok(());
            }
            session.environment.set_resident(binding);
            let expected_revision = session.revision;
            let payload = awaken_session_contract::SessionMutationPayload::Replace(session);
            let payload_hash = payload.stable_hash();
            let mutation = awaken_session_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_contract::IdempotencyRecord {
                    key: format!(
                        "session:bind-environment:{session_id}:{}:{payload_hash}",
                        expected_revision.0
                    ),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            };
            match self
                .repo
                .commit_mutation(&owner, mutation)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?
            {
                awaken_session_contract::SessionMutationResult::Applied { .. }
                | awaken_session_contract::SessionMutationResult::Replayed { .. } => return Ok(()),
                awaken_session_contract::SessionMutationResult::Conflict { .. }
                    if attempt + 1 < CAS_ATTEMPTS => {}
                awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                    return Err(RunError::internal(
                        "Session environment binding CAS exhausted",
                    ));
                }
                awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                    return Err(RunError::internal(
                        "Session environment binding idempotency mismatch",
                    ));
                }
            }
        }
        Err(RunError::internal(
            "Session environment binding CAS exhausted",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_supervisor_claim_is_one_shot() {
        // Cause/effect decision table: C1=unclaimed fence, C2=already claimed.
        // R1 C1 -> E1 first caller becomes owner; R2 C2 -> E2 every later caller
        // is rejected. This proves moving the fence out of the protocol adapter
        // cannot start parallel Session lifecycle supervisors.
        let fence = AtomicBool::new(false);
        assert!(claim_once(&fence), "R1");
        assert!(!claim_once(&fence), "R2");
    }
}
