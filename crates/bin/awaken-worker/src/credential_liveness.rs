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

use awaken_acp_contract::AcpCapabilityObservationSource;
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialObservationState, WorkerLocalCredentialResolver,
};
use awaken_worker_contract::{
    WorkerAcpCapabilityObservation, WorkerCredentialObservation, WorkerCredentialRevision,
    WorkerCredentialState,
};

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
        let credentials = credential_observations(resolver, now_ms, ttl)
            .await
            .map_err(|error| error.to_string())?;
        let acp_capabilities = capability_observations(capability_source, now_ms, ttl).await?;
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
        interval.tick().await;
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

pub(crate) async fn capability_observations(
    source: Option<&dyn AcpCapabilityObservationSource>,
    now_ms: u64,
    ttl: Duration,
) -> Result<Vec<WorkerAcpCapabilityObservation>, String> {
    match source {
        Some(source) => {
            let valid_until_ms = now_ms.saturating_add(ttl.as_millis() as u64);
            source.capability_observations().await.map(|observations| {
                observations
                    .into_iter()
                    .map(|observation| WorkerAcpCapabilityObservation {
                        observation,
                        valid_until_ms,
                    })
                    .collect()
            })
        }
        None => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_acp_contract::{
        AcpCapabilityObservation, AcpCapabilityObservationSource, AcpCapabilityObservationState,
        NegotiatedAcpCapabilities,
    };
    use awaken_runtime_contract::{
        CredentialObservation, CredentialObservationSource, CredentialRef,
        WorkerLocalReferenceRevalidator,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            Ok(vec![AcpCapabilityObservation {
                backend_ref: "acp:codex".into(),
                adapter_version: "1.2.3".into(),
                state: AcpCapabilityObservationState::Verified,
                observed_at_ms: 1,
                fingerprint: Some("fingerprint".into()),
                negotiated: Some(NegotiatedAcpCapabilities {
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
                }),
                reason_code: None,
            }])
        }
    }

    struct FailedCapabilitySource;

    #[async_trait::async_trait]
    impl AcpCapabilityObservationSource for FailedCapabilitySource {
        async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
            Err("handshake transport failed".into())
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
        assert_eq!(
            credential_observations(Some(&AvailableResolver), 100, Duration::from_millis(30))
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
                .valid_until_ms,
            130
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
        assert_eq!(capabilities_before[0].valid_until_ms, 130, "R1");

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
