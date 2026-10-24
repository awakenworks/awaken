// @formal entrypoint worker_credential.heartbeat_observations_are_one_atomic_batch
// @formal entrypoint worker_credential.probe_failure_does_not_drain_worker
// @formal evidence worker_credential.publication_budget_covers_probe_and_delivery
//! Worker-local observation lifecycle.
//!
//! Provider adapters own opaque login inspection. This module owns only the
//! trusted observation window and the one atomic cache published by the Worker
//! heartbeat. Credential evidence is refreshed first so an ACP capability source
//! can reuse that exact host observation instead of probing login twice.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationSource};
use awaken_credential_contract::{
    CredentialMaterialError, CredentialObservation, WorkerLocalCredentialResolver,
};
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_contract::{
    WorkerAcpCapabilityObservation, WorkerCredentialObservation, WorkerHeartbeat,
};

/// One validation owner for authored probe settings and negotiated publication
/// timing. This is a normal-operation budget, not a lease extension or a promise
/// that failed/slow probes and transport retries preserve availability.

#[derive(Default)]
pub(crate) struct WorkerObservationCache {
    batch: RwLock<ObservationBatch>,
    refresh_guard: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ObservationBatch {
    generation: u64,
    pub(crate) credentials: BTreeSet<WorkerCredentialObservation>,
    pub(crate) acp_capabilities: Vec<WorkerAcpCapabilityObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservationBatchCoordinate {
    generation: u64,
    observed_at_ms: u64,
    valid_until_ms: u64,
}

#[must_use]
const fn next_observation_batch_coordinate(
    current_generation: u64,
    observed_at_ms: u64,
    ttl_ms: u64,
) -> Option<ObservationBatchCoordinate> {
    let Some(generation) = current_generation.checked_add(1) else {
        return None;
    };
    Some(ObservationBatchCoordinate {
        generation,
        observed_at_ms,
        valid_until_ms: observed_at_ms.saturating_add(ttl_ms),
    })
}

impl ObservationBatch {
    fn leased(
        coordinate: ObservationBatchCoordinate,
        credentials: BTreeSet<CredentialObservation>,
        acp_capabilities: Vec<AcpCapabilityObservation>,
    ) -> Self {
        Self {
            generation: coordinate.generation,
            credentials: lease_credential_observations(credentials, coordinate),
            acp_capabilities: lease_capability_observations(acp_capabilities, coordinate),
        }
    }

    /// Consume one frozen observation batch into the sole heartbeat wire
    /// projection. Initial and periodic heartbeats share this path so neither
    /// can read credential and capability evidence from different generations.
    pub(crate) fn into_heartbeat(
        self,
        sequence: u64,
        ready: bool,
        in_flight: u32,
        warm_environment_shapes: BTreeSet<awaken_provisioning_contract::SandboxCapacityShapeId>,
    ) -> WorkerHeartbeat {
        WorkerHeartbeat {
            sequence,
            ready,
            in_flight,
            warm_environment_shapes,
            credential_observations: self.credentials,
            acp_capability_observations: self.acp_capabilities,
        }
    }
}

impl WorkerObservationCache {
    pub(crate) fn snapshot(&self) -> ObservationBatch {
        self.batch
            .read()
            .expect("observation batch cache poisoned")
            .clone()
    }

    fn generation(&self) -> u64 {
        self.batch
            .read()
            .expect("observation batch cache poisoned")
            .generation
    }

    fn publish(
        &self,
        credentials: BTreeSet<CredentialObservation>,
        acp_capabilities: Vec<AcpCapabilityObservation>,
        observed_at_ms: u64,
        ttl: Duration,
    ) -> Result<(), String> {
        let mut batch = self
            .batch
            .write()
            .expect("observation batch cache poisoned");
        let coordinate = next_observation_batch_coordinate(
            batch.generation,
            observed_at_ms,
            ttl.as_millis() as u64,
        )
        .ok_or("observation generation exhausted")?;
        *batch = ObservationBatch::leased(coordinate, credentials, acp_capabilities);
        Ok(())
    }

    pub(crate) async fn refresh(
        &self,
        resolver: Option<&dyn WorkerLocalCredentialResolver>,
        capability_source: Option<&dyn AcpCapabilityObservationSource>,
        clock: &dyn WallClock,
        ttl: Duration,
    ) -> Result<(), String> {
        let started_at_ms = clock.now_ms();
        // Every trigger uses this one operation. A caller that waited behind a
        // successful concurrent refresh consumes that causal batch instead of
        // probing the same host identity a second time.
        let observed_generation = self.generation();
        let _refresh = self.refresh_guard.lock().await;
        if self.generation() != observed_generation {
            return Ok(());
        }
        let credentials = credential_observations(resolver)
            .await
            .map_err(|error| error.to_string())?;
        let acp_capabilities = capability_observations(capability_source).await?;
        // A slow adapter handshake is part of the observation operation. Start
        // the full lease only after the complete causal batch has finished so
        // probe latency cannot consume (or entirely exhaust) fresh evidence.
        let observed_at_ms = clock.now_ms().max(started_at_ms);
        // Both probes form one causal batch. A hard failure in either source
        // cannot renew only half of the evidence or create mixed-generation
        // credential/capability truth.
        self.publish(credentials, acp_capabilities, observed_at_ms, ttl)
    }
}

#[cfg(kani)]
/// @obligation worker_credential.heartbeat_observations_are_one_atomic_batch
#[kani::proof]
fn observation_batch_is_single_generation_and_never_wraps() {
    // Causes: C1 a current cache generation; C2 one completed credential and
    // capability probe batch; C3 one trusted lease window. Effects: E1 a
    // successor exists iff C1 is not exhausted; E2 both projected evidence
    // families receive the same exclusive deadline; E3 the new generation is
    // exactly one greater. The RwLock and probe I/O remain explicit TCB.
    let current = kani::any::<u64>();
    let observed_at_ms = kani::any::<u64>();
    let ttl_ms = kani::any::<u64>();
    let next = next_observation_batch_coordinate(current, observed_at_ms, ttl_ms);
    if current == u64::MAX {
        assert!(next.is_none(), "E1");
    } else {
        let batch = next.expect("every non-exhausted generation has one successor");
        let deadline = observed_at_ms.saturating_add(ttl_ms);
        assert_eq!(batch.generation, current + 1, "E3");
        assert_eq!(batch.observed_at_ms, observed_at_ms, "E2");
        assert_eq!(batch.valid_until_ms, deadline, "E2");
    }
}

pub(crate) fn spawn_probe(
    cache: Arc<WorkerObservationCache>,
    resolver: Option<Arc<dyn WorkerLocalCredentialResolver>>,
    capability_source: Option<Arc<dyn AcpCapabilityObservationSource>>,
    probe_interval: Duration,
    observation_ttl: Duration,
    clock: Arc<dyn WallClock>,
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
                    clock.as_ref(),
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
    coordinate: ObservationBatchCoordinate,
) -> BTreeSet<WorkerCredentialObservation> {
    observations
        .into_iter()
        .map(|observation| WorkerCredentialObservation {
            credential: observation.credential,
            state: observation.state,
            observed_at_ms: coordinate.observed_at_ms,
            valid_until_ms: coordinate.valid_until_ms,
            reason_code: observation.reason_code,
        })
        .collect()
}

fn lease_capability_observations(
    observations: Vec<AcpCapabilityObservation>,
    coordinate: ObservationBatchCoordinate,
) -> Vec<WorkerAcpCapabilityObservation> {
    observations
        .into_iter()
        .map(|observation| WorkerAcpCapabilityObservation {
            observation,
            valid_until_ms: coordinate.valid_until_ms,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_acp_contract::{AcpCapabilityObservationSource, NegotiatedAcpCapabilities};
    use awaken_agent_contract::credential::CredentialRef;
    use awaken_credential_contract::{
        CredentialObservation, CredentialObservationSource, WorkerLocalReferenceRevalidator,
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
        // Test design — Cause: the adapter returns one exact Available revision
        // and the Worker completes the probe at T=100 with TTL=30. Effect: the
        // published fact is `[100,130)` for that revision. Constraints: the
        // Worker, not the adapter or heartbeat, owns trusted observation/deadline
        // time. Decision rule H1=successful probe=>one exact leased observation.
        let observations = credential_observations(Some(&AvailableResolver))
            .await
            .expect("worker-local probe");
        assert_eq!(
            lease_credential_observations(
                observations,
                next_observation_batch_coordinate(0, 100, 30).expect("one batch"),
            ),
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
        // Test design — Causes: C1 an Available fact is published with deadline
        // 130; C2 the next probe hard-fails at 120. Effects: refresh returns an
        // error and the cached fact/deadline remain byte exact until natural
        // expiry. Constraints: failure and heartbeat cannot renew stale evidence.
        // Decision rule H3=C1+C2=>preserve, never extend or replace partially.
        let cache = WorkerObservationCache::default();
        cache
            .refresh(
                Some(&AvailableResolver),
                None,
                &awaken_lease_testkit::ManualClock::new(100),
                Duration::from_millis(30),
            )
            .await
            .expect("initial probe succeeds");
        let before = cache.snapshot().credentials;

        assert!(
            cache
                .refresh(
                    Some(&FailedResolver),
                    None,
                    &awaken_lease_testkit::ManualClock::new(120),
                    Duration::from_millis(30),
                )
                .await
                .is_err()
        );
        assert_eq!(
            cache.snapshot().credentials,
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
    // Effects: R1 publishes both facts with one deadline; R2 returns error and
    // preserves both prior snapshots. Constraints/invariants: a Worker heartbeat
    // exposes one atomic observation batch and never mixes generations.
    #[tokio::test]
    async fn capability_failure_cannot_partially_renew_credential_evidence() {
        let cache = WorkerObservationCache::default();
        cache
            .refresh(
                Some(&AvailableResolver),
                Some(&VerifiedCapabilitySource),
                &awaken_lease_testkit::ManualClock::new(100),
                Duration::from_millis(30),
            )
            .await
            .expect("R1");
        let credentials_before = cache.snapshot().credentials;
        let capabilities_before = cache.snapshot().acp_capabilities;
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
                    &awaken_lease_testkit::ManualClock::new(120),
                    Duration::from_millis(30),
                )
                .await
                .is_err(),
            "R2"
        );
        assert_eq!(cache.snapshot().credentials, credentials_before, "R2");
        assert_eq!(cache.snapshot().acp_capabilities, capabilities_before, "R2");
    }

    #[tokio::test]
    async fn capability_probe_latency_does_not_consume_the_observation_lease() {
        // Cause/effect table, started_at=100 and TTL=30:
        // | probe outcome | injected reading after probe | effect |
        // | success | 1000 (forward) | credential stamp 1000, both deadlines 1030 |
        // | success | 100 (same) | credential stamp 100, both deadlines 130 |
        // | success | 50 (rollback) | credential stamp 100, both deadlines 130 |
        // All rows publish only after the complete probe and preserve one
        // causal batch. Hard errors retain the prior batch in sibling tests.
        for released_at_ms in [1_000, 100, 50] {
            let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
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
                let clock = clock.clone();
                tokio::spawn(async move {
                    cache
                        .refresh(
                            Some(&AvailableResolver),
                            Some(source.as_ref()),
                            clock.as_ref(),
                            Duration::from_millis(30),
                        )
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(30), entered.notified())
                .await
                .expect("probe entered");
            assert!(cache.snapshot().credentials.is_empty(), "no partial batch");
            clock.set(released_at_ms);
            release.notify_one();
            refresh.await.expect("refresh task").expect("refresh batch");
            let batch = cache.snapshot();
            let credential = batch.credentials.iter().next().expect("one credential");
            let capability = &batch.acp_capabilities[0];
            assert_eq!(credential.observed_at_ms, released_at_ms.max(100));
            assert_eq!(credential.valid_until_ms, credential.observed_at_ms + 30);
            // ACP's protocol evidence retains its source-owned timestamp;
            // only its publication lease is projected from the batch clock.
            assert_eq!(capability.observation.observed_at_ms(), 1);
            assert_eq!(capability.valid_until_ms, credential.valid_until_ms);
        }
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
         * Constraint/invariant: startup and heartbeat progress never treat an
         * in-flight probe as evidence or wait on it synchronously.
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
            Arc::new(awaken_lease_testkit::ManualClock::new(100)),
        );

        tokio::time::timeout(Duration::from_millis(100), entered.notified())
            .await
            .expect("BP1/E1: first probe starts immediately");
        assert!(cache.snapshot().acp_capabilities.is_empty(), "BP1/E2");
        release.notify_one();
        tokio::time::timeout(Duration::from_millis(100), async {
            while cache.snapshot().acp_capabilities.is_empty() {
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
    // Constraint/invariant: overlapping triggers share the single refresh owner;
    // neither can publish a parallel observation generation.
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
                        &awaken_lease_testkit::ManualClock::new(100),
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
                        &awaken_lease_testkit::ManualClock::new(101),
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
        assert_eq!(cache.generation(), 1, "R1/E3");
    }

    #[tokio::test]
    async fn heartbeat_snapshot_never_mixes_concurrent_publication_batches() {
        // B1 a heartbeat freezes one snapshot; B2 another successful publication
        // happens before fields are projected => the old snapshot stays paired.
        // B3 concurrent writers/readers => both observations always carry the
        // same marker, generation never regresses, and every accepted write
        // advances exactly once. The adapter test is not a proof of std RwLock.
        let cache = WorkerObservationCache::default();
        let credentials = AvailableResolver.credential_observations().await.unwrap();
        let capabilities = VerifiedCapabilitySource
            .capability_observations()
            .await
            .unwrap();
        let publish = |marker| {
            cache
                .publish(
                    credentials.clone(),
                    capabilities.clone(),
                    marker,
                    Duration::ZERO,
                )
                .unwrap();
        };
        publish(10);
        let old = cache.snapshot().into_heartbeat(1, true, 0, BTreeSet::new());
        publish(20);
        assert_eq!(
            old.credential_observations.first().unwrap().valid_until_ms,
            10
        );
        assert_eq!(old.acp_capability_observations[0].valid_until_ms, 10);
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for writer in 0..2 {
                let publish = &publish;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for step in 0..128 {
                        publish(100 + writer * 128 + step);
                    }
                });
            }
            for _ in 0..2 {
                let cache = &cache;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    let mut last = 0;
                    for _ in 0..256 {
                        let snapshot = cache.snapshot();
                        assert!(snapshot.generation >= last);
                        last = snapshot.generation;
                        let heartbeat = snapshot.into_heartbeat(last, true, 0, BTreeSet::new());
                        assert_eq!(
                            heartbeat
                                .credential_observations
                                .first()
                                .unwrap()
                                .valid_until_ms,
                            heartbeat.acp_capability_observations[0].valid_until_ms
                        );
                    }
                });
            }
        });
        assert_eq!(cache.generation(), 258);
    }

    #[test]
    fn exhausted_batch_generation_rejects_without_partial_publication() {
        // B4 exact last successor commits once. B5 exhaustion rejects repeated
        // attempts without wrapping, clearing evidence or changing either part.
        let cache = WorkerObservationCache::default();
        cache.batch.write().unwrap().generation = u64::MAX - 1;
        cache
            .publish(BTreeSet::new(), Vec::new(), 0, Duration::ZERO)
            .unwrap();
        let before = cache.snapshot();
        for _ in 0..2 {
            assert!(
                cache
                    .publish(BTreeSet::new(), Vec::new(), 0, Duration::ZERO)
                    .is_err()
            );
            assert_eq!(cache.snapshot(), before);
        }
    }
}
