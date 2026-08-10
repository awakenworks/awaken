//! Control-owned recovery supervisor for static executable registrations.
//!
//! Agent publications and Environment revisions keep their independent
//! aggregates and command paths. This supervisor only owns their shared retry,
//! readiness, and observability lifecycle after authority data has committed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_config_service::PublicationBindingReconciler;
use awaken_environment_application::EnvironmentApplication;

#[derive(Clone, Copy, Debug)]
pub struct RegistrationSupervisorConfig {
    pub retry_min: Duration,
    pub retry_max: Duration,
    pub settled_interval: Duration,
}

impl Default for RegistrationSupervisorConfig {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            settled_interval: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationHealthSnapshot {
    pub ready: bool,
    pub pending_domains: usize,
    pub consecutive_failures: u64,
    pub lag_seconds: u64,
}

pub struct RegistrationHealth {
    ready: AtomicBool,
    pending_domains: AtomicUsize,
    consecutive_failures: AtomicU64,
    last_success_unix_ms: AtomicU64,
    started_unix_ms: u64,
}

impl Default for RegistrationHealth {
    fn default() -> Self {
        Self {
            ready: AtomicBool::new(false),
            pending_domains: AtomicUsize::new(2),
            consecutive_failures: AtomicU64::new(0),
            last_success_unix_ms: AtomicU64::new(0),
            started_unix_ms: unix_now_ms(),
        }
    }
}

impl RegistrationHealth {
    #[must_use]
    pub fn snapshot(&self) -> RegistrationHealthSnapshot {
        let last_success = self.last_success_unix_ms.load(Ordering::Acquire);
        let now = unix_now_ms();
        RegistrationHealthSnapshot {
            ready: self.ready.load(Ordering::Acquire),
            pending_domains: self.pending_domains.load(Ordering::Acquire),
            consecutive_failures: self.consecutive_failures.load(Ordering::Acquire),
            lag_seconds: if last_success == 0 {
                now.saturating_sub(self.started_unix_ms) / 1_000
            } else {
                now.saturating_sub(last_success) / 1_000
            },
        }
    }

    fn record(&self, pending_domains: usize) {
        self.pending_domains
            .store(pending_domains, Ordering::Release);
        if pending_domains == 0 {
            self.last_success_unix_ms
                .store(unix_now_ms().max(1), Ordering::Release);
            self.consecutive_failures.store(0, Ordering::Release);
            self.ready.store(true, Ordering::Release);
        } else {
            self.consecutive_failures.fetch_add(1, Ordering::AcqRel);
            self.ready.store(false, Ordering::Release);
        }
    }
}

pub struct StaticRegistrationSupervisor {
    health: Arc<RegistrationHealth>,
    wake: tokio::sync::watch::Sender<u64>,
}

impl StaticRegistrationSupervisor {
    #[must_use]
    pub fn start(
        agents: Arc<dyn PublicationBindingReconciler>,
        environments: Arc<EnvironmentApplication>,
        process_tasks: &awaken_process_lifecycle::ProcessTaskGroup,
    ) -> Arc<Self> {
        Self::start_with_config(
            agents,
            environments,
            RegistrationSupervisorConfig::default(),
            process_tasks,
        )
    }

    #[must_use]
    pub fn start_with_config(
        agents: Arc<dyn PublicationBindingReconciler>,
        environments: Arc<EnvironmentApplication>,
        config: RegistrationSupervisorConfig,
        process_tasks: &awaken_process_lifecycle::ProcessTaskGroup,
    ) -> Arc<Self> {
        let health = Arc::new(RegistrationHealth::default());
        let (wake, mut wake_rx) = tokio::sync::watch::channel(0_u64);
        let task_health = health.clone();
        process_tasks.spawn("control-static-registration", move |cancel| async move {
            let mut failures = 0_u32;
            loop {
                let (agent_result, environment_result) = tokio::join!(
                    agents.recover_registrations(),
                    environments.reconcile_registrations()
                );
                let pending =
                    usize::from(agent_result.is_err()) + usize::from(environment_result.is_err());
                task_health.record(pending);
                if let Err(error) = agent_result {
                    tracing::warn!(error, "Agent registration recovery remains pending");
                }
                if let Err(error) = environment_result {
                    tracing::warn!(error = %error, "Environment registration recovery remains pending");
                }
                let delay = if pending == 0 {
                    failures = 0;
                    config.settled_interval
                } else {
                    failures = failures.saturating_add(1);
                    retry_delay(config, failures)
                };
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {}
                    changed = wake_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                    }
                }
            }
            Ok(())
        });
        Arc::new(Self { health, wake })
    }

    #[must_use]
    pub fn health(&self) -> Arc<RegistrationHealth> {
        self.health.clone()
    }

    /// Request an immediate idempotent recovery pass. Durable facts remain the
    /// only work source; this signal carries no registration data.
    pub fn wake(&self) {
        self.wake.send_modify(|generation| {
            *generation = generation.saturating_add(1);
        });
    }
}

fn retry_delay(config: RegistrationSupervisorConfig, failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    config
        .retry_min
        .saturating_mul(1_u32 << exponent)
        .min(config.retry_max)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_environment_contract::EnvRegistry;
    use awaken_executable_environment_contract::{
        ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
        ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
        ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
    };

    use super::*;

    struct AgentRecovery {
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl PublicationBindingReconciler for AgentRecovery {
        async fn reconcile(&self) -> Result<usize, String> {
            Ok(0)
        }

        async fn reconcile_all(&self) -> Result<usize, String> {
            Ok(0)
        }

        async fn recover_registrations(&self) -> Result<usize, String> {
            if self.fail.swap(false, Ordering::SeqCst) {
                Err("agent unavailable".into())
            } else {
                Ok(1)
            }
        }
    }

    #[derive(Default)]
    struct EnvironmentRegistrar {
        registrations: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl ExecutableEnvironmentRegistrar for EnvironmentRegistrar {
        async fn register(
            &self,
            _registration: ExecutableEnvironmentRegistration,
        ) -> Result<ExecutableEnvironmentRegistrationOutcome, ExecutableEnvironmentRegistrationError>
        {
            *self.registrations.lock().unwrap() += 1;
            Ok(ExecutableEnvironmentRegistrationOutcome::RegisteredCurrent)
        }

        async fn withdraw(
            &self,
            _withdrawal: ExecutableEnvironmentWithdrawal,
        ) -> Result<ExecutableEnvironmentWithdrawalOutcome, ExecutableEnvironmentRegistrationError>
        {
            Ok(ExecutableEnvironmentWithdrawalOutcome::WithdrawnCurrent)
        }
    }

    #[tokio::test]
    async fn readiness_and_backoff_follow_both_authoritative_recovery_results() {
        // Cause/effect decision table:
        // R1 Agent fails while Environment succeeds -> not ready, one pending
        // domain, one failure; R2 wake retries unchanged durable facts and both
        // succeed -> ready, zero pending/failures; R3 repeated failures use capped
        // exponential delays without creating another work source.
        let agents = Arc::new(AgentRecovery {
            fail: AtomicBool::new(true),
        });
        let envs: Arc<dyn EnvRegistry> = Arc::new(awaken_env_store::InMemoryEnvRegistry::new());
        let registrar = Arc::new(EnvironmentRegistrar::default());
        let environments = Arc::new(EnvironmentApplication::new(envs, registrar, None));
        let process_tasks = awaken_process_lifecycle::ProcessTaskGroup::new();
        let supervisor = StaticRegistrationSupervisor::start_with_config(
            agents,
            environments,
            RegistrationSupervisorConfig {
                retry_min: Duration::from_secs(60),
                retry_max: Duration::from_secs(60),
                settled_interval: Duration::from_secs(60),
            },
            &process_tasks,
        );
        for _ in 0..100 {
            if supervisor.health().snapshot().consecutive_failures == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        let first = supervisor.health().snapshot();
        assert!(!first.ready, "R1");
        assert_eq!(first.pending_domains, 1, "R1");
        assert_eq!(first.consecutive_failures, 1, "R1");

        supervisor.wake();
        for _ in 0..100 {
            if supervisor.health().snapshot().ready {
                break;
            }
            tokio::task::yield_now().await;
        }
        let recovered = supervisor.health().snapshot();
        assert!(recovered.ready, "R2");
        assert_eq!(recovered.pending_domains, 0, "R2");
        assert_eq!(recovered.consecutive_failures, 0, "R2");

        assert_eq!(
            retry_delay(
                RegistrationSupervisorConfig {
                    retry_min: Duration::from_secs(2),
                    retry_max: Duration::from_secs(5),
                    settled_interval: Duration::from_secs(10),
                },
                8,
            ),
            Duration::from_secs(5),
            "R3"
        );
        process_tasks
            .shutdown(Duration::from_secs(1))
            .await
            .expect("test supervisor stops cooperatively");
    }
}
