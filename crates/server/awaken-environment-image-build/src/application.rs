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
    pub retry_cap: Duration,
    pub wait_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for EnvironmentImageBuildPolicy {
    fn default() -> Self {
        Self {
            lease: Duration::from_secs(15 * 60),
            retry: Duration::from_secs(15),
            retry_cap: Duration::from_secs(6 * 60 * 60),
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
        base_image: Option<&str>,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
        if registration.definition.config.is_self_hosted()
            || registration.definition.config.packages().is_empty()
        {
            return Ok(None);
        }
        let base_image = registration.package_base_image(base_image.unwrap_or(&self.base_image));
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
        if let Some(demand) = self.demand(registration, None).await? {
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
        let failure = match self.builder.build(&claim.demand).await {
            Ok(image) => match self.builder.available(&image).await {
                Ok(true) => {
                    self.store
                        .complete(&claim, &image, crate::now_unix_ms())
                        .await?;
                    None
                }
                Ok(false) => Some("builder returned an unavailable image".to_owned()),
                Err(error) => Some(error.to_string()),
            },
            Err(error) => Some(error.to_string()),
        };
        if let Some(message) = failure {
            self.store
                .fail(
                    &claim,
                    &message,
                    crate::now_unix_ms(),
                    retry_delay_ms(&self.policy, claim.lease_epoch),
                )
                .await?;
        }
        Ok(true)
    }

    pub async fn run_worker(
        self: Arc<Self>,
        owner: impl Into<String>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        let owner = owner.into();
        loop {
            if cancellation.is_cancelled() {
                break;
            }
            if let Err(error) = self.run_once(&owner).await {
                eprintln!("Environment image-build worker failed: {error}");
            }
            tokio::select! {
                () = cancellation.cancelled() => break,
                () = tokio::time::sleep(self.policy.poll_interval) => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EnvironmentImageReadiness for EnvironmentImageBuildCoordinator {
    async fn ready_image(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        let Some(demand) = self.demand(registration, base_image).await? else {
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

    async fn ready_image_now(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: Option<&str>,
    ) -> Result<Option<String>, EnvironmentImageBuildError> {
        let Some(demand) = self.demand(registration, base_image).await? else {
            return Ok(None);
        };
        self.store
            .ensure(demand.clone(), crate::now_unix_ms())
            .await?;
        let Some(record) = self.store.get(&demand.build_key).await? else {
            return Ok(None);
        };
        let EnvironmentImageBuildState::Ready { image, .. } = record.state else {
            return Ok(None);
        };
        if self.builder.available(&image).await? {
            Ok(Some(image))
        } else {
            self.store
                .invalidate_ready(&demand.build_key, crate::now_unix_ms())
                .await?;
            Ok(None)
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

fn retry_delay_ms(policy: &EnvironmentImageBuildPolicy, attempt: u64) -> u64 {
    let base = millis(policy.retry);
    let cap = millis(policy.retry_cap).max(base);
    let exponent = u32::try_from(attempt.saturating_sub(1).min(63)).unwrap_or(63);
    base.saturating_mul(1_u64.checked_shl(exponent).unwrap_or(u64::MAX))
        .min(cap)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_environment_contract::{
        CreateEnvironmentCommand, EnvItem, EnvRegistry, EnvironmentConfig, EnvironmentPackages,
        EnvironmentRevision,
    };
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationSource;

    use super::*;
    use crate::InMemoryEnvironmentImageBuildStore;

    #[test]
    fn retry_delay_uses_attempt_backoff_and_a_bounded_cap() {
        // Cause/effect decision table for failed immutable build recipes:
        // | Rule | base | cap versus base | attempt | effect |
        // | R1 | 15s | above | 1 | retry after 15s |
        // | R2 | 15s | above | 2 | retry after 30s |
        // | R3 | 15s | above | very large | retry at the 6h cap without overflow |
        // | R4 | 15s | below | any | effective cap cannot shorten the base delay |
        // Constraints: attempt is the existing durable claim epoch (minimum one
        // after a claim); Failed remains recoverable, so no terminal state or
        // second scheduler is introduced. Effects cover prompt transient retry,
        // load shedding for persistent failures, and arithmetic saturation.
        let policy = EnvironmentImageBuildPolicy::default();
        assert_eq!(retry_delay_ms(&policy, 1), 15_000, "R1");
        assert_eq!(retry_delay_ms(&policy, 2), 30_000, "R2");
        assert_eq!(retry_delay_ms(&policy, u64::MAX), 21_600_000, "R3");

        let below_base_cap = EnvironmentImageBuildPolicy {
            retry_cap: Duration::from_secs(1),
            ..policy
        };
        assert_eq!(retry_delay_ms(&below_base_cap, u64::MAX), 15_000, "R4");
    }

    struct FakeBuilder {
        builds: Mutex<usize>,
    }

    #[derive(Clone, Copy)]
    enum FailureMode {
        Build,
        UnavailableImage,
        AvailabilityCheck,
    }

    struct FailingBuilder(FailureMode);

    #[async_trait]
    impl EnvironmentImageBuilder for FailingBuilder {
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
            match self.0 {
                FailureMode::Build => Err(EnvironmentImageBuildError::Unavailable(
                    "build failed".into(),
                )),
                FailureMode::UnavailableImage | FailureMode::AvailabilityCheck => {
                    Ok(format!("{}@sha256:candidate", demand.base_image))
                }
            }
        }

        async fn available(&self, _image: &str) -> Result<bool, EnvironmentImageBuildError> {
            match self.0 {
                FailureMode::Build => unreachable!("a failed build has no image to inspect"),
                FailureMode::UnavailableImage => Ok(false),
                FailureMode::AvailabilityCheck => Err(EnvironmentImageBuildError::Unavailable(
                    "availability failed".into(),
                )),
            }
        }
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
                description: None,
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
    async fn all_builder_failures_use_the_one_backoff_transition() {
        // Cause/effect graph: a claimed build can fail while building (C1),
        // return an image that is absent (C2), or fail while proving image
        // availability (C3). Each cause must produce the same authoritative
        // Failed transition (E1), preserve attempt=1 (E2), and schedule the
        // first retry at the policy base delay (E3). Decision-table rules
        // R1=C1, R2=C2, and R3=C3 cover every failure edge; successful Ready
        // behavior is covered by registration_worker_and_readiness_follow_one_demand_path.
        for (rule, mode, expected_message) in [
            ("R1", FailureMode::Build, "build failed"),
            (
                "R2",
                FailureMode::UnavailableImage,
                "builder returned an unavailable image",
            ),
            ("R3", FailureMode::AvailabilityCheck, "availability failed"),
        ] {
            let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
            let builder = Arc::new(FailingBuilder(mode));
            let coordinator = EnvironmentImageBuildCoordinator::new(
                store.clone(),
                builder,
                "registry/awaken:base",
                EnvironmentImageBuildPolicy::default(),
            )
            .unwrap();
            let packaged = registration(
                &format!("env-{rule}"),
                EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["@playwright/mcp@latest".into()],
                        ..Default::default()
                    },
                },
            );
            coordinator.ensure_registration(&packaged).await.unwrap();
            let demand = coordinator.demand(&packaged, None).await.unwrap().unwrap();
            let before = crate::now_unix_ms();
            assert!(coordinator.run_once("builder-a").await.unwrap(), "{rule}");
            let after = crate::now_unix_ms();
            let record = store.get(&demand.build_key).await.unwrap().unwrap();
            let EnvironmentImageBuildState::Failed {
                message,
                retry_at_ms,
                attempt,
            } = record.state
            else {
                panic!("{rule}: expected one Failed transition")
            };
            assert!(message.contains(expected_message), "{rule}: E1");
            assert_eq!(attempt, 1, "{rule}: E2");
            assert!(retry_at_ms >= before + 15_000, "{rule}: E3 lower bound");
            assert!(retry_at_ms <= after + 15_000, "{rule}: E3 upper bound");
        }
    }

    #[tokio::test]
    async fn registration_worker_and_readiness_follow_one_demand_path() {
        // FMECA: F1 every Environment is treated as an image build (S6/O4/D2,
        // RPN48) -> SelfHosted and package-free Cloud are excluded by the one
        // demand function; F2 two recipes alias one mutable tag (S9/O3/D5,
        // RPN135) -> base identity + exact revision/package facts form the build
        // key; F3 builder returns a tag that is not pullable (S8/O3/D4, RPN96)
        // -> availability must pass before Ready; F4 Session installs packages
        // again after image preparation (S7/O3/D4, RPN84) -> downstream freezes
        // the digest and clears runtime package requirements.
        // Cause/effect decision table: R0 a mutable operator base is resolved
        // before durable demand identity; R1 SelfHosted registration persists no
        // build demand; R2 package-free Cloud persists no demand; R3 packaged
        // Cloud persists one Pending demand before acknowledgement; R4 one worker
        // claim invokes the injected builder and records its immutable image; R5
        // Session readiness reuses that Ready result without a second build;
        // R6 an exact Session policy with a different base image creates and
        // reuses a distinct demand rather than aliasing the default build; R7 a
        // cancelled service token exits the worker without a detached loop.
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
        let cancellation = awaken_runtime_contract::CancellationToken::new();
        cancellation.cancel();
        coordinator
            .run_worker("builder-a", cancellation)
            .await
            .expect("R7 cancelled worker exits");
        assert_eq!(*builder.builds.lock().unwrap(), 2, "R7 performs no work");
    }

    #[tokio::test]
    async fn authored_packages_become_one_frozen_image_before_session_use() {
        // End-to-end FMECA for the Environment critical path:
        // F1 Control mutation is visible without an executable registration
        // (S9/O3/D5, RPN135) -> transactional intent + acknowledged registrar;
        // F2 package demand is built from mutable current state (S9/O3/D6,
        // RPN162) -> demand consumes the exact registered revision; F3 Session
        // starts before the image is Ready (S8/O4/D2, RPN64) -> snapshot
        // resolution is the readiness barrier; F4 archive leaves current
        // admission enabled (S10/O2/D4, RPN80) -> terminal Withdraw removes the
        // current projection while exact history remains.
        //
        // Cause/effect graph: C1=Cloud+packages; C2=registration acknowledged;
        // C3=build Pending; C4=verified Ready; C5=archive. Effects: E1=one build
        // demand; E2=no usable snapshot before Ready; E3=frozen digest in exact
        // snapshot; E4=current admission denied. Constraint: C1 is necessary for
        // E1 and C4 is necessary for E3.
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // | X1   | 1  | 1  | 1  | 0  | 0  | E1,E2 |
        // | X2   | 1  | 1  | 0  | 1  | 0  | E1,E3 |
        // | X3   | 1  | 1  | 0  | 1  | 1  | E3,E4 |
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
        let builder = Arc::new(FakeBuilder {
            builds: Mutex::new(0),
        });
        let builds = Arc::new(
            EnvironmentImageBuildCoordinator::new(
                store.clone(),
                builder,
                "registry/awaken:base",
                EnvironmentImageBuildPolicy {
                    wait_timeout: Duration::ZERO,
                    poll_interval: Duration::from_millis(1),
                    ..EnvironmentImageBuildPolicy::default()
                },
            )
            .unwrap(),
        );
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
            Arc::new(BuildAwareExecutableEnvironmentRegistrar::new(
                Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
                builds.clone(),
            ));
        let definitions: Arc<dyn EnvRegistry> =
            Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let control = awaken_environment_application::EnvironmentApplication::new(
            definitions,
            registrar,
            None,
        );
        let authored = control
            .create(CreateEnvironmentCommand {
                command_id: "full-environment-flow".into(),
                name: "browser".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::Cloud {
                    networking: Default::default(),
                    packages: EnvironmentPackages {
                        npm: vec!["@playwright/mcp@latest".into()],
                        ..Default::default()
                    },
                },
            })
            .await
            .expect("X1 authority and registration commit");
        let registered = catalog
            .current_registration(&authored.id)
            .await
            .unwrap()
            .expect("X1 executable projection");
        let demand = builds.demand(&registered, None).await.unwrap().unwrap();
        assert!(
            matches!(
                store.get(&demand.build_key).await.unwrap().unwrap().state,
                EnvironmentImageBuildState::Pending { .. }
            ),
            "X1/E1"
        );
        let execution =
            awaken_environment_execution_application::EnvironmentExecutionApplication::new(
                Arc::new(awaken_work_store::InMemoryWorkQueue::new()),
                catalog.clone(),
            )
            .with_image_readiness(builds.clone());
        assert!(
            matches!(
                execution.snapshot(&authored.id, None).await,
                Err(EnvironmentImageBuildError::Timeout(_))
            ),
            "X1/E2"
        );

        assert!(builds.run_once("builder-full-flow").await.unwrap(), "X2");
        let snapshot = execution
            .snapshot(&authored.id, None)
            .await
            .unwrap()
            .expect("X2 ready snapshot");
        assert_eq!(snapshot.revision, authored.revision, "X2 exact revision");
        assert_eq!(
            snapshot.prepared_image.as_deref(),
            Some("registry/awaken:base@sha256:resolved@sha256:ready"),
            "X2/E3"
        );

        control.archive(&authored.id).await.expect("X3 archive");
        assert!(
            execution
                .snapshot(&authored.id, None)
                .await
                .unwrap()
                .is_none(),
            "X3/E4"
        );
        assert!(
            catalog
                .registration_at_revision(&authored.id, authored.revision)
                .await
                .unwrap()
                .is_some(),
            "X3 exact history retained"
        );
    }
}
