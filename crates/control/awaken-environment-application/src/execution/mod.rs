// @formal entrypoint environment.cloud_native_credential_custody_selection_is_exact
//! Coordinator application for executable Environment projection and WorkQueue coordination.
//!
//! The Managed protocol is an outer adapter. This module is the sole owner of
//! Session snapshot compilation, executable-registration convergence, and
//! Environment-scoped work commands.

#[cfg(test)]
use std::collections::BTreeMap;
use std::sync::Arc;

mod work_ports;
pub use work_ports::WorkQueue;
mod work_roles;
pub use work_roles::{
    SessionAccessAuthenticator, SessionWorkLeaseControl, WorkConsumer, WorkMetadataPatch,
    WorkQueries,
};
mod session_access;
pub use session_access::SESSION_TOKEN_PREFIX;

use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentNetworking};
#[cfg(test)]
use awaken_executable_environment_contract::WorkItem;
use awaken_executable_environment_contract::WorkQueueError;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildError, EnvironmentImageReadiness,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentRegistrationSource, ExecutableEnvironmentWithdrawal,
    ExecutableEnvironmentWithdrawalOutcome,
};
#[cfg(test)]
use awaken_executable_environment_contract::{HeartbeatResult, LeaseHeartbeat, QueueStats};

/// Application-level failure returned to outer transports.
#[derive(Debug, thiserror::Error)]
pub enum EnvironmentExecutionError {
    #[error("environment not found")]
    EnvironmentNotFound,
    #[error(transparent)]
    Registration(#[from] ExecutableEnvironmentRegistrationError),
    #[error(transparent)]
    WorkQueue(#[from] WorkQueueError),
}

/// Coordinator-owned executable projection and WorkQueue application service.
pub struct EnvironmentExecutionApplication {
    work: WorkQueue,
    execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    image_readiness: Option<Arc<dyn EnvironmentImageReadiness>>,
    cloud_native_credential_realization:
        Option<awaken_credential_contract::CredentialRealizationProfile>,
}

impl EnvironmentExecutionApplication {
    #[must_use]
    pub fn new(
        work: impl Into<WorkQueue>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            work: work.into(),
            execution_source,
            image_readiness: None,
            cloud_native_credential_realization: None,
        }
    }

    #[must_use]
    pub fn with_image_readiness(mut self, readiness: Arc<dyn EnvironmentImageReadiness>) -> Self {
        self.image_readiness = Some(readiness);
        self
    }

    /// Install the deployment-owned exact credential profile for Cloud Native
    /// Environments. Self-hosted and ACP Sessions retain their canonical local
    /// profiles; this is normalization input, not a runtime fallback order.
    #[must_use]
    pub fn with_cloud_native_credential_realization(
        mut self,
        profile: awaken_credential_contract::CredentialRealizationProfile,
    ) -> Self {
        self.cloud_native_credential_realization = Some(profile);
        self
    }

    /// Read exact incarnation-qualified WorkQueue occupancy across every
    /// Environment. Expired but unreleased claims still count; the caller must
    /// preserve an error as unavailable rather than infer an empty owner.
    pub async fn has_worker_owner_claims(
        &self,
        worker_owner: &str,
    ) -> Result<bool, WorkQueueError> {
        self.work.observation.has_owner_claims(worker_owner).await
    }

    pub async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        self.execution_source
            .current_registration(environment_id)
            .await
            .map(|registration| registration.map(|registration| registration.definition))
    }

    pub async fn snapshot(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        self.resolve_current_for_session(environment_id, runtime, &[])
            .await
    }

    pub async fn snapshot_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        let registration = self
            .execution_source
            .current_registration(environment_id)
            .await
            .map_err(registration_unavailable)?;
        let Some(registration) = registration else {
            return Ok(None);
        };
        snapshot_from_registration(
            registration,
            runtime,
            mcp_targets,
            self.image_readiness.as_ref(),
            self.cloud_native_credential_realization.as_ref(),
            true,
        )
        .await
    }

    pub async fn snapshot_exact(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        self.resolve_exact_for_session(environment_id, revision, runtime, &[])
            .await
    }

    pub async fn snapshot_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        self.resolve_exact_for_session(environment_id, revision, runtime, mcp_targets)
            .await
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
        EnvironmentImageBuildError,
    > {
        // Current availability is a live deny overlay. Exact history remains
        // queryable, but withdrawal denies admission of a new Session.
        if self
            .execution_source
            .current_registration(environment_id)
            .await
            .map_err(registration_unavailable)?
            .is_none()
        {
            return Ok(None);
        }
        let registration = self
            .execution_source
            .registration_at_revision(
                environment_id,
                awaken_environment_contract::EnvironmentRevision(revision),
            )
            .await
            .map_err(registration_unavailable)?;
        let Some(registration) = registration else {
            return Ok(None);
        };
        snapshot_from_registration(
            registration,
            runtime,
            mcp_targets,
            self.image_readiness.as_ref(),
            self.cloud_native_credential_realization.as_ref(),
            true,
        )
        .await
    }

    pub async fn is_self_hosted(
        &self,
        environment_id: &str,
    ) -> Result<bool, ExecutableEnvironmentRegistrationError> {
        self.execution_source
            .current_registration(environment_id)
            .await
            .map(|registration| {
                registration.is_some_and(|registration| registration.definition.is_self_hosted())
            })
    }
}

#[async_trait::async_trait]
impl awaken_executable_environment_contract::SessionEnvironmentProjectionSource
    for EnvironmentExecutionApplication
{
    type Snapshot = awaken_session_configuration_contract::EnvironmentSnapshot;

    async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        Self::get(self, environment_id).await
    }

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_executable_environment_contract::ResolvedSessionEnvironment<Self::Snapshot>>,
        EnvironmentImageBuildError,
    > {
        Ok(
            Self::resolve_current_for_session(self, environment_id, runtime, mcp_targets)
                .await?
                .map(|snapshot| {
                    awaken_executable_environment_contract::ResolvedSessionEnvironment { snapshot }
                }),
        )
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_agent_contract::McpTarget],
    ) -> Result<
        Option<awaken_executable_environment_contract::ResolvedSessionEnvironment<Self::Snapshot>>,
        EnvironmentImageBuildError,
    > {
        Ok(
            Self::resolve_exact_for_session(self, environment_id, revision, runtime, mcp_targets)
                .await?
                .map(|snapshot| {
                    awaken_executable_environment_contract::ResolvedSessionEnvironment { snapshot }
                }),
        )
    }
}

fn session_work_unavailable(error: WorkQueueError) -> awaken_session_types::RunError {
    let WorkQueueError::Storage(detail) = error;
    awaken_session_types::RunError::unavailable(detail.to_string())
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerAssignment
    for EnvironmentExecutionApplication
{
    async fn enqueue_session(
        &self,
        environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        self.work
            .assignment
            .enqueue_session(environment_id, session_id)
            .await
            .map_err(session_work_unavailable)
    }

    async fn wake_session(
        &self,
        environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, awaken_session_types::RunError> {
        self.work
            .assignment
            .wake_session(environment_id, session_id)
            .await
            .map_err(session_work_unavailable)
    }

    async fn retire_session(
        &self,
        environment_id: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(), awaken_session_types::RunError> {
        self.work
            .assignment
            .retire_session(environment_id, session_id)
            .await
            .map(|_| ())
            .map_err(session_work_unavailable)
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeases for EnvironmentExecutionApplication {
    async fn release_session(
        &self,
        lease: &awaken_session_types::SessionWorkLease,
    ) -> Result<bool, awaken_session_types::RunError> {
        self.work
            .worker_leases
            .release_session(lease)
            .await
            .map_err(session_work_unavailable)
    }

    async fn acquire_session(
        &self,
        environment_id: &str,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<awaken_session_types::SessionWorkLease>, awaken_session_types::RunError>
    {
        self.work
            .worker_leases
            .acquire_session(environment_id, session_id, worker_owner, now_ms)
            .await
            .map_err(session_work_unavailable)
    }

    async fn release_owner(
        &self,
        worker_owner: &str,
    ) -> Result<usize, awaken_session_types::RunError> {
        self.work
            .worker_leases
            .release_owner(worker_owner)
            .await
            .map_err(session_work_unavailable)
    }
}

/// Registration and WorkQueue intent converge before Control receives an acknowledgement.
pub struct CoordinatorEnvironmentRegistrar {
    delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
    work: Arc<dyn awaken_executable_environment_contract::EnvironmentWorkLifecycle>,
}

impl CoordinatorEnvironmentRegistrar {
    #[must_use]
    pub fn new(
        delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
        work: Arc<dyn awaken_executable_environment_contract::EnvironmentWorkLifecycle>,
    ) -> Self {
        Self { delegate, work }
    }
}

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for CoordinatorEnvironmentRegistrar {
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        // Retained frozen Sessions require history even if latest queue/build
        // convergence fails. This path can never advance a queue watermark.
        self.delegate
            .restore_historical_registration(registration)
            .await
    }

    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = registration.definition.id.clone();
        let revision = registration.definition.revision;
        let healthcheck = registration.definition.is_self_hosted();
        // Historical revisions must still rebuild immutable catalog snapshots.
        // Delegate includes image-build convergence; no watermark precedes it.
        let outcome = self.delegate.register(registration).await?;
        if outcome == ExecutableEnvironmentRegistrationOutcome::RegisteredHistorical {
            return Ok(outcome);
        }
        // A current duplicate must still repair an interrupted queue update.
        self.work.apply_environment_revision(
            &environment_id, revision,
            awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Register { healthcheck },
        ).await.map_err(registration_revision_error)?;
        Ok(outcome)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = withdrawal.environment_id.clone();
        let revision = withdrawal.lifecycle_revision;
        let outcome = self.delegate.withdraw(withdrawal).await?;
        if outcome == ExecutableEnvironmentWithdrawalOutcome::HistoricalNoop {
            return Ok(outcome);
        }
        self.work
            .apply_environment_revision(
                &environment_id,
                revision,
                awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Withdraw,
            )
            .await
            .map_err(registration_revision_error)?;
        Ok(outcome)
    }
}

fn registration_revision_error(
    error: awaken_executable_environment_contract::EnvironmentWorkRevisionError,
) -> ExecutableEnvironmentRegistrationError {
    use awaken_executable_environment_contract::EnvironmentWorkRevisionError;
    match error {
        EnvironmentWorkRevisionError::Invalid(reason) => {
            ExecutableEnvironmentRegistrationError::Invalid(reason.to_string())
        }
        conflict @ EnvironmentWorkRevisionError::Conflict { .. } => {
            ExecutableEnvironmentRegistrationError::Conflict(conflict.to_string())
        }
        EnvironmentWorkRevisionError::Storage(error) => {
            ExecutableEnvironmentRegistrationError::Unavailable(error.to_string())
        }
    }
}

fn registration_unavailable(
    error: ExecutableEnvironmentRegistrationError,
) -> EnvironmentImageBuildError {
    EnvironmentImageBuildError::Unavailable(error.to_string())
}

async fn snapshot_from_registration(
    registration: ExecutableEnvironmentRegistration,
    runtime: Option<&str>,
    mcp_targets: &[awaken_agent_contract::McpTarget],
    image_readiness: Option<&Arc<dyn EnvironmentImageReadiness>>,
    cloud_native_credential_realization: Option<
        &awaken_credential_contract::CredentialRealizationProfile,
    >,
    wait_for_image: bool,
) -> Result<
    Option<awaken_session_configuration_contract::EnvironmentSnapshot>,
    EnvironmentImageBuildError,
> {
    let item = &registration.definition;
    if item.archived_at.is_some() {
        return Ok(None);
    }
    let packages = item.config.packages();
    let acp = runtime.is_some_and(|value| value.starts_with("acp:"));
    let credential_realization = match awaken_credential_contract::credential_realization_selection(
        acp,
        matches!(item.config, EnvironmentConfig::Cloud { .. }),
        cloud_native_credential_realization.is_some(),
    ) {
        awaken_credential_contract::CredentialRealizationSelection::SelfHostedAcp => {
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp()
        }
        awaken_credential_contract::CredentialRealizationSelection::HostedCloud => {
            cloud_native_credential_realization
                .cloned()
                .expect("hosted profile selection requires an installed profile")
        }
        awaken_credential_contract::CredentialRealizationSelection::SelfHostedNative => {
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
        }
    };
    let network = session_network_policy(
        &item.config,
        mcp_targets,
        credential_realization.mcp_holder.boundary,
    );
    let (sandbox, sandbox_provisioning, idle_retention) = match &registration.sandbox_policy {
        Some(policy) if policy.disabled => return Ok(None),
        Some(policy) => (
            policy.config.clone(),
            policy.provisioning,
            policy.idle_retention.clone(),
        ),
        None => (
            Default::default(),
            awaken_session_configuration_contract::SandboxProvisioning::Eager,
            Default::default(),
        ),
    };
    let prepared_image = match image_readiness {
        Some(readiness) if wait_for_image => readiness.ready_image(&registration, None).await?,
        Some(readiness) => readiness.ready_image_now(&registration, None).await?,
        None => None,
    };
    // A package-bearing shape cannot be prewarmed against a mutable/unprepared
    // image while Coordinator owns image realization. Keep the previous receipt
    // until the exact immutable image becomes ready on a later reconciliation.
    if !wait_for_image
        && image_readiness.is_some()
        && !packages.is_empty()
        && prepared_image.is_none()
    {
        return Ok(None);
    }
    let self_hosted = item.is_self_hosted();
    let config_fingerprint = awaken_session_configuration_contract::EnvironmentFingerprint(
        awaken_agent_contract::stable_fingerprint(&(
            self_hosted,
            &sandbox,
            &sandbox_provisioning,
            &idle_retention,
            &packages,
            &network,
            &credential_realization,
            &prepared_image,
        )),
    );
    Ok(Some(
        awaken_session_configuration_contract::EnvironmentSnapshot {
            environment_id: item.id.clone(),
            revision: item.revision,
            self_hosted,
            config_fingerprint,
            sandbox,
            sandbox_provisioning,
            idle_retention,
            packages,
            prepared_image,
            network,
            credential_realization,
        },
    ))
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::EnvironmentWarmupSource
    for EnvironmentExecutionApplication
{
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String> {
        let registrations = self
            .execution_source
            .current_registrations()
            .await
            .map_err(|error| error.to_string())?;
        let mut warmups = Vec::new();
        for registration in registrations {
            if registration.definition.is_self_hosted() {
                continue;
            }
            if let Some(snapshot) = snapshot_from_registration(
                registration,
                None,
                &[],
                self.image_readiness.as_ref(),
                self.cloud_native_credential_realization.as_ref(),
                false,
            )
            .await
            .map_err(|error| error.to_string())?
            {
                warmups.push(snapshot);
            }
        }
        warmups.sort_by(|left, right| {
            left.environment_id
                .cmp(&right.environment_id)
                .then_with(|| left.revision.cmp(&right.revision))
        });
        Ok(warmups)
    }
}

fn session_network_policy(
    config: &EnvironmentConfig,
    mcp_targets: &[awaken_agent_contract::McpTarget],
    mcp_holder: awaken_credential_contract::PlaintextBoundary,
) -> awaken_session_configuration_contract::SessionNetworkPolicy {
    use awaken_session_configuration_contract::SessionNetworkPolicy;

    let EnvironmentNetworking::Limited {
        allow_mcp_servers, ..
    } = (match config {
        EnvironmentConfig::Cloud { networking, .. } => networking,
        EnvironmentConfig::SelfHosted => return SessionNetworkPolicy::Unrestricted,
    })
    else {
        return SessionNetworkPolicy::Unrestricted;
    };
    let mut hosts = config
        .runtime_network_hosts()
        .expect("limited config has a host projection");
    // Only a Workload-held MCP credential authorizes the Sandbox to dial its
    // upstream host. Worker/Platform custody must use their mediated route;
    // adding the upstream here would mint a signed direct CONNECT bypass.
    if *allow_mcp_servers && mcp_holder == awaken_credential_contract::PlaintextBoundary::Workload {
        hosts.extend(mcp_targets.iter().filter_map(|target| {
            target.http_url().and_then(|url| {
                awaken_agent_contract::McpTarget::identity(url)
                    .ok()
                    .map(|identity| identity.host)
            })
        }));
    }
    SessionNetworkPolicy::Allowlist { hosts }.normalized()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_executable_environment_contract::{
        EnvironmentWorkLifecycle, SessionWorkStore, SessionWorkerLeaseStore, WorkConsumer,
        WorkQueries, WorkerLeaseObservation,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_environment_contract::{
        EnvironmentNetworking, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_executable_environment_contract::HeartbeatCondition;
    use awaken_work_store::InMemoryWorkQueue;

    fn registration(
        id: &str,
        revision: u64,
        config: EnvironmentConfig,
    ) -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: id.into(),
                revision: EnvironmentRevision(revision),
                name: id.into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config,
                sandbox_policy: None,
                archived_at: None,
                created_at: Some("2026-02-03T04:05:06.007Z".into()),
                updated_at: Some("2026-02-03T04:05:06.007Z".into()),
            },
            None,
        )
    }

    fn fixture() -> (
        Arc<ExecutableEnvironmentCatalog>,
        Arc<InMemoryWorkQueue>,
        CoordinatorEnvironmentRegistrar,
        EnvironmentExecutionApplication,
    ) {
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
        let registrar = CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        );
        let application = EnvironmentExecutionApplication::new(work.clone(), catalog.clone());
        (catalog, work, registrar, application)
    }

    struct ToggleImageReadiness(AtomicBool);

    #[async_trait::async_trait]
    impl EnvironmentImageReadiness for ToggleImageReadiness {
        async fn ready_image(
            &self,
            _registration: &ExecutableEnvironmentRegistration,
            _base_image: Option<&str>,
        ) -> Result<Option<String>, EnvironmentImageBuildError> {
            Ok(Some("registry/env@sha256:ready".into()))
        }

        async fn ready_image_now(
            &self,
            _registration: &ExecutableEnvironmentRegistration,
            _base_image: Option<&str>,
        ) -> Result<Option<String>, EnvironmentImageBuildError> {
            Ok(self
                .0
                .load(Ordering::SeqCst)
                .then(|| "registry/env@sha256:ready".into()))
        }
    }

    #[tokio::test]
    async fn current_catalog_derives_nonblocking_warmup_demand() {
        // FMECA: F1 self-hosted Environment creates local capacity (S7 O3 D2,
        // RPN42); F2 package shape warms before immutable image Ready (S8 O4 D3,
        // RPN96); F3 image worker outage blocks unrelated package-free warmup
        // (S4 O3 D3, RPN36); F4 config revision leaves the old catalog entry as
        // desired (S5 O4 D2, RPN40). The catalog is the sole desired-state owner,
        // and image readiness is a non-blocking filter.
        // Cause graph: C1=current; C2=self-hosted; C3=packages; C4=image ready;
        // C5=new revision supersedes old; C6=old frozen Session resolves exact
        // history while the new image is pending. Effects: E1=emit snapshot;
        // E2=omit; E3=prepared image pinned; E4=only new revision emitted;
        // E5=old exact frozen snapshot remains executable during the warm gap.
        // | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect |
        // | W1   | 1  | 1  | -  | -  | 0  | 0  | E2     |
        // | W2   | 1  | 0  | 0  | -  | 0  | 0  | E1     |
        // | W3   | 1  | 0  | 1  | 0  | 0  | 0  | E2     |
        // | W4   | 1  | 0  | 1  | 1  | 0  | 0  | E1,E3  |
        // | W5   | -  | 0  | 0  | -  | 1  | 0  | E1,E4  |
        // | W6   | 1  | 0  | 1  | 0  | 1  | 1  | E2,E5  |
        // | W7   | 1  | 0  | 1  | 1  | 1  | 1  | E1,E3,E4,E5 |
        let (catalog, _, registrar, _) = fixture();
        registrar
            .register(registration("external", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        registrar
            .register(registration(
                "plain",
                1,
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: Default::default(),
                },
            ))
            .await
            .unwrap();
        registrar
            .register(registration(
                "packaged",
                1,
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["tsx@latest".into()],
                        ..Default::default()
                    },
                },
            ))
            .await
            .unwrap();
        let readiness = Arc::new(ToggleImageReadiness(AtomicBool::new(false)));
        let application = EnvironmentExecutionApplication::new(
            Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock())),
            catalog,
        )
        .with_image_readiness(readiness.clone());
        let pending =
            awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
                &application,
            )
            .await
            .unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|snapshot| snapshot.environment_id.as_str())
                .collect::<Vec<_>>(),
            ["plain"],
            "W1-W3"
        );
        readiness.0.store(true, Ordering::SeqCst);
        let ready = awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
            &application,
        )
        .await
        .unwrap();
        assert_eq!(ready.len(), 2, "W4");
        assert_eq!(
            ready
                .iter()
                .find(|snapshot| snapshot.environment_id == "packaged")
                .and_then(|snapshot| snapshot.prepared_image.as_deref()),
            Some("registry/env@sha256:ready"),
            "W4"
        );
        let frozen_packaged_v1 = application
            .snapshot_exact("packaged", 1, None)
            .await
            .unwrap()
            .expect("W4 exact frozen snapshot");
        registrar
            .register(registration(
                "plain",
                2,
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Vec::new(),
                        allow_mcp_servers: false,
                        allow_package_managers: false,
                    },
                    packages: Default::default(),
                },
            ))
            .await
            .unwrap();
        let revised =
            awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
                &application,
            )
            .await
            .unwrap();
        assert_eq!(
            revised
                .iter()
                .filter(|snapshot| snapshot.environment_id == "plain")
                .map(|snapshot| snapshot.revision.0)
                .collect::<Vec<_>>(),
            [2],
            "W5"
        );

        registrar
            .register(registration(
                "packaged",
                2,
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["tsx@next".into()],
                        ..Default::default()
                    },
                },
            ))
            .await
            .unwrap();
        readiness.0.store(false, Ordering::SeqCst);
        let rollout_pending =
            awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
                &application,
            )
            .await
            .unwrap();
        assert!(
            rollout_pending
                .iter()
                .all(|snapshot| snapshot.environment_id != "packaged"),
            "W6 pending current shape is not warmed"
        );
        assert_eq!(
            application
                .snapshot_exact("packaged", 1, None)
                .await
                .unwrap(),
            Some(frozen_packaged_v1.clone()),
            "W6 E5 old frozen Session remains exact"
        );
        readiness.0.store(true, Ordering::SeqCst);
        let rollout_ready =
            awaken_session_configuration_contract::EnvironmentWarmupSource::current_environment_warmups(
                &application,
            )
            .await
            .unwrap();
        assert_eq!(
            rollout_ready
                .iter()
                .filter(|snapshot| snapshot.environment_id == "packaged")
                .map(|snapshot| snapshot.revision.0)
                .collect::<Vec<_>>(),
            [2],
            "W7 only ready current revision becomes desired"
        );
        assert_eq!(
            application
                .snapshot_exact("packaged", 1, None)
                .await
                .unwrap(),
            Some(frozen_packaged_v1),
            "W7 E5"
        );
    }

    #[tokio::test]
    async fn snapshot_compilation_follows_the_network_and_runtime_decision_table() {
        // Cause/effect graph: C1 Environment kind; C2 limited/unrestricted network;
        // C3 MCP exception and exact target; C4 package-manager exception; C5
        // Native/ACP runtime; C6 an exact hosted Cloud profile is installed.
        // Effects: E1 frozen network is canonical, E2 only exact allowed hosts are
        // added, E3 self-hosted/default Native selects Worker, E4 self-hosted
        // ACP selects Workload and its upstream, E5 Cloud execution selects its
        // installed profile without authorizing direct MCP upstream CONNECT,
        // E6 the exact holder changes the frozen fingerprint.
        //
        // | Rule | kind/network | runtime | hosted profile | effects |
        // | S1 | SelfHosted | Native | installed | unrestricted, E3 |
        // | S2 | Cloud limited | Native | absent | no network, E3 |
        // | S3 | Cloud limited+exceptions | ACP | installed | Gateway, no upstream, E5 |
        // | S4 | Cloud unrestricted | Native | installed | unrestricted, E5,E6 |
        // | S5 | SelfHosted | ACP | installed | E4 |
        // | S6 | Cloud limited+exceptions | ACP | absent | upstream, E4 |
        let (catalog, work, registrar, application) = fixture();
        registrar
            .register(registration("self", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        registrar
            .register(registration(
                "closed",
                1,
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: Default::default(),
                        allow_mcp_servers: false,
                        allow_package_managers: false,
                    },
                    packages: EnvironmentPackages::default(),
                },
            ))
            .await
            .unwrap();
        registrar
            .register(registration(
                "exceptions",
                1,
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Limited {
                        allowed_hosts: [
                            "api.example.test".to_owned(),
                            "relay.awaken-cloud.svc".to_owned(),
                        ]
                        .into_iter()
                        .collect(),
                        allow_mcp_servers: true,
                        allow_package_managers: true,
                    },
                    packages: EnvironmentPackages::default(),
                },
            ))
            .await
            .unwrap();
        registrar
            .register(registration(
                "open",
                1,
                EnvironmentConfig::Cloud {
                    networking: EnvironmentNetworking::Unrestricted {},
                    packages: EnvironmentPackages::default(),
                },
            ))
            .await
            .unwrap();

        let platform_profile = awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: awaken_credential_contract::PlaintextHolder::new(
                awaken_credential_contract::PlaintextBoundary::Platform,
                "hosted.example.gateway",
            ),
            ..awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
        };
        let hosted_application = EnvironmentExecutionApplication::new(work, catalog)
            .with_cloud_native_credential_realization(platform_profile.clone());

        let self_hosted = hosted_application
            .snapshot("self", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            self_hosted.network,
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            "S1"
        );
        assert_eq!(
            self_hosted.credential_realization,
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            "S1"
        );
        let closed = application.snapshot("closed", None).await.unwrap().unwrap();
        assert_eq!(
            closed.network,
            awaken_session_configuration_contract::SessionNetworkPolicy::None,
            "S2"
        );
        assert_eq!(
            closed.credential_realization,
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
            "S2 E3"
        );
        let target = awaken_agent_contract::McpTarget::parse_http("https://Mcp.Example.test/rpc")
            .expect("valid MCP target");
        let exceptions = hosted_application
            .snapshot_for_session("exceptions", Some("acp:stdio"), &[target])
            .await
            .unwrap()
            .unwrap();
        let awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist { hosts } =
            &exceptions.network
        else {
            panic!("S3 must compile an allowlist")
        };
        assert!(
            hosts.iter().any(|host| host == "api.example.test"),
            "S3 explicit host"
        );
        assert!(
            hosts.iter().any(|host| host == "relay.awaken-cloud.svc"),
            "S3 exact Gateway host"
        );
        assert!(
            hosts.iter().all(|host| host != "mcp.example.test"),
            "S3 upstream must not enter the Sandbox CONNECT grant"
        );
        assert!(
            hosts.iter().any(|host| host == "registry.npmjs.org"),
            "S3 package registry"
        );
        assert_eq!(exceptions.credential_realization, platform_profile, "S3 E5");
        let standalone_cloud_acp = application
            .snapshot_for_session(
                "exceptions",
                Some("acp:stdio"),
                &[
                    awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/rpc")
                        .unwrap(),
                ],
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                standalone_cloud_acp.network,
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist { ref hosts }
                    if hosts.iter().any(|host| host == "mcp.example.test")
            ),
            "S6 Workload-held ACP keeps its exact direct MCP allowlist"
        );
        let standalone_acp = hosted_application
            .snapshot_for_session("self", Some("acp:stdio"), &[])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            standalone_acp.credential_realization,
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp(),
            "S5 E4"
        );
        let open = hosted_application
            .snapshot("open", None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            open.network,
            awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
            "S4 network"
        );
        assert_eq!(open.credential_realization, platform_profile, "S4 E5");
        assert_ne!(
            application
                .snapshot("open", None)
                .await
                .unwrap()
                .unwrap()
                .config_fingerprint,
            open.config_fingerprint,
            "S4 E6"
        );
    }

    #[tokio::test]
    async fn exact_resolution_applies_current_availability_as_a_deny_overlay() {
        // Cause/effect graph: C1 exact revision exists; C2 a current registration
        // exists; C3 withdrawal advances lifecycle. Effects: E1 exact admission
        // succeeds only under C1∧C2; E2 withdrawal preserves history but denies a
        // new Session. Decision rules: A1 T/T -> snapshot; A2 T/F -> None; A3
        // F/T -> None. This prevents stale Agent defaults bypassing archive.
        let (catalog, _, registrar, application) = fixture();
        registrar
            .register(registration("pinned", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        assert!(
            application
                .snapshot_exact("pinned", 1, None)
                .await
                .unwrap()
                .is_some(),
            "A1"
        );
        registrar
            .withdraw(ExecutableEnvironmentWithdrawal {
                environment_id: "pinned".into(),
                lifecycle_revision: EnvironmentRevision(2),
            })
            .await
            .unwrap();
        assert!(
            catalog
                .at_revision("pinned", EnvironmentRevision(1))
                .is_some(),
            "A2 history"
        );
        assert!(
            application
                .snapshot_exact("pinned", 1, None)
                .await
                .unwrap()
                .is_none(),
            "A2 deny"
        );
        assert!(
            application
                .snapshot_exact("missing", 1, None)
                .await
                .unwrap()
                .is_none(),
            "A3"
        );
    }

    #[tokio::test]
    async fn exact_sandbox_policy_facts_are_frozen_into_each_revision() {
        // Cause/effect graph: C1 no policy; C2 exact enabled policy with
        // provisioning/isolation; C3 a newer Environment revision carries a new
        // policy; C4 policy disabled. Effects: E1 C1 defaults Eager, E2 exact
        // config/provisioning enter the snapshot and fingerprint, E3 historical
        // revision keeps its old policy while current remains available, E4 C4
        // denies admission. Decision rules P1..P4 correspond to C1..C4.
        use awaken_provisioning_contract::{
            IsolationClass, SandboxExecutionPolicy, SandboxExecutionPolicyId,
            SandboxExecutionPolicyVersion, SandboxOverride,
        };

        let (_, _, registrar, application) = fixture();
        let base = registration("policy", 1, EnvironmentConfig::SelfHosted);
        registrar.register(base).await.unwrap();
        let eager = application.snapshot("policy", None).await.unwrap().unwrap();
        assert_eq!(
            eager.sandbox_provisioning,
            awaken_session_configuration_contract::SandboxProvisioning::Eager,
            "P1"
        );

        let v2 = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("policy-a".into()),
            version: SandboxExecutionPolicyVersion(1),
            config: SandboxOverride {
                isolation: Some(IsolationClass::Namespace),
                ..Default::default()
            },
            provisioning: awaken_session_configuration_contract::SandboxProvisioning::OnToolUse,
            idle_retention: Default::default(),
            disabled: false,
        };
        let mut revision_two = registration("policy", 2, EnvironmentConfig::SelfHosted);
        revision_two = ExecutableEnvironmentRegistration::new(revision_two.definition, Some(v2));
        registrar.register(revision_two).await.unwrap();
        let lazy = application.snapshot("policy", None).await.unwrap().unwrap();
        let frozen = lazy.sandbox.clone();
        assert_eq!(frozen.isolation, Some(IsolationClass::Namespace), "P2");
        assert_eq!(
            lazy.sandbox_provisioning,
            awaken_session_configuration_contract::SandboxProvisioning::OnToolUse,
            "P2"
        );
        assert_ne!(eager.config_fingerprint, lazy.config_fingerprint, "P2");
        assert_eq!(
            application
                .snapshot_exact("policy", 1, None)
                .await
                .unwrap()
                .unwrap()
                .sandbox_provisioning,
            awaken_session_configuration_contract::SandboxProvisioning::Eager,
            "P3"
        );

        let disabled = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("policy-a".into()),
            version: SandboxExecutionPolicyVersion(2),
            config: SandboxOverride::default(),
            provisioning: awaken_session_configuration_contract::SandboxProvisioning::Eager,
            idle_retention: Default::default(),
            disabled: true,
        };
        let mut revision_three = registration("policy", 3, EnvironmentConfig::SelfHosted);
        revision_three =
            ExecutableEnvironmentRegistration::new(revision_three.definition, Some(disabled));
        registrar.register(revision_three).await.unwrap();
        assert!(
            application
                .snapshot("policy", None)
                .await
                .unwrap()
                .is_none(),
            "P4"
        );
    }

    #[tokio::test]
    async fn work_commands_validate_environment_before_mutating_the_queue() {
        // Cause/effect graph: C1 current Environment exists; C2 work exists; C3
        // heartbeat precondition matches. Effects: E1 missing Environment is a
        // typed application error with no queue mutation; E2 missing work remains
        // an ordinary None for the HTTP adapter's work-404; E3 valid claim and
        // heartbeat use the one WorkQueue lease authority; E4 stale receipt is
        // rejected without extending the lease.
        //
        // | Rule | C1 | C2 | C3 | application result |
        // | W1 | F | - | - | EnvironmentNotFound |
        // | W2 | T | F | - | Ok(None) |
        // | W3 | T | T | T | Accepted |
        // | W4 | T | T | F | PreconditionFailed |
        let (_, _, registrar, application) = fixture();
        assert!(
            matches!(
                application.work_queries().list("missing").await,
                Err(EnvironmentExecutionError::EnvironmentNotFound)
            ),
            "W1"
        );
        registrar
            .register(registration("worker", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        assert!(
            application
                .work_queries()
                .get("worker", "missing")
                .await
                .unwrap()
                .is_none(),
            "W2"
        );
        let claimed = application
            .work_consumer()
            .claim("worker", "worker-a", "worker-a", 10, None)
            .await
            .unwrap()
            .expect("healthcheck is claimable")
            .item;
        let accepted = application
            .work_consumer()
            .heartbeat(
                "worker",
                &claimed.id,
                "worker-a",
                11,
                LeaseHeartbeat {
                    condition: HeartbeatCondition::First,
                    desired_ttl_seconds: Some(30),
                },
            )
            .await
            .unwrap();
        assert!(matches!(accepted, HeartbeatResult::Accepted(_)), "W3");
        let stale = application
            .work_consumer()
            .heartbeat(
                "worker",
                &claimed.id,
                "worker-a",
                12,
                LeaseHeartbeat {
                    condition: HeartbeatCondition::First,
                    desired_ttl_seconds: Some(30),
                },
            )
            .await
            .unwrap();
        assert!(matches!(stale, HeartbeatResult::PreconditionFailed), "W4");
    }

    struct FailingConvergenceQueue {
        inner: InMemoryWorkQueue,
        fail_ensure: AtomicBool,
        fail_list: AtomicBool,
        fail_remove: AtomicBool,
        fail_session_operations: AtomicBool,
    }

    impl FailingConvergenceQueue {
        fn require_session_storage(&self, operation: &str) -> Result<(), WorkQueueError> {
            if self.fail_session_operations.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage(
                    format!("session storage outage: {operation}").into(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::WorkQueries for FailingConvergenceQueue {
        async fn list(&self, env: &str) -> Result<Vec<WorkItem>, WorkQueueError> {
            if self.fail_list.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage("list outage".into()))
            } else {
                self.inner.list(env).await
            }
        }
        async fn get(&self, env: &str, work: &str) -> Result<Option<WorkItem>, WorkQueueError> {
            self.inner.get(env, work).await
        }
        async fn stats(&self, env: &str, now: u64) -> Result<QueueStats, WorkQueueError> {
            self.inner.stats(env, now).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::WorkConsumer for FailingConvergenceQueue {
        async fn claim_with_session_access(
            &self,
            env: &str,
            owner: &str,
            poller: &str,
            now: u64,
            reclaim: Option<u64>,
            digest: &awaken_executable_environment_contract::SessionAccessDigest,
        ) -> Result<Option<awaken_executable_environment_contract::WorkClaim>, WorkQueueError>
        {
            self.inner
                .claim_with_session_access(env, owner, poller, now, reclaim, digest)
                .await
        }
        async fn claim_with_reclaim(
            &self,
            env: &str,
            lease_owner: &str,
            poller_id: &str,
            now: u64,
            reclaim_older_than_ms: Option<u64>,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            // The port requires this: a decorator that forwarded only `claim`
            // would drop the reclaim age and the poll record silently.
            self.inner
                .claim_with_reclaim(env, lease_owner, poller_id, now, reclaim_older_than_ms)
                .await
        }
        async fn claim(
            &self,
            env: &str,
            worker: &str,
            now: u64,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            self.inner.claim(env, worker, now).await
        }
        async fn ack(
            &self,
            env: &str,
            work: &str,
            worker: &str,
        ) -> Result<awaken_executable_environment_contract::WorkMutationResult, WorkQueueError>
        {
            self.inner.ack(env, work, worker).await
        }
        async fn heartbeat(
            &self,
            env: &str,
            work: &str,
            worker: &str,
            now: u64,
            heartbeat: LeaseHeartbeat,
        ) -> Result<HeartbeatResult, WorkQueueError> {
            self.inner
                .heartbeat(env, work, worker, now, heartbeat)
                .await
        }
        async fn stop(
            &self,
            env: &str,
            work: &str,
            worker: &str,
        ) -> Result<awaken_executable_environment_contract::WorkMutationResult, WorkQueueError>
        {
            self.inner.stop(env, work, worker).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::SessionAccessAuthenticator
        for FailingConvergenceQueue
    {
        async fn authenticate_session_access(
            &self,
            digest: &awaken_executable_environment_contract::SessionAccessDigest,
            now: u64,
        ) -> Result<Option<awaken_executable_environment_contract::WorkSessionAccess>, WorkQueueError>
        {
            self.inner.authenticate_session_access(digest, now).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::SessionWorkLeaseControl for FailingConvergenceQueue {
        async fn ack_with_session_access(
            &self,
            access: &awaken_executable_environment_contract::WorkSessionAccess,
        ) -> Result<awaken_executable_environment_contract::WorkMutationResult, WorkQueueError>
        {
            self.inner.ack_with_session_access(access).await
        }
        async fn heartbeat_with_session_access(
            &self,
            access: &awaken_executable_environment_contract::WorkSessionAccess,
            now: u64,
            heartbeat: LeaseHeartbeat,
        ) -> Result<HeartbeatResult, WorkQueueError> {
            self.inner
                .heartbeat_with_session_access(access, now, heartbeat)
                .await
        }
        async fn stop_with_session_access(
            &self,
            access: &awaken_executable_environment_contract::WorkSessionAccess,
        ) -> Result<awaken_executable_environment_contract::WorkMutationResult, WorkQueueError>
        {
            self.inner.stop_with_session_access(access).await
        }
    }

    #[async_trait::async_trait]
    impl SessionWorkStore for FailingConvergenceQueue {
        async fn enqueue_session(
            &self,
            env: &str,
            session: &awaken_session_types::SessionId,
        ) -> Result<String, WorkQueueError> {
            self.require_session_storage("enqueue")?;
            self.inner.enqueue_session(env, session).await
        }
        async fn wake_session(
            &self,
            env: &str,
            session: &awaken_session_types::SessionId,
        ) -> Result<String, WorkQueueError> {
            self.require_session_storage("wake")?;
            self.inner.wake_session(env, session).await
        }
        async fn retire_session(
            &self,
            env: &str,
            session: &awaken_session_types::SessionId,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            self.require_session_storage("retire")?;
            self.inner.retire_session(env, session).await
        }
    }

    #[async_trait::async_trait]
    impl SessionWorkerLeaseStore for FailingConvergenceQueue {
        async fn release_session(
            &self,
            lease: &awaken_session_types::SessionWorkLease,
        ) -> Result<bool, WorkQueueError> {
            self.require_session_storage("release")?;
            self.inner.release_session(lease).await
        }
        async fn acquire_session(
            &self,
            env: &str,
            session: &awaken_session_types::SessionId,
            worker: &str,
            now: u64,
        ) -> Result<Option<awaken_session_types::SessionWorkLease>, WorkQueueError> {
            self.require_session_storage("acquire")?;
            self.inner.acquire_session(env, session, worker, now).await
        }
        async fn release_owner(&self, worker_owner: &str) -> Result<usize, WorkQueueError> {
            self.require_session_storage("release_owner")?;
            self.inner.release_owner(worker_owner).await
        }
    }

    #[async_trait::async_trait]
    impl WorkerLeaseObservation for FailingConvergenceQueue {
        async fn has_owner_claims(&self, worker_owner: &str) -> Result<bool, WorkQueueError> {
            self.inner.has_owner_claims(worker_owner).await
        }
    }

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::WorkMetadataPatch for FailingConvergenceQueue {
        async fn update_metadata(
            &self,
            env: &str,
            work: &str,
            patch: BTreeMap<String, Option<String>>,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            self.inner.update_metadata(env, work, patch).await
        }
    }

    #[async_trait::async_trait]
    impl EnvironmentWorkLifecycle for FailingConvergenceQueue {
        async fn apply_environment_revision(
            &self,
            env: &str,
            revision: awaken_environment_contract::EnvironmentRevision,
            operation: awaken_executable_environment_contract::EnvironmentWorkRevisionOperation,
        ) -> Result<
            awaken_executable_environment_contract::EnvironmentWorkRevisionOutcome,
            awaken_executable_environment_contract::EnvironmentWorkRevisionError,
        > {
            let fail = match operation {
                awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Register { .. } => self.fail_ensure.load(Ordering::SeqCst),
                awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Withdraw => self.fail_remove.load(Ordering::SeqCst),
            };
            if fail {
                Err(WorkQueueError::Storage(
                    match operation {
                    awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Register { .. } => "ensure outage",
                    awaken_executable_environment_contract::EnvironmentWorkRevisionOperation::Withdraw => "remove outage",
                }.into(),
                ).into())
            } else {
                self.inner
                    .apply_environment_revision(env, revision, operation)
                    .await
            }
        }
        async fn enqueue_healthcheck(&self, env: &str) -> Result<String, WorkQueueError> {
            self.inner.enqueue_healthcheck(env).await
        }
        async fn ensure_healthcheck(&self, env: &str) -> Result<String, WorkQueueError> {
            if self.fail_ensure.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage("ensure outage".into()))
            } else {
                self.inner.ensure_healthcheck(env).await
            }
        }
        async fn remove_env(&self, env: &str) -> Result<(), WorkQueueError> {
            if self.fail_remove.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage("remove outage".into()))
            } else {
                self.inner.remove_env(env).await
            }
        }
    }

    #[tokio::test]
    async fn session_ports_classify_storage_outages_without_widening_error_conversions() {
        use awaken_session_application_contract::{
            SessionWorkerAssignment as Assignment, SessionWorkerLeases as Leases,
        };
        use awaken_session_types::{RunErrorClassification, SessionId, SessionWorkLease};

        // E1 each Session operation preserves storage unavailability and its
        // operation-specific detail. E2 no failure grants authority or creates
        // work. E3 storage recovery permits the same enqueue to commit once.
        // Six independent failure rows cover enqueue/wake/retire/acquire/release/
        // release_owner; queue queries observe effects independently of errors.
        let work = Arc::new(FailingConvergenceQueue {
            inner: InMemoryWorkQueue::new(awaken_time::system_clock()),
            fail_ensure: AtomicBool::new(false),
            fail_list: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
            fail_session_operations: AtomicBool::new(true),
        });
        let application = EnvironmentExecutionApplication::new(
            work.clone(),
            Arc::new(ExecutableEnvironmentCatalog::new()),
        );
        let session = SessionId::from("session-a");
        let lease =
            SessionWorkLease::from_persisted("work", "env", session.clone(), "owner", 1, 100)
                .expect("nonzero observation");
        let errors = [
            (
                "enqueue",
                Assignment::enqueue_session(&application, "env", &session)
                    .await
                    .unwrap_err(),
            ),
            (
                "wake",
                Assignment::wake_session(&application, "env", &session)
                    .await
                    .unwrap_err(),
            ),
            (
                "retire",
                Assignment::retire_session(&application, "env", &session)
                    .await
                    .unwrap_err(),
            ),
            (
                "acquire",
                Leases::acquire_session(&application, "env", &session, "owner", 0)
                    .await
                    .unwrap_err(),
            ),
            (
                "release",
                Leases::release_session(&application, &lease)
                    .await
                    .unwrap_err(),
            ),
            (
                "release_owner",
                Leases::release_owner(&application, "owner")
                    .await
                    .unwrap_err(),
            ),
        ];
        for (operation, error) in errors {
            assert_eq!(
                error.classification(),
                RunErrorClassification::Unavailable,
                "{operation}"
            );
            assert_eq!(
                error.message,
                format!("session storage outage: {operation}")
            );
        }
        assert!(
            work.inner.list("env").await.unwrap().is_empty(),
            "E2 no assignment"
        );
        assert!(
            !work.inner.has_owner_claims("owner").await.unwrap(),
            "E2 no authority"
        );
        work.fail_session_operations.store(false, Ordering::SeqCst);
        let id = Assignment::enqueue_session(&application, "env", &session)
            .await
            .unwrap();
        assert_eq!(
            work.inner.list("env").await.unwrap().len(),
            1,
            "E3 committed once"
        );
        assert!(
            work.inner.get("env", &id).await.unwrap().is_some(),
            "E3 exact assignment"
        );
    }

    #[tokio::test]
    async fn registration_acknowledgement_waits_for_work_intent_convergence() {
        // Cause/effect graph: C1 catalog command commits; C2 queue convergence
        // fails; C3 the same command is replayed after recovery. Effects: E1 C2
        // returns Unavailable rather than false success, E2 catalog replay stays
        // idempotent, E3 register replay converges exactly one healthcheck, E4
        // withdrawal replay removes all work.
        //
        // | Rule | command | queue | acknowledgement | final queue |
        // | C1 | register | fail | Unavailable | empty |
        // | C2 | replay register | healthy | AlreadyRegistered | one healthcheck |
        // | C3 | withdraw | fail | Unavailable | unchanged |
        // | C4 | replay withdraw | healthy | AlreadyWithdrawn | empty |
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let work = Arc::new(FailingConvergenceQueue {
            inner: InMemoryWorkQueue::new(awaken_time::system_clock()),
            fail_ensure: AtomicBool::new(true),
            fail_list: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
            fail_session_operations: AtomicBool::new(false),
        });
        let registrar = CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        );
        let exact = registration("converge", 1, EnvironmentConfig::SelfHosted);
        assert!(
            matches!(
                registrar.register(exact.clone()).await,
                Err(ExecutableEnvironmentRegistrationError::Unavailable(_))
            ),
            "C1"
        );
        assert!(
            catalog.current("converge").is_some(),
            "C1 catalog committed"
        );
        assert!(work.list("converge").await.unwrap().is_empty(), "C1");
        work.fail_ensure.store(false, Ordering::SeqCst);
        assert_eq!(
            registrar.register(exact).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::AlreadyRegistered,
            "C2"
        );
        assert_eq!(work.list("converge").await.unwrap().len(), 1, "C2");
        work.enqueue_session(
            "converge",
            &awaken_session_types::SessionId::from("session-a"),
        )
        .await
        .unwrap();
        let withdrawal = ExecutableEnvironmentWithdrawal {
            environment_id: "converge".into(),
            lifecycle_revision: EnvironmentRevision(2),
        };
        work.fail_remove.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                registrar.withdraw(withdrawal.clone()).await,
                Err(ExecutableEnvironmentRegistrationError::Unavailable(_))
            ),
            "C3"
        );
        assert_eq!(work.list("converge").await.unwrap().len(), 2, "C3");
        work.fail_remove.store(false, Ordering::SeqCst);
        assert_eq!(
            registrar.withdraw(withdrawal).await.unwrap(),
            ExecutableEnvironmentWithdrawalOutcome::AlreadyWithdrawn,
            "C4"
        );
        let remaining = work.list("converge").await.unwrap();
        assert_eq!(
            remaining.len(),
            1,
            "C4 archive keeps the Session, removes healthcheck"
        );
        assert!(matches!(
            remaining[0].data,
            awaken_executable_environment_contract::WorkPayload::Session { .. }
        ));
    }

    struct FailingRegistrationSource;

    #[async_trait::async_trait]
    impl awaken_executable_environment_contract::ExecutableEnvironmentLifecycleSource
        for FailingRegistrationSource
    {
        async fn current_lifecycle(
            &self,
            _environment_id: &str,
        ) -> Result<
            Option<awaken_executable_environment_contract::ExecutableEnvironmentLifecycle>,
            ExecutableEnvironmentRegistrationError,
        > {
            Err(ExecutableEnvironmentRegistrationError::Storage(
                "catalog outage".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl ExecutableEnvironmentRegistrationSource for FailingRegistrationSource {
        async fn current_registration(
            &self,
            _environment_id: &str,
        ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>
        {
            Err(ExecutableEnvironmentRegistrationError::Storage(
                "catalog outage".into(),
            ))
        }

        async fn registration_at_revision(
            &self,
            _environment_id: &str,
            _revision: EnvironmentRevision,
        ) -> Result<Option<ExecutableEnvironmentRegistration>, ExecutableEnvironmentRegistrationError>
        {
            Err(ExecutableEnvironmentRegistrationError::Storage(
                "catalog outage".into(),
            ))
        }
    }

    #[tokio::test]
    async fn dependency_outages_never_masquerade_as_absence_or_an_empty_queue() {
        // Cause/effect graph: C1 catalog returns definitive None; C2 catalog
        // fails; C3 current Environment exists and WorkQueue fails. Effects: E1
        // C1 is EnvironmentNotFound, E2 C2 is Registration(Storage), E3 C3 is
        // WorkQueue(Storage); neither outage becomes false/None/empty. D1 C2 on
        // classification and list -> typed catalog error; D2 C3 -> typed queue
        // error. Definitive absence is covered by W1 in the work-command table.
        let catalog_failure = EnvironmentExecutionApplication::new(
            Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock())),
            Arc::new(FailingRegistrationSource),
        );
        assert!(
            matches!(
                catalog_failure.is_self_hosted("environment-a").await,
                Err(ExecutableEnvironmentRegistrationError::Storage(_))
            ),
            "D1 classification"
        );
        assert!(
            matches!(
                catalog_failure.work_queries().list("environment-a").await,
                Err(EnvironmentExecutionError::Registration(
                    ExecutableEnvironmentRegistrationError::Storage(_)
                ))
            ),
            "D1 list"
        );

        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let work = Arc::new(FailingConvergenceQueue {
            inner: InMemoryWorkQueue::new(awaken_time::system_clock()),
            fail_ensure: AtomicBool::new(false),
            fail_list: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
            fail_session_operations: AtomicBool::new(false),
        });
        let registrar = CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        );
        registrar
            .register(registration(
                "environment-a",
                1,
                EnvironmentConfig::SelfHosted,
            ))
            .await
            .unwrap();
        work.fail_list.store(true, Ordering::SeqCst);
        let application = EnvironmentExecutionApplication::new(work, catalog);
        assert!(
            matches!(
                application.work_queries().list("environment-a").await,
                Err(EnvironmentExecutionError::WorkQueue(
                    WorkQueueError::Storage(_)
                ))
            ),
            "D2"
        );
    }

    #[tokio::test]
    async fn session_access_is_minted_by_the_application_and_bound_by_the_queue() {
        // Cause/effect graph: C1 the claimed item is healthcheck or Session work;
        // C2 the presented bearer is the one returned by poll, another bearer, or
        // the first bearer after the lease was reclaimed. Effects: E1 healthcheck
        // claims return no bearer and bind no digest; E2 a Session claim returns a
        // bearer whose application-derived digest resolves to that exact lease;
        // E3 any other bearer resolves to nothing; E4 a reclaim binds a fresh
        // digest and fences the previous bearer.
        //
        // | Rule | C1 | C2 | Effect |
        // | A1 | healthcheck | - | E1 |
        // | A2 | Session | returned | E2 |
        // | A3 | Session | other | E3 |
        // | A4 | Session | reclaimed first | E4 |
        let (_, work, registrar, application) = fixture();
        registrar
            .register(registration("worker", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        let mut now = 0;
        let session_work = work
            .enqueue_session("worker", &awaken_session_types::SessionId::from("sesn_a"))
            .await
            .unwrap();
        let claimed = loop {
            let claimed = application
                .work_consumer()
                .claim("worker", "owner-a", "poller", now, None)
                .await
                .unwrap()
                .expect("queued work");
            if claimed.item.id == session_work {
                break claimed;
            }
            assert!(claimed.sessions_token.is_none(), "A1");
            assert!(
                work.stop("worker", &claimed.item.id, "owner-a")
                    .await
                    .unwrap()
                    .is_accepted()
            );
            now += 1;
        };
        let token = claimed.sessions_token.expect("A2 bearer");
        let access = application
            .session_access()
            .authenticate(&token, now)
            .await
            .unwrap()
            .expect("A2 bound digest");
        assert_eq!(
            (access.work_id.as_str(), access.lease_owner.as_str()),
            (session_work.as_str(), "owner-a"),
            "A2"
        );
        assert!(
            application
                .session_access()
                .authenticate(
                    &awaken_agent_contract::RedactedString::new("sk-ant-req-other"),
                    now,
                )
                .await
                .unwrap()
                .is_none(),
            "A3"
        );
        let reclaimed_at = now + awaken_work_store::LEASE_TTL_MS;
        let replacement = application
            .work_consumer()
            .claim("worker", "owner-b", "poller", reclaimed_at, None)
            .await
            .unwrap()
            .expect("A4 reclaim");
        let replacement_token = replacement.sessions_token.expect("A4 fresh bearer");
        assert!(
            application
                .session_access()
                .authenticate(&token, reclaimed_at)
                .await
                .unwrap()
                .is_none(),
            "A4 previous bearer fenced"
        );
        assert_eq!(
            application
                .session_access()
                .authenticate(&replacement_token, reclaimed_at)
                .await
                .unwrap()
                .expect("A4 replacement")
                .lease_owner,
            "owner-b"
        );
    }
    #[test]
    fn prepared_packages_do_not_grant_runtime_network_or_mcp_upstream_access() {
        // DT-RN: C1 valid declared npm dependency; C2 empty limited runtime
        // policy with both grants false; C3 an attached MCP upstream exists.
        // RN1 C1 does not change C2: execution compiles to None; RN2 C3 cannot
        // expand it. Package construction/readiness and CNI enforcement retain
        // their existing owners and independent qualification.
        let config = EnvironmentConfig::Cloud {
            networking: EnvironmentNetworking::Limited {
                allowed_hosts: Vec::new(),
                allow_mcp_servers: false,
                allow_package_managers: false,
            },
            packages: EnvironmentPackages {
                npm: vec!["tsx@4.0.0".into()],
                ..Default::default()
            },
        };
        config.validate().unwrap();
        let target =
            awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/rpc").unwrap();
        assert_eq!(
            session_network_policy(
                &config,
                &[target],
                awaken_credential_contract::PlaintextBoundary::Workload
            ),
            awaken_session_configuration_contract::SessionNetworkPolicy::None,
            "RN1-RN2"
        );
        assert!(
            !config.package_build_network_hosts().unwrap().is_empty(),
            "RN build policy remains independent"
        );
    }
    struct FailingCatalogBoundary {
        inner: LocalExecutableEnvironmentRegistrar,
        before: AtomicBool,
        after: AtomicBool,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ExecutableEnvironmentRegistrar for FailingCatalogBoundary {
        async fn restore_historical_registration(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<(), ExecutableEnvironmentRegistrationError> {
            self.inner
                .restore_historical_registration(registration)
                .await
        }
        async fn register(
            &self,
            registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.before.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "before catalog".into(),
                ));
            }
            let outcome = self.inner.register(registration).await?;
            // Same outer-boundary failure position as BuildAware's ensure_registration.
            if self.after.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "after catalog/build boundary".into(),
                ));
            }
            Ok(outcome)
        }
        async fn withdraw(
            &self,
            withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let outcome = self.inner.withdraw(withdrawal).await?;
            if self.after.load(Ordering::SeqCst) {
                return Err(ExecutableEnvironmentRegistrationError::Unavailable(
                    "after catalog".into(),
                ));
            }
            Ok(outcome)
        }
    }
    #[tokio::test]
    async fn catalog_and_build_failure_never_advance_queue_registration_and_retry_reexecutes_delegate()
     {
        for before in [false, true] {
            let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
            let delegate = Arc::new(FailingCatalogBoundary {
                inner: LocalExecutableEnvironmentRegistrar::new(catalog.clone()),
                before: AtomicBool::new(before),
                after: AtomicBool::new(!before),
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let registrar = CoordinatorEnvironmentRegistrar::new(delegate.clone(), work.clone());
            let exact = registration("failed-catalog", 2, EnvironmentConfig::SelfHosted);
            assert!(registrar.register(exact.clone()).await.is_err());
            assert!(work.list("failed-catalog").await.unwrap().is_empty());
            delegate.before.store(false, Ordering::SeqCst);
            delegate.after.store(false, Ordering::SeqCst);
            registrar.register(exact.clone()).await.unwrap();
            registrar.register(exact).await.unwrap();
            assert_eq!(
                delegate.calls.load(Ordering::SeqCst),
                3,
                "AlreadyApplied never skips catalog/build"
            );
            assert_eq!(work.list("failed-catalog").await.unwrap().len(), 1);
            let withdrawal = ExecutableEnvironmentWithdrawal {
                environment_id: "failed-catalog".into(),
                lifecycle_revision: EnvironmentRevision(3),
            };
            delegate.after.store(true, Ordering::SeqCst);
            assert!(registrar.withdraw(withdrawal.clone()).await.is_err());
            assert_eq!(
                work.list("failed-catalog").await.unwrap().len(),
                1,
                "failed delegate cannot advance watermark or purge"
            );
            delegate.after.store(false, Ordering::SeqCst);
            registrar.withdraw(withdrawal).await.unwrap();
            assert!(work.list("failed-catalog").await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn archived_session_work_and_bearer_continue_through_application_roles() {
        let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
        let session = work
            .enqueue_session("archived", &"existing".into())
            .await
            .unwrap();
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let registrar = CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        );
        registrar
            .register(registration("archived", 1, EnvironmentConfig::SelfHosted))
            .await
            .unwrap();
        let app = EnvironmentExecutionApplication::new(work, catalog);
        let claim = app
            .work_consumer()
            .claim("archived", "owner", "poller", 0, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.item.id, session);
        let token = claim.sessions_token.unwrap();
        let before = app
            .session_access()
            .authenticate(&token, 1)
            .await
            .unwrap()
            .unwrap();
        registrar
            .withdraw(ExecutableEnvironmentWithdrawal {
                environment_id: "archived".into(),
                lifecycle_revision: EnvironmentRevision(2),
            })
            .await
            .unwrap();
        assert!(
            app.work_queries()
                .get("archived", &session)
                .await
                .expect("archive must admit existing Session Work operations")
                .is_some()
        );
        assert_eq!(
            app.session_access().authenticate(&token, 1).await.unwrap(),
            Some(before.clone())
        );
        assert_eq!(app.work_queries().list("archived").await.unwrap().len(), 1);
        let stats = app.work_queries().stats("archived", 1).await.unwrap();
        assert_eq!(stats.depth, 0, "the retained Session is already claimed");
        assert_eq!(stats.pending, 1, "archive retains the in-flight Session");
        let patched = app
            .work_metadata()
            .apply(
                "archived",
                &session,
                BTreeMap::from([("archive".into(), Some("retained".into()))]),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            patched.metadata.get("archive").map(String::as_str),
            Some("retained")
        );
        assert!(
            app.get("archived").await.unwrap().is_none(),
            "new current admission stays withdrawn"
        );
        assert!(
            app.snapshot_exact("archived", 1, None)
                .await
                .unwrap()
                .is_none(),
            "history must not admit a new Session"
        );
        assert!(matches!(
            app.work_consumer()
                .heartbeat(
                    "archived",
                    &session,
                    "wrong-owner",
                    2,
                    LeaseHeartbeat::unconditional()
                )
                .await
                .unwrap(),
            HeartbeatResult::PreconditionFailed
        ));
        assert!(matches!(
            app.session_work_leases()
                .heartbeat(&before, 2, LeaseHeartbeat::unconditional())
                .await
                .unwrap(),
            HeartbeatResult::Accepted(_)
        ));
        assert!(
            app.session_work_leases()
                .acknowledge(&before)
                .await
                .unwrap()
                .is_accepted()
        );
        assert!(
            app.session_work_leases()
                .stop(&before)
                .await
                .unwrap()
                .is_accepted()
        );
        assert!(
            app.session_access()
                .authenticate(&token, 3)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            app.work_queries().list("unknown").await,
            Err(EnvironmentExecutionError::EnvironmentNotFound)
        ));
    }
}

#[cfg(test)]
mod revision_errors_tests;
