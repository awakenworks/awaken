use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_environment_realization_contract::{
    EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuildState,
    EnvironmentImageBuildStore, EnvironmentImageBuilder, EnvironmentImageReadiness,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};

#[derive(Clone, Debug)]
pub struct EnvironmentImageBuildPolicy {
    pub lease: Duration,
    pub retry: Duration,
    pub wait_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for EnvironmentImageBuildPolicy {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(15 * 60),
            retry: Duration::from_secs(15),
            wait_timeout: Duration::from_secs(20 * 60),
            poll_interval: Duration::from_millis(250),
        }
    }
}

pub struct EnvironmentImageBuildCoordinator {
    store: Arc<dyn EnvironmentImageBuildStore>,
    builder: Arc<dyn EnvironmentImageBuilder>,
    base_image: String,
    policy: EnvironmentImageBuildPolicy,
}

impl EnvironmentImageBuildCoordinator {
    async fn demand(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: &str,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
        if registration.definition.config.is_self_hosted()
            || registration.definition.config.packages().is_empty()
        {
            return Ok(None);
        }
        let base_image = self.builder.base_image_identity(base_image).await?;
        Ok(EnvironmentImageBuildDemand::from_registration(
            registration,
            &base_image,
        ))
    }

    pub fn new(
        store: Arc<dyn EnvironmentImageBuildStore>,
        builder: Arc<dyn EnvironmentImageBuilder>,
        base_image: impl Into<String>,
        policy: EnvironmentImageBuildPolicy,
    ) -> Result<Self, EnvironmentImageBuildError> {
        let base_image = base_image.into();
        if base_image.trim().is_empty() {
            return Err(EnvironmentImageBuildError::Unavailable(
                "a package-image builder requires a base image".into(),
            ));
        }
        Ok(Self {
            store,
            builder,
            base_image,
            policy,
        })
    }

    pub async fn ensure_registration(
        &self,
        registration: &ExecutableEnvironmentRegistration,
    ) -> Result<(), EnvironmentImageBuildError> {
        if let Some(demand) = self.demand(registration, &self.base_image).await? {
            self.store.ensure(demand, crate::now_unix_ms()).await?;
        }
        Ok(())
    }

    pub async fn run_once(&self, owner: &str) -> Result<bool, EnvironmentImageBuildError> {
        let now = crate::now_unix_ms();
        let Some(claim) = self
            .store
            .claim_next(owner, now, millis(self.policy.lease))
            .await?
        else {
            return Ok(false);
        };
        match self.builder.build(&claim.demand).await {
            Ok(image) => match self.builder.available(&image).await {
                Ok(true) => {
                    self.store
                        .complete(&claim, &image, crate::now_unix_ms())
                        .await?;
                }
                Ok(false) => {
                    self.store
                        .fail(
                            &claim,
                            "builder returned an unavailable image",
                            crate::now_unix_ms(),
                            millis(self.policy.retry),
                        )
                        .await?;
                }
                Err(error) => {
                    self.store
                        .fail(
                            &claim,
                            &error.to_string(),
                            crate::now_unix_ms(),
                            millis(self.policy.retry),
                        )
                        .await?;
                }
            },
            Err(error) => {
                self.store
                    .fail(
                        &claim,
                        &error.to_string(),
                        crate::now_unix_ms(),
                        millis(self.policy.retry),
                    )
                    .await?;
            }
        }
        Ok(true)
    }

    pub fn spawn_worker(self: &Arc<Self>, owner: impl Into<String>) {
        let coordinator = self.clone();
        let owner = owner.into();
        tokio::spawn(async move {
            loop {
                if let Err(error) = coordinator.run_once(&owner).await {
                    eprintln!("Environment image-build worker failed: {error}");
                }
                tokio::time::sleep(coordinator.policy.poll_interval).await;
            }
        });
    }
}

#[async_trait]
impl EnvironmentImageReadiness for EnvironmentImageBuildCoordinator {
    async fn ready_image(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        let Some(demand) = self
            .demand(registration, base_image.unwrap_or(&self.base_image))
            .await?
        else {
            return Ok(None);
        };
        self.store
            .ensure(demand.clone(), crate::now_unix_ms())
            .await?;
        let deadline = tokio::time::Instant::now() + self.policy.wait_timeout;
        loop {
            if let Some(record) = self.store.get(&demand.build_key).await?
                && let EnvironmentImageBuildState::Ready { image, .. } = record.state
            {
                if self.builder.available(&image).await? {
                    return Ok(Some(image));
                }
                self.store
                    .invalidate_ready(&demand.build_key, crate::now_unix_ms())
                    .await?;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(EnvironmentImageBuildError::Timeout(format!(
                    "Environment `{}` revision {}",
                    demand.environment_id, demand.source_revision.0
                )));
            }
            tokio::time::sleep(self.policy.poll_interval).await;
        }
    }
}

pub struct BuildAwareExecutableEnvironmentRegistrar {
    registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
    builds: Arc<EnvironmentImageBuildCoordinator>,
}

impl BuildAwareExecutableEnvironmentRegistrar {
    #[must_use]
    pub fn new(
        registrar: Arc<dyn ExecutableEnvironmentRegistrar>,
        builds: Arc<EnvironmentImageBuildCoordinator>,
    ) -> Self {
        Self { registrar, builds }
    }
}

#[async_trait]
impl ExecutableEnvironmentRegistrar for BuildAwareExecutableEnvironmentRegistrar {
    async fn register(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
    {
        let outcome = self.registrar.register(registration.clone()).await?;
        self.builds
            .ensure_registration(&registration)
            .await
            .map_err(|error| {
                ExecutableEnvironmentRegistrationError::Unavailable(error.to_string())
            })?;
        Ok(outcome)
    }

    async fn withdraw(
        &self,
        withdrawal: ExecutableEnvironmentWithdrawal,
    ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
    {
        self.registrar.withdraw(withdrawal).await
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_environment_contract::{
        EnvItem, EnvironmentConfig, EnvironmentPackages, EnvironmentRevision,
    };
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationSource;

    use super::*;
    use crate::InMemoryEnvironmentImageBuildStore;

    struct FakeBuilder {
        builds: Mutex<usize>,
    }

    #[async_trait]
    impl EnvironmentImageBuilder for FakeBuilder {
        async fn base_image_identity(
            &self,
            reference: &str,
        ) -> Result<String, EnvironmentImageBuildError> {
            Ok(format!("{reference}@sha256:resolved"))
        }

        async fn build(
            &self,
            demand: &EnvironmentImageBuildDemand,
        ) -> Result<String, EnvironmentImageBuildError> {
            *self.builds.lock().unwrap() += 1;
            Ok(format!("{}@sha256:ready", demand.base_image))
        }

        async fn available(&self, image: &str) -> Result<bool, EnvironmentImageBuildError> {
            Ok(image.ends_with("@sha256:ready"))
        }
    }

    fn registration(
        environment_id: &str,
        config: EnvironmentConfig,
    ) -> ExecutableEnvironmentRegistration {
        ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: environment_id.into(),
                revision: EnvironmentRevision(1),
                name: "browser".into(),
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

    #[tokio::test]
    async fn registration_worker_and_readiness_follow_one_demand_path() {
        // Cause/effect decision table: R0 a mutable operator base is resolved
        // before durable demand identity; R1 SelfHosted registration persists no
        // build demand; R2 package-free Cloud persists no demand; R3 packaged
        // Cloud persists one Pending demand before acknowledgement; R4 one worker
        // claim invokes the injected builder and records its immutable image; R5
        // Session readiness reuses that Ready result without a second build;
        // R6 an exact Session policy with a different base image creates and
        // reuses a distinct demand rather than aliasing the default build.
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
        let builder = Arc::new(FakeBuilder {
            builds: Mutex::new(0),
        });
        let coordinator = Arc::new(
            EnvironmentImageBuildCoordinator::new(
                store.clone(),
                builder.clone(),
                "registry/awaken:base",
                EnvironmentImageBuildPolicy {
                    wait_timeout: Duration::from_secs(1),
                    poll_interval: Duration::from_millis(1),
                    ..EnvironmentImageBuildPolicy::default()
                },
            )
            .unwrap(),
        );
        let registrar = BuildAwareExecutableEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            coordinator.clone(),
        );
        registrar
            .register(registration(
                "env-self-hosted",
                EnvironmentConfig::SelfHosted,
            ))
            .await
            .unwrap();
        registrar
            .register(registration(
                "env-plain",
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: Default::default(),
                },
            ))
            .await
            .unwrap();
        let packaged = registration(
            "env-browser",
            EnvironmentConfig::Cloud {
                networking: Default::default(),
                packages: EnvironmentPackages {
                    npm: vec!["@playwright/mcp@latest".into()],
                    ..Default::default()
                },
            },
        );
        registrar.register(packaged.clone()).await.unwrap();
        let demand = EnvironmentImageBuildDemand::from_registration(
            &packaged,
            "registry/awaken:base@sha256:resolved",
        )
        .unwrap();
        assert!(
            matches!(
                store.get(&demand.build_key).await.unwrap().unwrap().state,
                EnvironmentImageBuildState::Pending { .. }
            ),
            "R1-R3"
        );
        assert!(coordinator.run_once("builder-a").await.unwrap(), "R4");
        assert_eq!(
            coordinator.ready_image(&packaged, None).await.unwrap(),
            Some("registry/awaken:base@sha256:resolved@sha256:ready".into()),
            "R5"
        );
        assert_eq!(*builder.builds.lock().unwrap(), 1, "R5");
        let policy_demand = EnvironmentImageBuildDemand::from_registration(
            &packaged,
            "registry/policy-base@sha256:exact@sha256:resolved",
        )
        .unwrap();
        store
            .ensure(policy_demand, crate::now_unix_ms())
            .await
            .unwrap();
        assert!(coordinator.run_once("builder-a").await.unwrap(), "R6");
        assert_eq!(
            coordinator
                .ready_image(&packaged, Some("registry/policy-base@sha256:exact"))
                .await
                .unwrap(),
            Some("registry/policy-base@sha256:exact@sha256:resolved@sha256:ready".into()),
            "R6"
        );
        assert_eq!(*builder.builds.lock().unwrap(), 2, "R6");
        assert!(
            catalog
                .current_registration("env-browser")
                .await
                .unwrap()
                .is_some()
        );
    }
}
