//! Coordinator-owned convergence boundary for executable Environment registration.

use std::sync::Arc;

use awaken_executable_environment_contract::{
    ExecutableEnvironmentRegistrar, ExecutableEnvironmentRegistration,
    ExecutableEnvironmentRegistrationError, ExecutableEnvironmentRegistrationOutcome,
    ExecutableEnvironmentWithdrawal, ExecutableEnvironmentWithdrawalOutcome,
};
use awaken_session_contract::work_queue::WorkQueue;

/// Registration and WorkQueue healthcheck convergence share one idempotent
/// boundary. Control sees only [`ExecutableEnvironmentRegistrar`] and cannot
/// access the queue or acknowledge publication before its work intent converges.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    use awaken_environment_contract::{EnvItem, EnvironmentConfig, EnvironmentRevision};
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use awaken_session_contract::work_queue::{
        HeartbeatResult, LeaseHeartbeat, QueueStats, WorkItem, WorkQueueError,
    };
    use awaken_work_store::InMemoryWorkQueue;

    struct FailingConvergenceWorkQueue {
        inner: InMemoryWorkQueue,
        fail_ensure: AtomicBool,
        fail_remove: AtomicBool,
    }

    impl FailingConvergenceWorkQueue {
        fn new() -> Self {
            Self {
                inner: InMemoryWorkQueue::new(),
                fail_ensure: AtomicBool::new(true),
                fail_remove: AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkQueue for FailingConvergenceWorkQueue {
        async fn enqueue_session(&self, env_id: &str, session_id: &str) -> String {
            self.inner.enqueue_session(env_id, session_id).await
        }

        async fn enqueue_healthcheck(&self, env_id: &str) -> String {
            self.inner.enqueue_healthcheck(env_id).await
        }

        async fn ensure_healthcheck(&self, env_id: &str) -> Result<String, WorkQueueError> {
            if self.fail_ensure.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage(
                    "injected healthcheck outage".into(),
                ))
            } else {
                self.inner.ensure_healthcheck(env_id).await
            }
        }

        async fn list(&self, env_id: &str) -> Vec<WorkItem> {
            self.inner.list(env_id).await
        }

        async fn get(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
            self.inner.get(env_id, wid).await
        }

        async fn claim(&self, env_id: &str, worker_id: &str, now_ms: u64) -> Option<WorkItem> {
            self.inner.claim(env_id, worker_id, now_ms).await
        }

        async fn ack(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
            self.inner.ack(env_id, wid).await
        }

        async fn heartbeat(
            &self,
            env_id: &str,
            wid: &str,
            worker_id: &str,
            now_ms: u64,
            heartbeat: LeaseHeartbeat,
        ) -> HeartbeatResult {
            self.inner
                .heartbeat(env_id, wid, worker_id, now_ms, heartbeat)
                .await
        }

        async fn stop(&self, env_id: &str, wid: &str) -> Option<WorkItem> {
            self.inner.stop(env_id, wid).await
        }

        async fn update_metadata(
            &self,
            env_id: &str,
            wid: &str,
            patch: std::collections::BTreeMap<String, String>,
        ) -> Option<WorkItem> {
            self.inner.update_metadata(env_id, wid, patch).await
        }

        async fn stats(&self, env_id: &str, now_ms: u64) -> QueueStats {
            self.inner.stats(env_id, now_ms).await
        }

        async fn remove_env(&self, env_id: &str) -> Result<(), WorkQueueError> {
            if self.fail_remove.load(Ordering::SeqCst) {
                Err(WorkQueueError::Storage("injected removal outage".into()))
            } else {
                self.inner.remove_env(env_id).await
            }
        }
    }

    #[tokio::test]
    async fn acknowledgement_waits_for_work_intent_convergence() {
        // Cause/effect graph: C1 catalog accepts an exact SelfHosted revision;
        // C2 healthcheck storage fails; C3 the same registration is replayed after
        // recovery; C4 catalog accepts a withdrawal; C5 queue removal fails; C6
        // the same withdrawal is replayed after recovery. Effects: E1 C2/C5 are
        // typed unavailable acknowledgements, E2 catalog commands remain
        // idempotent, E3 C3 creates one healthcheck, E4 C6 removes all work.
        //
        // | Rule | catalog command | queue | returned acknowledgement | final queue |
        // | Q1 | register current | fail | Unavailable | empty |
        // | Q2 | register replay | healthy | AlreadyRegistered | one healthcheck |
        // | Q3 | withdraw current | fail | Unavailable | unchanged |
        // | Q4 | withdraw replay | healthy | AlreadyWithdrawn | empty |
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let work = Arc::new(FailingConvergenceWorkQueue::new());
        let registrar = CoordinatorEnvironmentRegistrar::new(
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog.clone())),
            work.clone(),
        );
        let registration = ExecutableEnvironmentRegistration::new(
            EnvItem {
                id: "env-convergence".into(),
                revision: EnvironmentRevision(1),
                name: "convergence".into(),
                description: String::new(),
                metadata: Default::default(),
                scope: None,
                config: EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
                archived_at: None,
            },
            None,
        );
        assert!(
            matches!(
                registrar.register(registration.clone()).await,
                Err(ExecutableEnvironmentRegistrationError::Unavailable(_))
            ),
            "Q1"
        );
        assert!(catalog.current("env-convergence").is_some(), "Q1/E2");
        assert!(work.list("env-convergence").await.is_empty(), "Q1");

        work.fail_ensure.store(false, Ordering::SeqCst);
        assert_eq!(
            registrar.register(registration).await.unwrap(),
            ExecutableEnvironmentRegistrationOutcome::AlreadyRegistered,
            "Q2"
        );
        assert_eq!(work.list("env-convergence").await.len(), 1, "Q2/E3");
        work.enqueue_session("env-convergence", "session-a").await;

        let withdrawal = ExecutableEnvironmentWithdrawal {
            environment_id: "env-convergence".into(),
            lifecycle_revision: EnvironmentRevision(2),
        };
        work.fail_remove.store(true, Ordering::SeqCst);
        assert!(
            matches!(
                registrar.withdraw(withdrawal.clone()).await,
                Err(ExecutableEnvironmentRegistrationError::Unavailable(_))
            ),
            "Q3"
        );
        assert!(catalog.current("env-convergence").is_none(), "Q3/E2");
        assert_eq!(work.list("env-convergence").await.len(), 2, "Q3");

        work.fail_remove.store(false, Ordering::SeqCst);
        assert_eq!(
            registrar.withdraw(withdrawal).await.unwrap(),
            ExecutableEnvironmentWithdrawalOutcome::AlreadyWithdrawn,
            "Q4"
        );
        assert!(work.list("env-convergence").await.is_empty(), "Q4/E4");
    }
}
