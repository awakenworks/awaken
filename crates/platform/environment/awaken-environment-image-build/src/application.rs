// @formal surface requirement=runtime.executable_snapshots.immutable_snapshot
// @formal reason: This source implements the repository-controlled snapshot product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_executable_environment_contract::{
    EnvironmentImageBuildDemand, EnvironmentImageBuildError, EnvironmentImageBuildState,
    EnvironmentImageBuildStore, EnvironmentImageBuilder, EnvironmentImageReadiness,
};
use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};
use awaken_time::{WallClock, WallClockExt};

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

// A transient observation of the original build record, not another state store.
enum ImageReadinessObservation {
    Pending,
    Ready(String),
    Failed,
}

pub struct EnvironmentImageBuildCoordinator {
    store: Arc<dyn EnvironmentImageBuildStore>,
    builder: Arc<dyn EnvironmentImageBuilder>,
    clock: Arc<dyn WallClock>,
    base_image: String,
    policy: EnvironmentImageBuildPolicy,
    transition_hint: std::sync::OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl EnvironmentImageBuildCoordinator {
    async fn demand(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: Option<&str>,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
        registration
            .validate()
            .map_err(|error| EnvironmentImageBuildError::Conflict(error.to_string()))?;
        if registration.definition.config.is_self_hosted()
            || registration.definition.config.packages().is_empty()
        {
            return Ok(None);
        }
        let base_image = registration.package_base_image(base_image.unwrap_or(&self.base_image));
        let binding_key = EnvironmentImageBuildDemand::binding_key(registration, base_image);
        if let Some(pinned) = self.store.pinned_demand(&binding_key).await? {
            return Ok(EnvironmentImageBuildDemand::from_registration(
                registration,
                &pinned.base_image,
            ));
        }
        let base_image = self.builder.base_image_identity(base_image).await?;
        Ok(EnvironmentImageBuildDemand::from_registration(
            registration,
            &base_image,
        ))
    }

    async fn ensure_demand(
        &self,
        registration: &ExecutableEnvironmentRegistration,
        base_image: Option<&str>,
    ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
        let Some(demand) = self.demand(registration, base_image).await? else {
            return Ok(None);
        };
        let base_reference =
            registration.package_base_image(base_image.unwrap_or(&self.base_image));
        let binding_key = EnvironmentImageBuildDemand::binding_key(registration, base_reference);
        // Job insertion and first binding selection are atomic in the existing
        // store. A racing resolver adopts the winner without orphan queue work.
        let selected = self
            .store
            .ensure(demand, self.clock.now_ms(), Some(&binding_key))
            .await?;
        Ok(EnvironmentImageBuildDemand::from_registration(
            registration,
            &selected.base_image,
        ))
    }

    pub fn new(
        store: Arc<dyn EnvironmentImageBuildStore>,
        builder: Arc<dyn EnvironmentImageBuilder>,
        base_image: impl Into<String>,
        policy: EnvironmentImageBuildPolicy,
        clock: Arc<dyn WallClock>,
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
            clock,
            base_image,
            policy,
            transition_hint: std::sync::OnceLock::new(),
        })
    }

    /// Install the process-local delivery hint used when an exact build-state
    /// CAS succeeds. The build store remains authoritative; this callback only
    /// wakes an existing dependent reconciliation owner so it need not wait for
    /// an unrelated global retry backoff.
    pub fn install_transition_hint(
        &self,
        hint: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), EnvironmentImageBuildError> {
        self.transition_hint.set(hint).map_err(|_| {
            EnvironmentImageBuildError::Conflict(
                "Environment image-build transition hint is already installed".into(),
            )
        })
    }

    pub async fn ensure_registration(
        &self,
        registration: &ExecutableEnvironmentRegistration,
    ) -> Result<(), EnvironmentImageBuildError> {
        self.ensure_demand(registration, None).await.map(|_| ())
    }

    async fn observe_image(
        &self,
        demand: &EnvironmentImageBuildDemand,
    ) -> Result<ImageReadinessObservation, EnvironmentImageBuildError> {
        let Some(record) = self.store.get(&demand.build_key).await? else {
            return Ok(ImageReadinessObservation::Pending);
        };
        Ok(match record.state {
            EnvironmentImageBuildState::Ready { image, .. } => {
                if self.builder.available(demand, &image).await? {
                    ImageReadinessObservation::Ready(image)
                } else {
                    self.store
                        .invalidate_ready(&demand.build_key, self.clock.now_ms())
                        .await?;
                    ImageReadinessObservation::Pending
                }
            }
            EnvironmentImageBuildState::Failed { .. } => ImageReadinessObservation::Failed,
            EnvironmentImageBuildState::Pending { .. }
            | EnvironmentImageBuildState::Building { .. } => ImageReadinessObservation::Pending,
        })
    }

    pub async fn run_once(&self, owner: &str) -> Result<bool, EnvironmentImageBuildError> {
        let now = self.clock.now_ms();
        let Some(claim) = self
            .store
            .claim_next(owner, now, millis(self.policy.lease))
            .await?
        else {
            return Ok(false);
        };
        let result = match self.builder.build(&claim.demand).await {
            Ok(image) => match self.builder.available(&claim.demand, &image).await {
                Ok(true) => Ok(image),
                Ok(false) => Err("builder returned an unavailable image".to_owned()),
                Err(error) => Err(error.to_string()),
            },
            Err(error) => Err(error.to_string()),
        };
        let transitioned = match result {
            Ok(image) => {
                self.store
                    .complete(&claim, &image, self.clock.now_ms())
                    .await?
            }
            Err(message) => {
                self.store
                    .fail(
                        &claim,
                        &message,
                        self.clock.now_ms(),
                        retry_delay_ms(&self.policy, claim.lease_epoch.get()),
                    )
                    .await?
            }
        };
        if transitioned && let Some(hint) = self.transition_hint.get() {
            hint();
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
        let Some(demand) = self.ensure_demand(registration, base_image).await? else {
            return Ok(None);
        };
        let deadline = awaken_time::mono::MonoDeadline::after(self.policy.wait_timeout);
        loop {
            match self.observe_image(&demand).await? {
                ImageReadinessObservation::Ready(image) => return Ok(Some(image)),
                ImageReadinessObservation::Failed => {
                    // Keep retry/backoff in the original queue and private
                    // provider diagnostics in its operator record.
                    return Err(EnvironmentImageBuildError::Unavailable(format!(
                        "Environment `{}` revision {} package-image build failed; Awaken retains the build task for retry",
                        demand.environment_id, demand.source_revision.0,
                    )));
                }
                ImageReadinessObservation::Pending => {}
            }
            if deadline.has_passed() {
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
        let Some(demand) = self.ensure_demand(registration, base_image).await? else {
            return Ok(None);
        };
        Ok(match self.observe_image(&demand).await? {
            ImageReadinessObservation::Ready(image) => Some(image),
            ImageReadinessObservation::Pending | ImageReadinessObservation::Failed => None,
        })
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
    async fn restore_historical_registration(
        &self,
        registration: ExecutableEnvironmentRegistration,
    ) -> Result<(), ExecutableEnvironmentRegistrationError> {
        self.registrar
            .restore_historical_registration(registration.clone())
            .await?;
        // Exact replay always re-establishes this revision's build demand.
        self.builds
            .ensure_registration(&registration)
            .await
            .map_err(|error| ExecutableEnvironmentRegistrationError::Unavailable(error.to_string()))
    }

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
mod admission_gate_tests;

#[cfg(test)]
mod frozen_identity_tests;

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use awaken_environment_contract::{
        CreateEnvironmentCommand, EnvItem, EnvironmentConfig, EnvironmentPackages,
        EnvironmentRevision,
    };
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationSource;
    use awaken_lease_testkit::ManualClock;

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
        available: AtomicBool,
        availability_checks: Mutex<Vec<(EnvironmentImageBuildDemand, String)>>,
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

        async fn available(
            &self,
            _demand: &EnvironmentImageBuildDemand,
            _image: &str,
        ) -> Result<bool, EnvironmentImageBuildError> {
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

        async fn available(
            &self,
            demand: &EnvironmentImageBuildDemand,
            image: &str,
        ) -> Result<bool, EnvironmentImageBuildError> {
            self.availability_checks
                .lock()
                .unwrap()
                .push((demand.clone(), image.to_owned()));
            Ok(self.available.load(Ordering::SeqCst) && image.ends_with("@sha256:ready"))
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
                created_at: Some("2026-02-03T04:05:06.007Z".into()),
                updated_at: Some("2026-02-03T04:05:06.007Z".into()),
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
        // R1=C1, R2=C2, and R3=C3 cover every failure edge. C4 the durable
        // Failed CAS succeeds; C5 no build can be claimed; C6 the injected
        // clock advances from registration at 1,000,000 to claim/failure at
        // 1,005,000. E4 emit one
        // dependency hint for C4 and none for C5. R4=C1|C2|C3+C4=>E4;
        // R5=C5=>no E4. Successful Ready hints are covered by
        // registration_worker_and_readiness_follow_one_demand_path.
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
            let clock = Arc::new(ManualClock::new(1_000_000));
            let coordinator = EnvironmentImageBuildCoordinator::new(
                store.clone(),
                builder,
                "registry/awaken:base",
                EnvironmentImageBuildPolicy::default(),
                clock.clone(),
            )
            .unwrap();
            let hints = Arc::new(AtomicUsize::new(0));
            let observed = hints.clone();
            coordinator
                .install_transition_hint(Arc::new(move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                }))
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
            clock.set(1_005_000);
            assert!(coordinator.run_once("builder-a").await.unwrap(), "{rule}");
            assert_eq!(hints.load(Ordering::SeqCst), 1, "{rule}: R4/E4");
            assert!(
                !coordinator.run_once("builder-a").await.unwrap(),
                "{rule}: R5"
            );
            assert_eq!(
                hints.load(Ordering::SeqCst),
                1,
                "{rule}: R5 no duplicate hint"
            );
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
            assert_eq!(retry_at_ms, 1_020_000, "{rule}: C6/E3 exact retry");
        }
    }

    #[tokio::test]
    async fn readiness_reports_failed_build_without_waiting_or_changing_its_retry_authority() {
        // Decision table: Pending/Building -> existing bounded Timeout; known
        // Failed -> Unavailable before that deadline, no build/state mutation;
        // original retry edge -> same queue claims/builds once -> exact Ready.
        // Zero wait budget makes error precedence deterministic without timing
        // assertions. A private provider message stays in the operator record.
        // Nonblocking capacity reads still return None for failed work.
        let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
        let builder = Arc::new(FakeBuilder {
            builds: Mutex::new(0),
            available: AtomicBool::new(true),
            availability_checks: Mutex::new(Vec::new()),
        });
        let clock = Arc::new(ManualClock::new(1_000));
        let coordinator = EnvironmentImageBuildCoordinator::new(
            store.clone(),
            builder.clone(),
            "registry/base",
            EnvironmentImageBuildPolicy {
                wait_timeout: Duration::ZERO,
                ..EnvironmentImageBuildPolicy::default()
            },
            clock.clone(),
        )
        .unwrap();
        let packaged = registration(
            "env-office",
            EnvironmentConfig::Cloud {
                networking: Default::default(),
                packages: EnvironmentPackages {
                    pip: vec!["generic-package==1.0".into()],
                    ..Default::default()
                },
            },
        );
        coordinator.ensure_registration(&packaged).await.unwrap();
        let demand = coordinator.demand(&packaged, None).await.unwrap().unwrap();
        assert!(
            matches!(
                coordinator.ready_image(&packaged, None).await,
                Err(EnvironmentImageBuildError::Timeout(_))
            ),
            "Pending"
        );
        let claim = store
            .claim_next("builder-a", clock.now_ms(), 10_000)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(
                coordinator.ready_image(&packaged, None).await,
                Err(EnvironmentImageBuildError::Timeout(_))
            ),
            "Building"
        );
        store
            .fail(
                &claim,
                "/private/provider/diagnostic-canary",
                clock.now_ms(),
                15,
            )
            .await
            .unwrap();
        let before = store.get(&demand.build_key).await.unwrap().unwrap();
        let error = coordinator.ready_image(&packaged, None).await.unwrap_err();
        assert!(matches!(error, EnvironmentImageBuildError::Unavailable(_)));
        assert!(error.to_string().contains("env-office"));
        assert!(!error.to_string().contains("diagnostic-canary"));
        assert_eq!(
            coordinator.ready_image_now(&packaged, None).await.unwrap(),
            None
        );
        assert_eq!(store.get(&demand.build_key).await.unwrap().unwrap(), before);
        assert_eq!(*builder.builds.lock().unwrap(), 0);
        assert!(
            !coordinator.run_once("builder-b").await.unwrap(),
            "backoff preserved"
        );
        clock.set(1_015);
        assert!(
            coordinator.run_once("builder-b").await.unwrap(),
            "same owner queue retries"
        );
        assert_eq!(*builder.builds.lock().unwrap(), 1);
        assert!(
            coordinator
                .ready_image(&packaged, None)
                .await
                .unwrap()
                .unwrap()
                .ends_with("@sha256:ready")
        );
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
        // reuses a distinct demand rather than aliasing the default build; R7
        // the claimed build, blocking readiness, and periodic non-blocking
        // readiness pass the same exact demand plus stored image into the one
        // availability port; a missing image invalidates Ready but cannot build
        // outside the claim/lease worker. R8 a cancelled service token exits
        // the worker without a detached loop. R9 each exact Ready CAS emits one
        // dependency hint; idle polls emit none and one process cannot install
        // competing hint sinks.
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let store = Arc::new(InMemoryEnvironmentImageBuildStore::new());
        let builder = Arc::new(FakeBuilder {
            builds: Mutex::new(0),
            available: AtomicBool::new(true),
            availability_checks: Mutex::new(Vec::new()),
        });
        let clock = awaken_time::system_clock();
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
                clock.clone(),
            )
            .unwrap(),
        );
        let transition_hints = Arc::new(AtomicUsize::new(0));
        let observed_hints = transition_hints.clone();
        coordinator
            .install_transition_hint(Arc::new(move || {
                observed_hints.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap();
        assert!(
            coordinator
                .install_transition_hint(Arc::new(|| {}))
                .is_err(),
            "R9 one process owns one transition-hint sink"
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
            transition_hints.load(Ordering::SeqCst),
            1,
            "R9 Ready CAS wakes once"
        );
        assert_eq!(
            coordinator.ready_image(&packaged, None).await.unwrap(),
            Some("registry/awaken:base@sha256:resolved@sha256:ready".into()),
            "R5"
        );
        assert_eq!(
            coordinator.ready_image_now(&packaged, None).await.unwrap(),
            Some("registry/awaken:base@sha256:resolved@sha256:ready".into()),
            "R7 periodic readiness"
        );
        assert_eq!(*builder.builds.lock().unwrap(), 1, "R5");
        let policy_demand = EnvironmentImageBuildDemand::from_registration(
            &packaged,
            "registry/policy-base@sha256:exact@sha256:resolved",
        )
        .unwrap();
        store
            .ensure(policy_demand.clone(), clock.now_ms(), None)
            .await
            .unwrap();
        assert!(coordinator.run_once("builder-a").await.unwrap(), "R6");
        assert_eq!(
            transition_hints.load(Ordering::SeqCst),
            2,
            "R9 each distinct Ready CAS wakes once"
        );
        assert_eq!(
            coordinator
                .ready_image(&packaged, Some("registry/policy-base@sha256:exact"))
                .await
                .unwrap(),
            Some("registry/policy-base@sha256:exact@sha256:resolved@sha256:ready".into()),
            "R6"
        );
        assert_eq!(*builder.builds.lock().unwrap(), 2, "R6");
        let default_image = "registry/awaken:base@sha256:resolved@sha256:ready";
        let policy_image = "registry/policy-base@sha256:exact@sha256:resolved@sha256:ready";
        assert_eq!(
            builder.availability_checks.lock().unwrap().as_slice(),
            [
                (demand.clone(), default_image.into()),
                (demand.clone(), default_image.into()),
                (demand.clone(), default_image.into()),
                (policy_demand.clone(), policy_image.into()),
                (policy_demand.clone(), policy_image.into()),
            ],
            "R7 every availability path receives its exact demand and stored image"
        );
        builder.available.store(false, Ordering::SeqCst);
        assert_eq!(
            coordinator
                .ready_image_now(&packaged, Some("registry/policy-base@sha256:exact"))
                .await
                .unwrap(),
            None,
            "R7 a missing or drifted image invalidates readiness"
        );
        assert!(
            matches!(
                store
                    .get(&policy_demand.build_key)
                    .await
                    .unwrap()
                    .unwrap()
                    .state,
                EnvironmentImageBuildState::Pending { .. }
            ),
            "R7 invalidation returns work to the canonical claim queue"
        );
        assert_eq!(
            *builder.builds.lock().unwrap(),
            2,
            "R7 readiness never invokes the builder outside a claim"
        );
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
            .expect("R8 cancelled worker exits");
        assert_eq!(*builder.builds.lock().unwrap(), 2, "R8 performs no work");
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
            available: AtomicBool::new(true),
            availability_checks: Mutex::new(Vec::new()),
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
                awaken_time::system_clock(),
            )
            .unwrap(),
        );
        let registrar: Arc<dyn ExecutableEnvironmentRegistrar> =
            Arc::new(BuildAwareExecutableEnvironmentRegistrar::new(
                Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
                builds.clone(),
            ));
        let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let control = awaken_environment_application::EnvironmentApplication::new(
            definitions,
            registrar,
            std::sync::Arc::new(
                awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default(),
            ),
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
                sandbox_policy: None,
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
            awaken_environment_application::execution::EnvironmentExecutionApplication::new(
                Arc::new(awaken_work_store::InMemoryWorkQueue::new(
                    awaken_time::system_clock(),
                )),
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

    struct LatestBuildOutageStore {
        inner: InMemoryEnvironmentImageBuildStore,
        unavailable: AtomicBool,
        ensures: Mutex<Vec<EnvironmentRevision>>,
    }

    #[async_trait]
    impl EnvironmentImageBuildStore for LatestBuildOutageStore {
        async fn ensure(
            &self,
            demand: EnvironmentImageBuildDemand,
            now: u64,
            binding_key: Option<&str>,
        ) -> Result<EnvironmentImageBuildDemand, EnvironmentImageBuildError> {
            self.ensures.lock().unwrap().push(demand.source_revision);
            if demand.source_revision == EnvironmentRevision(2)
                && self.unavailable.load(Ordering::SeqCst)
            {
                return Err(EnvironmentImageBuildError::Storage(
                    "latest build demand remains unavailable".into(),
                ));
            }
            self.inner.ensure(demand, now, binding_key).await
        }
        async fn pinned_demand(
            &self,
            binding_key: &str,
        ) -> Result<Option<EnvironmentImageBuildDemand>, EnvironmentImageBuildError> {
            self.inner.pinned_demand(binding_key).await
        }
        async fn get(
            &self,
            key: &str,
        ) -> Result<
            Option<awaken_executable_environment_contract::EnvironmentImageBuildRecord>,
            EnvironmentImageBuildError,
        > {
            self.inner.get(key).await
        }
        async fn claim_next(
            &self,
            owner: &str,
            now: u64,
            lease: u64,
        ) -> Result<
            Option<awaken_executable_environment_contract::EnvironmentImageBuildClaim>,
            EnvironmentImageBuildError,
        > {
            self.inner.claim_next(owner, now, lease).await
        }
        async fn complete(
            &self,
            claim: &awaken_executable_environment_contract::EnvironmentImageBuildClaim,
            image: &str,
            now: u64,
        ) -> Result<bool, EnvironmentImageBuildError> {
            self.inner.complete(claim, image, now).await
        }
        async fn fail(
            &self,
            claim: &awaken_executable_environment_contract::EnvironmentImageBuildClaim,
            message: &str,
            now: u64,
            retry: u64,
        ) -> Result<bool, EnvironmentImageBuildError> {
            self.inner.fail(claim, message, now, retry).await
        }
        async fn invalidate_ready(
            &self,
            key: &str,
            now: u64,
        ) -> Result<bool, EnvironmentImageBuildError> {
            self.inner.invalidate_ready(key, now).await
        }
    }

    #[tokio::test]
    async fn latest_build_outage_does_not_starve_frozen_history_or_grant_older_queue_work() {
        use awaken_environment_application::execution::CoordinatorEnvironmentRegistrar;
        use awaken_environment_contract::{
            EnvUpdate, EnvironmentDefinitionStore, EnvironmentRegistrationIntentFilter,
            EnvironmentRegistrationOutbox,
        };
        use awaken_executable_environment_contract::WorkQueries;
        // B1/B2 All/Pending: newest exact build demand repeatedly fails after
        // catalog retention. Old frozen revision must still get catalog+build
        // convergence every pass, never current and never a queue grant. B3 old
        // Pending stays pending until latest converges. B4 duplicate historical
        // restore must not skip ensure_registration; B5 normal retry promotes
        // queue only after latest build ensure succeeds.
        for recover_all in [false, true] {
            let clock = awaken_time::system_clock();
            let definitions = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(clock.clone()));
            let authored = definitions
                .create_once(CreateEnvironmentCommand {
                    command_id: format!("build-history-{recover_all}"),
                    name: "frozen".into(),
                    description: None,
                    metadata: Default::default(),
                    scope: None,
                    config: EnvironmentConfig::Cloud {
                        networking: Default::default(),
                        packages: EnvironmentPackages {
                            npm: vec!["tsx@4.0.0".into()],
                            ..Default::default()
                        },
                    },
                    sandbox_policy: None,
                })
                .await
                .unwrap()
                .into_item();
            definitions
                .update(
                    &authored.id,
                    authored.revision,
                    EnvUpdate {
                        name: Some("latest".into()),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
                .unwrap();
            let store = Arc::new(LatestBuildOutageStore {
                inner: InMemoryEnvironmentImageBuildStore::new(),
                unavailable: AtomicBool::new(true),
                ensures: Mutex::new(Vec::new()),
            });
            let builder = Arc::new(FakeBuilder {
                builds: Mutex::new(0),
                available: AtomicBool::new(true),
                availability_checks: Mutex::new(Vec::new()),
            });
            let builds = Arc::new(
                EnvironmentImageBuildCoordinator::new(
                    store.clone(),
                    builder,
                    "registry/history:base",
                    EnvironmentImageBuildPolicy::default(),
                    clock.clone(),
                )
                .unwrap(),
            );
            let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
            let work = Arc::new(awaken_work_store::InMemoryWorkQueue::new(clock));
            let application = awaken_environment_application::EnvironmentApplication::new(
                definitions.clone(),
                Arc::new(CoordinatorEnvironmentRegistrar::new(
                    Arc::new(BuildAwareExecutableEnvironmentRegistrar::new(
                        Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
                        builds.clone(),
                    )),
                    work.clone(),
                )),
                std::sync::Arc::new(
                    awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default(),
                ),
            );
            for _ in 0..2 {
                let result = if recover_all {
                    application.recover_registration_intents().await
                } else {
                    application.drain_registration_intents().await
                };
                assert!(result.is_err(), "B1/B2 latest outage persists");
                let old = catalog
                    .registration_at_revision(&authored.id, authored.revision)
                    .await
                    .unwrap()
                    .expect("B1/B2 frozen catalog restored");
                assert_eq!(
                    catalog.current(&authored.id).unwrap().definition.revision,
                    EnvironmentRevision(2),
                    "B1/B2 old history never becomes current"
                );
                assert!(
                    work.list(&authored.id).await.unwrap().is_empty(),
                    "B1/B2 no queue effect before latest build convergence"
                );
                let demand = builds.demand(&old, None).await.unwrap().unwrap();
                assert!(
                    store.get(&demand.build_key).await.unwrap().is_some(),
                    "B1/B2 frozen build demand restored"
                );
                assert_eq!(
                    definitions
                        .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                        .await
                        .unwrap()
                        .len(),
                    2,
                    "B3 keep both retry obligations"
                );
            }
            assert_eq!(
                store
                    .ensures
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|revision| **revision == EnvironmentRevision(1))
                    .count(),
                2,
                "B4 duplicate history still ensures build"
            );
            assert!(builds.run_once("frozen-builder").await.unwrap());
            let old = catalog
                .registration_at_revision(&authored.id, authored.revision)
                .await
                .unwrap()
                .unwrap();
            assert!(
                builds.ready_image_now(&old, None).await.unwrap().is_some(),
                "frozen image remains usable during latest outage"
            );
            store.unavailable.store(false, Ordering::SeqCst);
            application.drain_registration_intents().await.unwrap();
            assert!(
                definitions
                    .registration_intents(EnvironmentRegistrationIntentFilter::Pending)
                    .await
                    .unwrap()
                    .is_empty(),
                "B5"
            );
            assert!(
                work.list(&authored.id).await.unwrap().is_empty(),
                "Cloud never queues healthcheck"
            );
            assert_eq!(
                catalog.current(&authored.id).unwrap().definition.revision,
                EnvironmentRevision(2),
                "B5"
            );
        }
    }
}
