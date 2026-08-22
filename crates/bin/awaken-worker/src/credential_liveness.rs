//! Worker-local observation lifecycle.
//!
//! Provider adapters own opaque login inspection. This module owns only the
//! trusted observation window and the one atomic cache published by the Worker
//! heartbeat. Credential evidence is refreshed first so an ACP capability source
//! can reuse that exact host observation instead of probing login twice.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationSource};
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialObservation, WorkerLocalCredentialResolver,
};
use awaken_worker_contract::{WorkerAcpCapabilityObservation, WorkerCredentialObservation};

#[derive(Default)]
pub(crate) struct WorkerObservationCache {
    credentials: RwLock<BTreeSet<WorkerCredentialObservation>>,
    acp_capabilities: RwLock<Vec<WorkerAcpCapabilityObservation>>,
    refresh_generation: AtomicU64,
    refresh_guard: tokio::sync::Mutex<()>,
}

impl WorkerObservationCache {
    pub(crate) fn credential_snapshot(&self) -> BTreeSet<WorkerCredentialObservation> {
        self.credentials
            .read()
            .expect("credential observation cache poisoned")
            .clone()
    }

    pub(crate) fn acp_capability_snapshot(&self) -> Vec<WorkerAcpCapabilityObservation> {
        self.acp_capabilities
            .read()
            .expect("ACP capability observation cache poisoned")
            .clone()
    }

    pub(crate) async fn refresh(
        &self,
        resolver: Option<&dyn WorkerLocalCredentialResolver>,
        capability_source: Option<&dyn AcpCapabilityObservationSource>,
        now_ms: u64,
        ttl: Duration,
    ) -> Result<(), String> {
        // Every trigger uses this one operation. A caller that waited behind a
        // successful concurrent refresh consumes that causal batch instead of
        // probing the same host identity a second time.
        let observed_generation = self.refresh_generation.load(Ordering::Acquire);
        let _refresh = self.refresh_guard.lock().await;
        if self.refresh_generation.load(Ordering::Acquire) != observed_generation {
            return Ok(());
        }
        let credentials = credential_observations(resolver)
            .await
            .map_err(|error| error.to_string())?;
        let acp_capabilities = capability_observations(capability_source).await?;
        // A slow adapter handshake is part of the observation operation. Start
        // the full lease only after the complete causal batch has finished so
        // probe latency cannot consume (or entirely exhaust) fresh evidence.
        let observed_at_ms = crate::wall_clock_ms().max(now_ms);
        let credentials = lease_credential_observations(credentials, observed_at_ms, ttl);
        let acp_capabilities = lease_capability_observations(acp_capabilities, observed_at_ms, ttl);
        // Both probes form one causal batch. A hard failure in either source
        // cannot renew only half of the evidence or create mixed-generation
        // credential/capability truth.
        *self
            .credentials
            .write()
            .expect("credential observation cache poisoned") = credentials;
        *self
            .acp_capabilities
            .write()
            .expect("ACP capability observation cache poisoned") = acp_capabilities;
        self.refresh_generation.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

pub(crate) fn spawn_probe(
    cache: Arc<WorkerObservationCache>,
    resolver: Option<Arc<dyn WorkerLocalCredentialResolver>>,
    capability_source: Option<Arc<dyn AcpCapabilityObservationSource>>,
    probe_interval: Duration,
    observation_ttl: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(probe_interval);
        // A slow bounded probe batch must not cause Tokio's default burst mode
        // to replay every missed tick and immediately launch another batch.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(error) = cache
                .refresh(
                    resolver.as_deref(),
                    capability_source.as_deref(),
                    crate::wall_clock_ms(),
                    observation_ttl,
                )
                .await
            {
                eprintln!(
                    "worker_observation_probe_failed: {error}; prior observations retain their original deadlines"
                );
            }
        }
    })
}

pub(crate) async fn credential_observations(
    resolver: Option<&dyn WorkerLocalCredentialResolver>,
) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
    match resolver {
        Some(resolver) => resolver.credential_observations().await,
        None => Ok(BTreeSet::new()),
    }
}

pub(crate) async fn capability_observations(
    source: Option<&dyn AcpCapabilityObservationSource>,
) -> Result<Vec<AcpCapabilityObservation>, String> {
    match source {
        Some(source) => source.capability_observations().await,
        None => Ok(Vec::new()),
    }
}

fn lease_credential_observations(
    observations: BTreeSet<CredentialObservation>,
    observed_at_ms: u64,
    ttl: Duration,
) -> BTreeSet<WorkerCredentialObservation> {
    let valid_until_ms = observed_at_ms.saturating_add(ttl.as_millis() as u64);
    observations
        .into_iter()
        .map(|observation| WorkerCredentialObservation {
            credential: observation.credential,
            state: observation.state,
            observed_at_ms,
            valid_until_ms,
            reason_code: observation.reason_code,
        })
        .collect()
}

fn lease_capability_observations(
    observations: Vec<AcpCapabilityObservation>,
    observed_at_ms: u64,
    ttl: Duration,
) -> Vec<WorkerAcpCapabilityObservation> {
    let valid_until_ms = observed_at_ms.saturating_add(ttl.as_millis() as u64);
    observations
        .into_iter()
        .map(|observation| WorkerAcpCapabilityObservation {
            observation,
            valid_until_ms,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_acp_contract::{AcpCapabilityObservationSource, NegotiatedAcpCapabilities};
    use awaken_runtime_contract::{
        CredentialObservation, CredentialObservationSource, CredentialRef,
        WorkerLocalReferenceRevalidator,
    };
    use awaken_worker_contract::WorkerCredentialRevision;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

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

    struct VerifiedCapabilitySource;

    #[async_trait::async_trait]
    impl AcpCapabilityObservationSource for VerifiedCapabilitySource {
        async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
            Ok(vec![
                AcpCapabilityObservation::verified(
                    "acp:codex",
                    "1.2.3",
                    1,
                    "fingerprint",
                    NegotiatedAcpCapabilities {
                        protocol_version: "1".into(),
                        load_session: true,
                        prompt_image: false,
                        prompt_audio: false,
                        prompt_embedded_context: false,
                        mcp_http: false,
                        mcp_sse: false,
                        session_list: false,
                        modes: Vec::new(),
                        config_options: Vec::new(),
                    },
                )
                .map_err(|error| error.to_string())?,
            ])
        }
    }

    struct FailedCapabilitySource;

    #[async_trait::async_trait]
    impl AcpCapabilityObservationSource for FailedCapabilitySource {
        async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
            Err("handshake transport failed".into())
        }
    }

    struct BlockingCapabilitySource {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl AcpCapabilityObservationSource for BlockingCapabilitySource {
        async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
            self.entered.notify_one();
            self.release.notified().await;
            VerifiedCapabilitySource.capability_observations().await
        }
    }

    struct CountingResolver {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CredentialObservationSource for CountingResolver {
        async fn credential_observations(
            &self,
        ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            AvailableResolver.credential_observations().await
        }
    }

    #[async_trait::async_trait]
    impl WorkerLocalReferenceRevalidator for CountingResolver {
        async fn revalidate_worker_reference(
            &self,
            credential: &CredentialRef,
        ) -> Result<CredentialObservation, CredentialMaterialError> {
            AvailableResolver
                .revalidate_worker_reference(credential)
                .await
        }
    }

    #[tokio::test]
    async fn worker_stamps_the_trusted_observation_window() {
        let observations = credential_observations(Some(&AvailableResolver))
            .await
            .expect("worker-local probe");
        assert_eq!(
            lease_credential_observations(observations, 100, Duration::from_millis(30)),
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
        let cache = WorkerObservationCache::default();
        cache
            .refresh(
                Some(&AvailableResolver),
                None,
                100,
                Duration::from_millis(30),
            )
            .await
            .expect("initial probe succeeds");
        let before = cache.credential_snapshot();

        assert!(
            cache
                .refresh(Some(&FailedResolver), None, 120, Duration::from_millis(30),)
                .await
                .is_err()
        );
        assert_eq!(
            cache.credential_snapshot(),
            before,
            "failed probe cannot renew evidence"
        );
        assert_eq!(
            before
                .iter()
                .next()
                .expect("one observation")
                .valid_until_ms
                - before
                    .iter()
                    .next()
                    .expect("one observation")
                    .observed_at_ms,
            30
        );
    }

    // Cause/effect decision table for one Worker observation batch:
    // R1 credential=success, capability=success -> replace both with one deadline.
    // R2 credential=success, capability=hard-error -> replace neither; the prior
    // credential and capability deadlines remain unchanged.
    // Login unavailable and protocol ProbeFailed are successful observations,
    // not hard errors, and therefore belong to R1.
    #[tokio::test]
    async fn capability_failure_cannot_partially_renew_credential_evidence() {
        let cache = WorkerObservationCache::default();
        cache
            .refresh(
                Some(&AvailableResolver),
                Some(&VerifiedCapabilitySource),
                100,
                Duration::from_millis(30),
            )
            .await
            .expect("R1");
        let credentials_before = cache.credential_snapshot();
        let capabilities_before = cache.acp_capability_snapshot();
        assert_eq!(
            capabilities_before[0].valid_until_ms,
            credentials_before
                .iter()
                .next()
                .expect("one credential")
                .valid_until_ms,
            "R1 uses one causal-batch deadline"
        );

        assert!(
            cache
                .refresh(
                    Some(&AvailableResolver),
                    Some(&FailedCapabilitySource),
                    120,
                    Duration::from_millis(30),
                )
                .await
                .is_err(),
            "R2"
        );
        assert_eq!(cache.credential_snapshot(), credentials_before, "R2");
        assert_eq!(cache.acp_capability_snapshot(), capabilities_before, "R2");
    }

    #[tokio::test]
    async fn capability_probe_latency_does_not_consume_the_observation_lease() {
        let cache = Arc::new(WorkerObservationCache::default());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let source = Arc::new(BlockingCapabilitySource {
            entered: entered.clone(),
            release: release.clone(),
        });
        let refresh = {
            let cache = cache.clone();
            let source = source.clone();
            tokio::spawn(async move {
                cache
                    .refresh(
                        Some(&AvailableResolver),
                        Some(source.as_ref()),
                        crate::wall_clock_ms(),
                        Duration::from_millis(30),
                    )
                    .await
            })
        };

        entered.notified().await;
        let released_at_ms = crate::wall_clock_ms();
        release.notify_one();
        refresh.await.expect("refresh task").expect("refresh batch");

        let credential = cache
            .credential_snapshot()
            .into_iter()
            .next()
            .expect("one credential");
        let capability = cache
            .acp_capability_snapshot()
            .into_iter()
            .next()
            .expect("one capability");
        assert!(credential.observed_at_ms >= released_at_ms);
        assert_eq!(credential.valid_until_ms, credential.observed_at_ms + 30);
        assert_eq!(capability.valid_until_ms, credential.valid_until_ms);
    }

    #[tokio::test]
    async fn background_probe_starts_immediately_and_publishes_only_after_success() {
        /* Startup isolation cause/effect table.
         * Causes: C1 the background probe is spawned; C2 its Sandbox-backed
         * capability source is still blocked; C3 it later succeeds. Effects:
         * E1 the spawn call returns without awaiting the source; E2 no unproven
         * evidence is visible; E3 the completed batch becomes visible without
         * waiting one full periodic interval. Rules: BP1 C1+C2=>E1+E2;
         * BP2 C1+C3=>E3. Worker readiness relies on BP1 while placement remains
         * fail-closed through E2.
         */
        let cache = Arc::new(WorkerObservationCache::default());
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let task = spawn_probe(
            cache.clone(),
            None,
            Some(Arc::new(BlockingCapabilitySource {
                entered: entered.clone(),
                release: release.clone(),
            })),
            Duration::from_secs(3600),
            Duration::from_secs(60),
        );

        tokio::time::timeout(Duration::from_millis(100), entered.notified())
            .await
            .expect("BP1/E1: first probe starts immediately");
        assert!(cache.acp_capability_snapshot().is_empty(), "BP1/E2");
        release.notify_one();
        tokio::time::timeout(Duration::from_millis(100), async {
            while cache.acp_capability_snapshot().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("BP2/E3");
        task.abort();
    }

    // Cause/effect graph for concurrent refresh triggers:
    // C1 two triggers overlap before the first batch commits;
    // E1 one host probe runs, E2 both callers observe success, E3 the committed
    // batch has one generation. Sequential triggers are unconstrained and may
    // refresh again. Rule R1 = C1 -> E1+E2+E3.
    #[tokio::test]
    async fn overlapping_refresh_triggers_coalesce_into_one_probe_batch() {
        let cache = Arc::new(WorkerObservationCache::default());
        let resolver = Arc::new(CountingResolver {
            calls: AtomicUsize::new(0),
        });
        let first = {
            let cache = cache.clone();
            let resolver = resolver.clone();
            tokio::spawn(async move {
                cache
                    .refresh(
                        Some(resolver.as_ref()),
                        None,
                        100,
                        Duration::from_millis(30),
                    )
                    .await
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let cache = cache.clone();
            let resolver = resolver.clone();
            tokio::spawn(async move {
                cache
                    .refresh(
                        Some(resolver.as_ref()),
                        None,
                        101,
                        Duration::from_millis(30),
                    )
                    .await
            })
        };

        first.await.expect("first task").expect("first refresh");
        second
            .await
            .expect("second task")
            .expect("coalesced refresh");
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "R1/E1");
        assert_eq!(cache.refresh_generation.load(Ordering::Acquire), 1, "R1/E3");
    }
}
