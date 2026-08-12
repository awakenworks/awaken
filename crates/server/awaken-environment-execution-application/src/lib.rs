//! Coordinator application for executable Environment projection and WorkQueue coordination.
//!
//! The Managed protocol is an outer adapter. This crate is the sole owner of
//! Session snapshot compilation, executable-registration convergence, and
//! Environment-scoped work commands.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentNetworking};
use awaken_environment_realization_contract::{
    EnvironmentImageBuildError, EnvironmentImageReadiness,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentRegistrationSource, ExecutableEnvironmentWithdrawal,
    ExecutableEnvironmentWithdrawalOutcome,
};
use awaken_session_contract::work_queue::{
    HeartbeatResult, LeaseHeartbeat, QueueStats, WorkItem, WorkQueue, WorkQueueError,
};

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
    work: Arc<dyn WorkQueue>,
    execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    image_readiness: Option<Arc<dyn EnvironmentImageReadiness>>,
}

impl EnvironmentExecutionApplication {
    #[must_use]
    pub fn new(
        work: Arc<dyn WorkQueue>,
        execution_source: Arc<dyn ExecutableEnvironmentRegistrationSource>,
    ) -> Self {
        Self {
            work,
            execution_source,
            image_readiness: None,
        }
    }

    #[must_use]
    pub fn with_image_readiness(mut self, readiness: Arc<dyn EnvironmentImageReadiness>) -> Self {
        self.image_readiness = Some(readiness);
        self
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
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
        self.resolve_current_for_session(environment_id, runtime, &[])
            .await
    }

    pub async fn snapshot_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
        self.resolve_current_for_session(environment_id, runtime, mcp_targets)
            .await
    }

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
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
            true,
        )
        .await
    }

    pub async fn snapshot_exact(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
        self.resolve_exact_for_session(environment_id, revision, runtime, &[])
            .await
    }

    pub async fn snapshot_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
        self.resolve_exact_for_session(environment_id, revision, runtime, mcp_targets)
            .await
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError>
    {
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

    pub async fn list_work(
        &self,
        environment_id: &str,
    ) -> Result<Vec<WorkItem>, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self.work.list(environment_id).await?)
    }

    pub async fn claim_work(
        &self,
        environment_id: &str,
        worker_id: &str,
        now_ms: u64,
        reclaim_older_than_ms: Option<u64>,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self
            .work
            .claim_with_reclaim(environment_id, worker_id, now_ms, reclaim_older_than_ms)
            .await?)
    }

    pub async fn work_stats(
        &self,
        environment_id: &str,
        now_ms: u64,
    ) -> Result<QueueStats, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self.work.stats(environment_id, now_ms).await?)
    }

    pub async fn get_work(
        &self,
        environment_id: &str,
        work_id: &str,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self.work.get(environment_id, work_id).await?)
    }

    pub async fn update_work_metadata(
        &self,
        environment_id: &str,
        work_id: &str,
        patch: BTreeMap<String, String>,
    ) -> Result<Option<WorkItem>, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self
            .work
            .update_metadata(environment_id, work_id, patch)
            .await?)
    }

    pub async fn acknowledge_work(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_session_contract::work_queue::WorkMutationResult, EnvironmentExecutionError>
    {
        self.require_environment(environment_id).await?;
        Ok(self.work.ack(environment_id, work_id, worker_id).await?)
    }

    pub async fn heartbeat_work(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
        now_ms: u64,
        heartbeat: LeaseHeartbeat,
    ) -> Result<HeartbeatResult, EnvironmentExecutionError> {
        self.require_environment(environment_id).await?;
        Ok(self
            .work
            .heartbeat(environment_id, work_id, worker_id, now_ms, heartbeat)
            .await?)
    }

    pub async fn stop_work(
        &self,
        environment_id: &str,
        work_id: &str,
        worker_id: &str,
    ) -> Result<awaken_session_contract::work_queue::WorkMutationResult, EnvironmentExecutionError>
    {
        self.require_environment(environment_id).await?;
        Ok(self.work.stop(environment_id, work_id, worker_id).await?)
    }
}

#[async_trait::async_trait]
impl awaken_session_application::SessionEnvironmentSource for EnvironmentExecutionApplication {
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
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_application::ResolvedSessionEnvironment>,
        EnvironmentImageBuildError,
    > {
        Ok(
            Self::resolve_current_for_session(self, environment_id, runtime, mcp_targets)
                .await?
                .map(
                    |snapshot| awaken_session_application::ResolvedSessionEnvironment { snapshot },
                ),
        )
    }

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<
        Option<awaken_session_application::ResolvedSessionEnvironment>,
        EnvironmentImageBuildError,
    > {
        Ok(
            Self::resolve_exact_for_session(self, environment_id, revision, runtime, mcp_targets)
                .await?
                .map(
                    |snapshot| awaken_session_application::ResolvedSessionEnvironment { snapshot },
                ),
        )
    }

    async fn enqueue_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        self.work.enqueue_session(environment_id, session_id).await
    }

    async fn wake_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, WorkQueueError> {
        self.work.wake_session(environment_id, session_id).await
    }

    async fn retire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<Option<WorkItem>, WorkQueueError> {
        self.work.retire_session(environment_id, session_id).await
    }

    async fn acquire_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<Option<awaken_session_contract::work_queue::SessionWorkLease>, WorkQueueError> {
        self.work
            .acquire_session(environment_id, session_id, worker_owner, now_ms)
            .await
    }
}

/// Registration and WorkQueue intent converge before Control receives an acknowledgement.
pub struct CoordinatorEnvironmentRegistrar {
    delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
    work: Arc<dyn WorkQueue>,
}

impl CoordinatorEnvironmentRegistrar {
    #[must_use]
    pub fn new(
        delegate: Arc<dyn ExecutableEnvironmentRegistrar>,
        work: Arc<dyn WorkQueue>,
    ) -> Self {
        Self { delegate, work }
    }
}

#[async_trait::async_trait]
impl ExecutableEnvironmentRegistrar for CoordinatorEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = registration.definition.id.clone();
        let self_hosted = registration.definition.is_self_hosted();
        let outcome = self.delegate.register(registration).await?;
        if self_hosted {
            self.work
                .ensure_healthcheck(&environment_id)
                .await
                .map_err(|error| {
                    ExecutableEnvironmentRegistrationError::Unavailable(error.to_string())
                })?;
        }
        Ok(outcome)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        let environment_id = withdrawal.environment_id.clone();
        let outcome = self.delegate.withdraw(withdrawal).await?;
        self.work
            .remove_env(&environment_id)
            .await
            .map_err(|error| {
                ExecutableEnvironmentRegistrationError::Unavailable(error.to_string())
            })?;
        Ok(outcome)
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
    mcp_targets: &[awaken_session_contract::McpTarget],
    image_readiness: Option<&Arc<dyn EnvironmentImageReadiness>>,
    wait_for_image: bool,
) -> Result<Option<awaken_session_contract::EnvironmentSnapshot>, EnvironmentImageBuildError> {
    let item = &registration.definition;
    if item.archived_at.is_some() {
        return Ok(None);
    }
    let packages = item.config.packages();
    let network = session_network_policy(&item.config, mcp_targets);
    let acp = runtime.is_some_and(|value| value.starts_with("acp:"));
    let inference_holder = if acp {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Workload,
            "awaken.workload.acp",
        )
    } else {
        awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "awaken.worker",
        )
    };
    let credential_realization = awaken_credential_contract::CredentialRealizationProfile {
        inference_holder,
        mcp_holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
        resource_holder: awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            awaken_credential_contract::SELF_HOSTED_WORKER_TRUST_DOMAIN,
        ),
    };
    let (sandbox, sandbox_provisioning, idle_retention) = match &registration.sandbox_policy {
        Some(policy) if policy.disabled => return Ok(None),
        Some(policy) => {
            let Ok(config) = serde_json::to_value(&policy.config) else {
                return Ok(None);
            };
            (config, policy.provisioning, policy.idle_retention.clone())
        }
        None => (
            serde_json::json!({}),
            awaken_session_contract::SandboxProvisioning::Eager,
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
    let config_fingerprint = awaken_session_contract::EnvironmentFingerprint(
        awaken_session_contract::stable_fingerprint(&(
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
    Ok(Some(awaken_session_contract::EnvironmentSnapshot {
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
    }))
}

#[async_trait::async_trait]
impl awaken_session_contract::EnvironmentWarmupSource for EnvironmentExecutionApplication {
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_contract::EnvironmentSnapshot>, String> {
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
    mcp_targets: &[awaken_session_contract::McpTarget],
) -> awaken_session_contract::SessionNetworkPolicy {
    use awaken_session_contract::SessionNetworkPolicy;

    let EnvironmentNetworking::Limited {
        allowed_hosts,
        allow_mcp_servers,
        allow_package_managers,
    } = (match config {
        EnvironmentConfig::Cloud { networking, .. } => networking,
        EnvironmentConfig::SelfHosted => return SessionNetworkPolicy::Unrestricted,
    })
    else {
        return SessionNetworkPolicy::Unrestricted;
    };
    let mut hosts = allowed_hosts.clone();
    if *allow_mcp_servers {
        hosts.extend(mcp_targets.iter().filter_map(|target| {
            target.http_url().and_then(|url| {
                awaken_session_contract::McpTarget::identity(url)
                    .ok()
                    .map(|identity| identity.host)
            })
        }));
    }
    if *allow_package_managers {
        hosts.extend(
            awaken_environment_contract::PUBLIC_PACKAGE_REGISTRY_HOSTS
                .iter()
                .map(ToString::to_string),
        );
    }
    SessionNetworkPolicy::Allowlist { hosts }.normalized()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_environment_contract::{
        EnvironmentNetworking, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_session_contract::work_queue::HeartbeatCondition;
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
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config,
                sandbox_policy: None,
                archived_at: None,
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
        let work = Arc::new(InMemoryWorkQueue::new());
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
        let application =
            EnvironmentExecutionApplication::new(Arc::new(InMemoryWorkQueue::new()), catalog)
                .with_image_readiness(readiness.clone());
        let pending =
            awaken_session_contract::EnvironmentWarmupSource::current_environment_warmups(
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
        let ready = awaken_session_contract::EnvironmentWarmupSource::current_environment_warmups(
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
            awaken_session_contract::EnvironmentWarmupSource::current_environment_warmups(
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
            awaken_session_contract::EnvironmentWarmupSource::current_environment_warmups(
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
            awaken_session_contract::EnvironmentWarmupSource::current_environment_warmups(
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
        // Native/ACP runtime. Effects: E1 frozen network is canonical, E2 only
        // exact allowed hosts are added, E3 inference plaintext holder is Worker
        // for Native and Workload for ACP, E4 fingerprint changes with semantics.
        //
        // | Rule | kind/network | MCP exact | packages | runtime | effects |
        // | S1 | SelfHosted | n/a | n/a | Native | unrestricted, Worker |
        // | S2 | Cloud limited | disabled | disabled | Native | no network, Worker |
        // | S3 | Cloud limited | enabled+target | enabled | ACP | exact MCP+registries, Workload |
        // | S4 | Cloud unrestricted | n/a | n/a | Native | unrestricted, Worker |
        let (_, _, registrar, application) = fixture();
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
                        allowed_hosts: ["api.example.test".to_owned()].into_iter().collect(),
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
                    networking: EnvironmentNetworking::Unrestricted,
                    packages: EnvironmentPackages::default(),
                },
            ))
            .await
            .unwrap();

        let self_hosted = application.snapshot("self", None).await.unwrap().unwrap();
        assert_eq!(
            self_hosted.network,
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            "S1"
        );
        assert_eq!(
            self_hosted.credential_realization.inference_holder.boundary,
            awaken_credential_contract::PlaintextBoundary::Worker,
            "S1"
        );
        let closed = application.snapshot("closed", None).await.unwrap().unwrap();
        assert_eq!(
            closed.network,
            awaken_session_contract::SessionNetworkPolicy::None,
            "S2"
        );
        let target = awaken_session_contract::McpTarget::parse_http("https://Mcp.Example.test/rpc")
            .expect("valid MCP target");
        let exceptions = application
            .snapshot_for_session("exceptions", Some("acp:stdio"), &[target])
            .await
            .unwrap()
            .unwrap();
        let awaken_session_contract::SessionNetworkPolicy::Allowlist { hosts } =
            &exceptions.network
        else {
            panic!("S3 must compile an allowlist")
        };
        assert!(
            hosts.iter().any(|host| host == "api.example.test"),
            "S3 explicit host"
        );
        assert!(
            hosts.iter().any(|host| host == "mcp.example.test"),
            "S3 exact MCP host"
        );
        assert!(
            hosts.iter().any(|host| host == "registry.npmjs.org"),
            "S3 package registry"
        );
        assert_eq!(
            exceptions.credential_realization.inference_holder.boundary,
            awaken_credential_contract::PlaintextBoundary::Workload,
            "S3"
        );
        let open = application.snapshot("open", None).await.unwrap().unwrap();
        assert_eq!(
            open.network,
            awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            "S4"
        );
        assert_ne!(
            closed.config_fingerprint, exceptions.config_fingerprint,
            "E4"
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
            awaken_session_contract::SandboxProvisioning::Eager,
            "P1"
        );

        let v2 = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("policy-a".into()),
            version: SandboxExecutionPolicyVersion(1),
            config: SandboxOverride {
                isolation: Some(IsolationClass::Namespace),
                ..Default::default()
            },
            provisioning: awaken_session_contract::SandboxProvisioning::OnToolUse,
            idle_retention: Default::default(),
            disabled: false,
        };
        let mut revision_two = registration("policy", 2, EnvironmentConfig::SelfHosted);
        revision_two = ExecutableEnvironmentRegistration::new(revision_two.definition, Some(v2));
        registrar.register(revision_two).await.unwrap();
        let lazy = application.snapshot("policy", None).await.unwrap().unwrap();
        let frozen: SandboxOverride = serde_json::from_value(lazy.sandbox.clone()).unwrap();
        assert_eq!(frozen.isolation, Some(IsolationClass::Namespace), "P2");
        assert_eq!(
            lazy.sandbox_provisioning,
            awaken_session_contract::SandboxProvisioning::OnToolUse,
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
            awaken_session_contract::SandboxProvisioning::Eager,
            "P3"
        );

        let disabled = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("policy-a".into()),
            version: SandboxExecutionPolicyVersion(2),
            config: SandboxOverride::default(),
            provisioning: awaken_session_contract::SandboxProvisioning::Eager,
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
                application.list_work("missing").await,
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
                .get_work("worker", "missing")
                .await
                .unwrap()
                .is_none(),
            "W2"
        );
        let claimed = application
            .claim_work("worker", "worker-a", 10, None)
            .await
            .unwrap()
            .expect("healthcheck is claimable");
        let accepted = application
            .heartbeat_work(
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
            .heartbeat_work(
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
    }

    #[async_trait::async_trait]
    impl WorkQueue for FailingConvergenceQueue {
        async fn enqueue_session(
            &self,
            env: &str,
            session: &str,
        ) -> Result<String, WorkQueueError> {
            self.inner.enqueue_session(env, session).await
        }
        async fn wake_session(&self, env: &str, session: &str) -> Result<String, WorkQueueError> {
            self.inner.wake_session(env, session).await
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
        ) -> Result<awaken_session_contract::work_queue::WorkMutationResult, WorkQueueError>
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
        ) -> Result<awaken_session_contract::work_queue::WorkMutationResult, WorkQueueError>
        {
            self.inner.stop(env, work, worker).await
        }
        async fn retire_session(
            &self,
            env: &str,
            session: &str,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            self.inner.retire_session(env, session).await
        }
        async fn acquire_session(
            &self,
            env: &str,
            session: &str,
            worker: &str,
            now: u64,
        ) -> Result<Option<awaken_session_contract::work_queue::SessionWorkLease>, WorkQueueError>
        {
            self.inner.acquire_session(env, session, worker, now).await
        }
        async fn update_metadata(
            &self,
            env: &str,
            work: &str,
            patch: BTreeMap<String, String>,
        ) -> Result<Option<WorkItem>, WorkQueueError> {
            self.inner.update_metadata(env, work, patch).await
        }
        async fn stats(&self, env: &str, now: u64) -> Result<QueueStats, WorkQueueError> {
            self.inner.stats(env, now).await
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
            inner: InMemoryWorkQueue::new(),
            fail_ensure: AtomicBool::new(true),
            fail_list: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
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
        work.enqueue_session("converge", "session-a").await.unwrap();
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
        assert!(work.list("converge").await.unwrap().is_empty(), "C4");
    }

    struct FailingRegistrationSource;

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
            Arc::new(InMemoryWorkQueue::new()),
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
                catalog_failure.list_work("environment-a").await,
                Err(EnvironmentExecutionError::Registration(
                    ExecutableEnvironmentRegistrationError::Storage(_)
                ))
            ),
            "D1 list"
        );

        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let work = Arc::new(FailingConvergenceQueue {
            inner: InMemoryWorkQueue::new(),
            fail_ensure: AtomicBool::new(false),
            fail_list: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
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
                application.list_work("environment-a").await,
                Err(EnvironmentExecutionError::WorkQueue(
                    WorkQueueError::Storage(_)
                ))
            ),
            "D2"
        );
    }
}
