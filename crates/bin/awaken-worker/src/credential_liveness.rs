//! Worker-local credential observation lifecycle.
//!
//! Provider adapters own opaque login inspection. This module owns only the
//! trusted observation window and the cache published by the Worker heartbeat.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use awaken_runtime_contract::{
    CredentialMaterialError, CredentialObservationState, WorkerLocalCredentialResolver,
};
use awaken_worker_contract::{
    WorkerCredentialObservation, WorkerCredentialRevision, WorkerCredentialState,
};

#[derive(Default)]
pub(crate) struct CredentialObservationCache {
    observations: RwLock<BTreeSet<WorkerCredentialObservation>>,
}

impl CredentialObservationCache {
    pub(crate) fn snapshot(&self) -> BTreeSet<WorkerCredentialObservation> {
        self.observations
            .read()
            .expect("credential observation cache poisoned")
            .clone()
    }

    pub(crate) async fn refresh(
        &self,
        resolver: Option<&dyn WorkerLocalCredentialResolver>,
        now_ms: u64,
        ttl: Duration,
    ) -> Result<(), CredentialMaterialError> {
        let observations = observations(resolver, now_ms, ttl).await?;
        *self
            .observations
            .write()
            .expect("credential observation cache poisoned") = observations;
        Ok(())
    }
}

pub(crate) fn spawn_probe(
    cache: Arc<CredentialObservationCache>,
    resolver: Option<Arc<dyn WorkerLocalCredentialResolver>>,
    probe_interval: Duration,
    observation_ttl: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(probe_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = cache
                .refresh(resolver.as_deref(), crate::wall_clock_ms(), observation_ttl)
                .await
            {
                eprintln!(
                    "local_credential_probe_failed: {error}; prior observations retain their original deadlines"
                );
            }
        }
    })
}

pub(crate) async fn observations(
    resolver: Option<&dyn WorkerLocalCredentialResolver>,
    now_ms: u64,
    ttl: Duration,
) -> Result<BTreeSet<WorkerCredentialObservation>, CredentialMaterialError> {
    match resolver {
        Some(resolver) => resolver
            .credential_observations()
            .await
            .map(|observations| {
                let valid_until_ms = now_ms.saturating_add(ttl.as_millis() as u64);
                observations
                    .into_iter()
                    .map(|observation| WorkerCredentialObservation {
                        credential: WorkerCredentialRevision {
                            id: observation.credential.id,
                            revision: observation.credential.revision,
                        },
                        state: match observation.state {
                            CredentialObservationState::Available => {
                                WorkerCredentialState::Available
                            }
                            CredentialObservationState::LoginRequired => {
                                WorkerCredentialState::LoginRequired
                            }
                            CredentialObservationState::Expired => WorkerCredentialState::Expired,
                            CredentialObservationState::Invalid => WorkerCredentialState::Invalid,
                            CredentialObservationState::Disabled => WorkerCredentialState::Disabled,
                            CredentialObservationState::ProbeFailed => {
                                WorkerCredentialState::ProbeFailed
                            }
                        },
                        observed_at_ms: now_ms,
                        valid_until_ms,
                        reason_code: observation.reason_code,
                    })
                    .collect()
            }),
        None => Ok(BTreeSet::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::{
        CredentialObservation, CredentialObservationSource, CredentialRef,
        WorkerLocalReferenceRevalidator,
    };

    struct AvailableResolver;

    #[async_trait::async_trait]
    impl CredentialObservationSource for AvailableResolver {
        async fn credential_observations(
            &self,
        ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
            Ok(BTreeSet::from([CredentialObservation::available(
                CredentialRef {
                    id: "cred:worker".into(),
                    revision: 4,
                },
                1,
            )]))
        }
    }

    #[async_trait::async_trait]
    impl WorkerLocalReferenceRevalidator for AvailableResolver {
        async fn revalidate_worker_reference(
            &self,
            credential: &CredentialRef,
        ) -> Result<CredentialObservation, CredentialMaterialError> {
            Ok(CredentialObservation::available(credential.clone(), 1))
        }
    }

    struct FailedResolver;

    #[async_trait::async_trait]
    impl CredentialObservationSource for FailedResolver {
        async fn credential_observations(
            &self,
        ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
            Err(CredentialMaterialError::ProbeFailed)
        }
    }

    #[async_trait::async_trait]
    impl WorkerLocalReferenceRevalidator for FailedResolver {
        async fn revalidate_worker_reference(
            &self,
            _credential: &CredentialRef,
        ) -> Result<CredentialObservation, CredentialMaterialError> {
            Err(CredentialMaterialError::ProbeFailed)
        }
    }

    #[tokio::test]
    async fn worker_stamps_the_trusted_observation_window() {
        assert_eq!(
            observations(Some(&AvailableResolver), 100, Duration::from_millis(30))
                .await
                .expect("worker-local probe"),
            BTreeSet::from([WorkerCredentialObservation::available(
                WorkerCredentialRevision {
                    id: "cred:worker".into(),
                    revision: 4,
                },
                100,
                130,
            )])
        );
    }

    #[tokio::test]
    async fn probe_failure_preserves_the_original_deadline() {
        let cache = CredentialObservationCache::default();
        cache
            .refresh(Some(&AvailableResolver), 100, Duration::from_millis(30))
            .await
            .expect("initial probe succeeds");
        let before = cache.snapshot();

        assert_eq!(
            cache
                .refresh(Some(&FailedResolver), 120, Duration::from_millis(30))
                .await,
            Err(CredentialMaterialError::ProbeFailed)
        );
        assert_eq!(
            cache.snapshot(),
            before,
            "failed probe cannot renew evidence"
        );
        assert_eq!(
            before
                .iter()
                .next()
                .expect("one observation")
                .valid_until_ms,
            130
        );
    }
}
