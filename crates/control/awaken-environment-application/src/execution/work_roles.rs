// @formal surface obligation=work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal surface obligation=work.terminal_lease_clear
// @formal reason: Narrow application roles preserve registration checks and delegate atomic storage operations.
//! Work use cases handed to adapters by their calling role.
use super::{EnvironmentExecutionApplication, EnvironmentExecutionError, session_access};
use awaken_executable_environment_contract::{
    self as storage, ClaimedWork, ExecutableEnvironmentRegistrationSource, HeartbeatResult,
    LeaseHeartbeat, QueueStats, WorkItem, WorkQueueError, WorkSessionAccess,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
struct WorkContext {
    execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
}
impl WorkContext {
    async fn require_environment(
        &self,
        environment_id: &str,
    ) -> Result<(), EnvironmentExecutionError> {
        if self
            .execution_source
            .current_registration(environment_id)
            .await?
            .is_some()
        {
            Ok(())
        } else {
            Err(EnvironmentExecutionError::EnvironmentNotFound)
        }
    }
}

/// Read Work items and queue statistics without acquiring authority.
///
/// A read-only adapter needs no claim, mutation or registration-write methods.
/// ```
/// use std::sync::Arc;
/// use awaken_environment_application::execution::WorkQueries;
/// use awaken_executable_environment_contract::{WorkQueries as QueryStorage, ExecutableEnvironmentRegistrationSource};
/// fn read_only(storage: Arc<dyn QueryStorage>, environments: Arc<dyn ExecutableEnvironmentRegistrationSource>) -> WorkQueries {
///     WorkQueries::new(storage, environments)
/// }
/// ```
///
/// ```compile_fail
/// use awaken_environment_application::execution::WorkQueries;
/// async fn cannot_stop(queries: WorkQueries) {
///     queries.stop("env", "work", "owner").await;
/// }
/// ```
#[derive(Clone)]
pub struct WorkQueries {
    context: WorkContext,
    port: Arc<dyn storage::WorkQueries>,
}
impl WorkQueries {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkQueries>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn list(
        &self,
        environment_id: &str,
    ) -> Result<Vec<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.list(environment_id).await?)
    }
    pub async fn stats(
        &self,
        environment_id: &str,
        now_ms: u64,
    ) -> Result<QueueStats, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.stats(environment_id, now_ms).await?)
    }
    pub async fn get(
        &self,
        environment_id: &str,
        work_id: &str,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.get(environment_id, work_id).await?)
    }
}

/// Claim work as an Environment Worker, issue its Session bearer, and mutate by credential owner.
#[derive(Clone)]
pub struct WorkConsumer {
    context: WorkContext,
    port: Arc<dyn storage::WorkConsumer>,
}
impl WorkConsumer {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkConsumer>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn claim(
        &self,
        environment_id: &str,
        lease_owner: &str,
        poller_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<ClaimedWork>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        let minted = session_access::mint_session_access();
        let claim = self
            .port
            .claim_with_session_access(
                environment_id,
                lease_owner,
                poller_id,
                now_ms,
                reclaim_older_than_ms,
                &minted.digest,
            )
            .await?;
        claim
            .map(|claim| {
                let sessions_token = match claim.session_access {
                    None => None,
                    Some(bound) if bound == minted.digest => Some(minted.cleartext),
                    Some(_) => {
                        return Err(EnvironmentExecutionError::WorkQueue(
                            WorkQueueError::Storage(
                                "WorkQueue bound a Session access digest it was not given".into(),
                            ),
                        ));
                    }
                };
                Ok(ClaimedWork {
                    item: claim.item,
                    sessions_token,
                })
            })
            .transpose()
    }
    pub async fn acknowledge(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.ack(environment_id, work_id, worker_id).await?)
    }
    pub async fn heartbeat(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self
            .port
            .heartbeat(environment_id, work_id, worker_id, now_ms, heartbeat)
            .await?)
    }
    pub async fn stop(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context.require_environment(environment_id).await?;
        Ok(self.port.stop(environment_id, work_id, worker_id).await?)
    }
}

/// Authenticate a Session bearer against the current durable Work lease.
///
/// ```compile_fail
/// use awaken_environment_application::execution::SessionAccessAuthenticator;
/// async fn cannot_claim(authentication: SessionAccessAuthenticator) {
///     authentication.claim("env", "owner", "poller", 0, None).await;
/// }
/// ```
#[derive(Clone)]
pub struct SessionAccessAuthenticator {
    port: Arc<dyn storage::SessionAccessAuthenticator>,
}
impl SessionAccessAuthenticator {
    /// Construct from the authentication capability alone.
    #[must_use]
    pub fn new(port: Arc<dyn storage::SessionAccessAuthenticator>) -> Self {
        Self { port }
    }
    pub async fn authenticate(
        &self,
        presented: &awaken_agent_contract::RedactedString,
        now_ms: u64,
    ) -> Result<Option<WorkSessionAccess>, EnvironmentExecutionError> {
        Ok(self
            .port
            .authenticate_session_access(&session_access::session_access_digest(presented), now_ms)
            .await?)
    }
}

/// Acknowledge, renew or stop only the exact lease epoch recovered from a Session bearer.
#[derive(Clone)]
pub struct SessionWorkLeaseControl {
    context: WorkContext,
    port: Arc<dyn storage::SessionWorkLeaseControl>,
}
impl SessionWorkLeaseControl {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::SessionWorkLeaseControl>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn acknowledge(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        Ok(self.port.ack_with_session_access(access).await?)
    }
    pub async fn heartbeat(
        &self,
        access: &WorkSessionAccess,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, EnvironmentExecutionError> {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        Ok(self
            .port
            .heartbeat_with_session_access(access, now_ms, heartbeat)
            .await?)
    }
    pub async fn stop(
        &self,
        access: &WorkSessionAccess,
    ) -> Result<awaken_executable_environment_contract::WorkMutationResult, EnvironmentExecutionError>
    {
        self.context
            .require_environment(&access.environment_id)
            .await?;
        Ok(self.port.stop_with_session_access(access).await?)
    }
}

/// Apply metadata upserts and removals; no lifecycle or lease mutation is exposed.
#[derive(Clone)]
pub struct WorkMetadataPatch {
    context: WorkContext,
    port: Arc<dyn storage::WorkMetadataPatch>,
}
impl WorkMetadataPatch {
    /// Construct with only this role and the Environment availability source.
    #[must_use]
    pub fn new(
        port: Arc<dyn storage::WorkMetadataPatch>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            port,
            context: WorkContext { execution_source },
        }
    }
    pub async fn apply(
        &self,
        environment_id: &str,
        work_id: &str,
        patch: BTreeMap<String, Option<String>>,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.context.require_environment(environment_id).await?;
        Ok(self
            .port
            .update_metadata(environment_id, work_id, patch)
            .await?)
    }
}

impl EnvironmentExecutionApplication {
    #[must_use]
    pub fn work_queries(&self) -> WorkQueries {
        WorkQueries::new(self.work.queries(), self.execution_source.clone())
    }
    #[must_use]
    pub fn work_consumer(&self) -> WorkConsumer {
        WorkConsumer::new(self.work.consumer(), self.execution_source.clone())
    }
    #[must_use]
    pub fn session_access(&self) -> SessionAccessAuthenticator {
        SessionAccessAuthenticator::new(self.work.authentication())
    }
    #[must_use]
    pub fn session_work_leases(&self) -> SessionWorkLeaseControl {
        SessionWorkLeaseControl::new(self.work.session_control(), self.execution_source.clone())
    }
    #[must_use]
    pub fn work_metadata(&self) -> WorkMetadataPatch {
        WorkMetadataPatch::new(self.work.metadata(), self.execution_source.clone())
    }
}
