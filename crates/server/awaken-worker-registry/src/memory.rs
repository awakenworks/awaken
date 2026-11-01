// @formal entrypoint worker.drain_and_replacement_races_preserve_registry_authority
// @formal entrypoint worker.incarnation_generation_fences_stale_processes
// @formal evidence worker_credential.stale_heartbeat_never_rolls_back_revision
#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
use std::collections::BTreeMap;

#[cfg(not(all(test, feature = "loom")))]
use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerHeartbeat, WorkerIdentity,
    WorkerRegistration,
};

#[cfg(not(all(test, feature = "loom")))]
use awaken_worker_contract::{WorkerDirectory, WorkerObservationSource, WorkerProcessExit};

use crate::transition;

#[derive(Default)]
pub struct MemoryWorkerDirectory {
    #[cfg(all(test, feature = "loom"))]
    workers: Mutex<WorkerRecords>,
    #[cfg(not(all(test, feature = "loom")))]
    workers: std::sync::Arc<tokio::sync::Mutex<WorkerRecords>>,
}

type WorkerRecords = BTreeMap<String, BTreeMap<u64, RegisteredWorker>>;

fn register_records(
    workers: &mut WorkerRecords,
    registration: WorkerRegistration,
    now_ms: u64,
    ttl_ms: u64,
) -> Result<RegisteredWorker, RegistryError> {
    let records = workers.entry(registration.worker_id.clone()).or_default();
    let history = transition::RegistrationHistory {
        incarnation_seen: records
            .values()
            .any(|record| record.snapshot.identity.incarnation_id == registration.incarnation_id),
        complete: records.contains_key(&1),
    };
    let (record, changed) = transition::register(
        records.last_key_value().map(|(_, record)| record),
        history,
        registration,
        now_ms,
        ttl_ms,
    )?;
    if changed {
        records.insert(record.snapshot.identity.generation, record.clone());
    }
    Ok(record)
}

fn mutate_records(
    workers: &mut WorkerRecords,
    identity: &WorkerIdentity,
    decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
) -> Result<RegistryMutation, RegistryError> {
    let records = workers.get_mut(&identity.worker_id);
    let (next, outcome) = decide(
        records
            .as_ref()
            .and_then(|rows| rows.last_key_value())
            .map(|(_, record)| record),
    );
    if let (Some(next), Some(records)) = (next, records) {
        records.insert(next.snapshot.identity.generation, next);
    }
    Ok(outcome)
}

impl MemoryWorkerDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(all(test, feature = "loom"))]
    fn workers_lock(&self) -> Result<loom::sync::MutexGuard<'_, WorkerRecords>, RegistryError> {
        self.workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))
    }

    #[cfg(all(test, feature = "loom"))]
    fn register_sync(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let mut workers = self.workers_lock()?;
        register_records(&mut workers, registration, now_ms, ttl_ms)
    }

    #[cfg(all(test, feature = "loom"))]
    fn heartbeat_sync(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::heartbeat(current, identity, heartbeat, now_ms, ttl_ms)
        })
    }

    #[cfg(all(test, feature = "loom"))]
    fn begin_drain_sync(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::begin_drain(current, identity, deadline_ms)
        })
    }

    #[cfg(all(test, feature = "loom"))]
    fn mutate(
        &self,
        identity: &WorkerIdentity,
        decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
    ) -> Result<RegistryMutation, RegistryError> {
        let mut workers = self.workers_lock()?;
        mutate_records(&mut workers, identity, decide)
    }
}

#[cfg(not(all(test, feature = "loom")))]
impl MemoryWorkerDirectory {
    async fn mutate_async(
        &self,
        identity: &WorkerIdentity,
        decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
    ) -> Result<RegistryMutation, RegistryError> {
        let mut workers = self.workers.lock().await;
        mutate_records(&mut workers, identity, decide)
    }
}

#[cfg(not(all(test, feature = "loom")))]
#[async_trait]
impl WorkerObservationSource for MemoryWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self
            .workers
            .lock()
            .await
            .values()
            .filter_map(|rows| rows.last_key_value().map(|(_, record)| record))
            .cloned()
            .collect())
    }
}

#[cfg(not(all(test, feature = "loom")))]
#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for MemoryWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .workers
            .lock()
            .await
            .get(worker_id)
            .and_then(|rows| rows.last_key_value().map(|(_, record)| record))
            .cloned())
    }
}

#[cfg(not(all(test, feature = "loom")))]
#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for MemoryWorkerDirectory {
    async fn incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .workers
            .lock()
            .await
            .get(&identity.worker_id)
            .and_then(|rows| rows.get(&identity.generation))
            .filter(|record| record.snapshot.identity == *identity)
            .cloned())
    }
}

#[cfg(not(all(test, feature = "loom")))]
#[async_trait]
impl WorkerDirectory for MemoryWorkerDirectory {
    async fn lock_incarnation(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
        let held = self.workers.clone().lock_owned().await;
        let current = held
            .get(&identity.worker_id)
            .and_then(|rows| rows.last_key_value().map(|(_, r)| r))
            .cloned();
        Ok(current
            .filter(|r| r.snapshot.identity == *identity)
            .map(|r| awaken_worker_contract::WorkerIncarnationGuard::new(held, r.snapshot)))
    }
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let mut workers = self.workers.lock().await;
        register_records(&mut workers, registration, now_ms, ttl_ms)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate_async(identity, |current| {
            transition::heartbeat(current, identity, heartbeat, now_ms, ttl_ms)
        })
        .await
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate_async(identity, |current| {
            transition::begin_drain(current, identity, deadline_ms)
        })
        .await
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate_async(identity, |current| transition::quiesce(current, identity))
            .await
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate_async(identity, |current| {
            transition::deregister(current, identity)
        })
        .await
    }

    async fn expire_incarnation(
        &self,
        identity: &WorkerIdentity,
        now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate_async(identity, |current| {
            transition::expire(current, identity, now_ms)
        })
        .await
    }

    async fn record_process_exit(
        &self,
        identity: &WorkerIdentity,
        receipt: WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        receipt.validate()?;
        let mut workers = self.workers.lock().await;
        let record = workers
            .get_mut(&identity.worker_id)
            .and_then(|rows| rows.get_mut(&identity.generation));
        let (next, outcome) =
            transition::record_process_exit(record.as_deref(), identity, receipt)?;
        if let (Some(next), Some(record)) = (next, record) {
            *record = next;
        }
        Ok(outcome)
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use awaken_worker_contract::{WorkerManifest, WorkerState};
    use loom::sync::Arc;
    use loom::thread;

    // One existing model-resource policy for full Worker snapshots. This is
    // Loom's coroutine allocation, independent of the production Tokio stack.
    const SNAPSHOT_ACTOR_STACK_BYTES: usize = 1024 * 1024;

    fn snapshot_actor<T: Send + 'static>(
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> thread::JoinHandle<T> {
        thread::Builder::new()
            .stack_size(SNAPSHOT_ACTOR_STACK_BYTES)
            .spawn(operation)
            .unwrap()
    }

    fn registration(incarnation: &str) -> WorkerRegistration {
        WorkerRegistration {
            worker_id: "worker".to_string(),
            incarnation_id: incarnation.to_string(),
            manifest: WorkerManifest::default(),
            create_ledger: None,
        }
    }

    #[test]
    fn historical_register_cannot_win_a_concurrent_fresh_replacement() {
        // L1 A/g1 -> B/g2 expired, then concurrent old A retry and fresh C.
        // Both mutex schedules must reject A and allocate C/g3 exactly once;
        // all three physical identities remain in the same Registry history.
        // This uses the production Memory transaction and reducer, not a model
        // of the lock. SQL independent-connection races live in conformance.
        loom::model(|| {
            // A full RegisteredWorker snapshot/fingerprint exceeds generator's
            // small default coroutine stack on this host. Give every actor a
            // larger stack without bounding preemptions or permutations: the
            // two original mutex acquisition orders remain exhaustive.
            let scenario = snapshot_actor(|| {
                let directory = Arc::new(MemoryWorkerDirectory::new());
                directory.register_sync(registration("A"), 0, 10).unwrap();
                directory.register_sync(registration("B"), 10, 10).unwrap();
                let first = directory.clone();
                let old = snapshot_actor(move || first.register_sync(registration("A"), 20, 10));
                let second = directory.clone();
                let fresh = snapshot_actor(move || second.register_sync(registration("C"), 20, 10));
                assert!(matches!(
                    old.join().unwrap(),
                    Err(RegistryError::HistoricalIncarnation { .. })
                ));
                let fresh = fresh.join().unwrap().unwrap();
                assert_eq!(fresh.snapshot.identity.incarnation_id, "C");
                assert_eq!(fresh.snapshot.identity.generation, 3);
                let workers = directory.workers.lock().unwrap();
                let records = workers.get("worker").unwrap();
                assert_eq!(records.len(), 3);
                assert_eq!(
                    records.get(&1).unwrap().snapshot.identity.incarnation_id,
                    "A"
                );
                assert_eq!(records.last_key_value().unwrap().1, &fresh);
            });
            scenario.join().unwrap();
        });
    }

    #[test]
    fn heartbeat_cannot_reopen_a_concurrent_drain() {
        loom::model(|| {
            let directory = Arc::new(MemoryWorkerDirectory::new());
            let registered = directory
                .register_sync(registration("boot-1"), 0, 100)
                .unwrap();
            let identity = registered.snapshot.identity;
            let heartbeat_directory = directory.clone();
            let heartbeat_identity = identity.clone();
            let heartbeat = thread::spawn(move || {
                heartbeat_directory
                    .heartbeat_sync(
                        &heartbeat_identity,
                        WorkerHeartbeat {
                            sequence: 1,
                            ready: true,
                            in_flight: 1,
                            warm_environment_shapes: Default::default(),
                            credential_observations: Default::default(),
                            acp_capability_observations: Default::default(),
                            source_retention: Default::default(),
                        },
                        1,
                        100,
                    )
                    .unwrap();
            });
            let drain_directory = directory.clone();
            let drain = thread::spawn(move || {
                drain_directory.begin_drain_sync(&identity, 50).unwrap();
            });
            heartbeat.join().unwrap();
            drain.join().unwrap();
            let workers = directory.workers.lock().unwrap();
            assert_eq!(
                workers
                    .get("worker")
                    .unwrap()
                    .last_key_value()
                    .unwrap()
                    .1
                    .snapshot
                    .state,
                WorkerState::Draining
            );
        });
    }

    #[test]
    fn stale_heartbeat_and_expiry_cannot_mutate_a_concurrent_replacement() {
        // R9 three concurrent commands: old-time heartbeat, exact expiry and
        // replacement. Heartbeat first => renewed old identity survives both
        // expiry/replacement; expiry/replacement first => successor survives,
        // old heartbeat is rejected. The real mutation mutex and transition
        // functions are used; Registry closure is not process-stop evidence.
        loom::model(|| {
            let directory = Arc::new(MemoryWorkerDirectory::new());
            let first = directory
                .register_sync(registration("boot-1"), 0, 10)
                .unwrap();
            let stale_identity = first.snapshot.identity;
            let heartbeat_directory = directory.clone();
            let expiry_identity = stale_identity.clone();
            let heartbeat = thread::spawn(move || {
                heartbeat_directory
                    .heartbeat_sync(
                        &stale_identity,
                        WorkerHeartbeat {
                            sequence: 1,
                            ready: true,
                            in_flight: 1,
                            warm_environment_shapes: Default::default(),
                            credential_observations: Default::default(),
                            acp_capability_observations: Default::default(),
                            source_retention: Default::default(),
                        },
                        1,
                        100,
                    )
                    .unwrap()
            });
            let replacement_directory = directory.clone();
            let replacement = thread::spawn(move || {
                replacement_directory.register_sync(registration("boot-2"), 11, 100)
            });
            let expiry_directory = directory.clone();
            let expiry = thread::spawn(move || {
                expiry_directory
                    .mutate(&expiry_identity, |current| {
                        transition::expire(current, &expiry_identity, 11)
                    })
                    .unwrap()
            });
            let heartbeat_result = heartbeat.join().unwrap();
            let replacement_result = replacement.join().unwrap();
            let expiry_result = expiry.join().unwrap();
            let workers = directory.workers.lock().unwrap();
            let current = workers.get("worker").unwrap().last_key_value().unwrap().1;
            if current.snapshot.identity.incarnation_id == "boot-2" {
                assert_eq!(current.snapshot.identity.generation, 2);
                assert_eq!(current.heartbeat_sequence, 0);
                assert!(matches!(
                    heartbeat_result,
                    RegistryMutation::StaleIncarnation | RegistryMutation::InvalidTransition
                ));
                assert!(matches!(
                    expiry_result,
                    RegistryMutation::Applied | RegistryMutation::StaleIncarnation
                ));
                assert!(replacement_result.is_ok());
            } else {
                assert_eq!(current.snapshot.identity.incarnation_id, "boot-1");
                assert_eq!(current.snapshot.identity.generation, 1);
                assert_eq!(current.heartbeat_sequence, 1);
                assert_eq!(heartbeat_result, RegistryMutation::Applied);
                assert_eq!(expiry_result, RegistryMutation::InvalidTransition);
                assert!(matches!(
                    replacement_result,
                    Err(RegistryError::SlotOccupied { .. })
                ));
            }
        });
    }

    #[test]
    fn reordered_login_revocation_never_restores_trusted_host_admission() {
        // Cause/effect rules: H1 exact host backend + current login => admitted;
        // H2 Available(seq=1) racing LoginRequired(seq=2) => seq=2 wins under
        // EVERY mutex schedule; H3 a ready cloud Worker with valid login but no
        // exact backend cannot substitute. Readiness remains Ready, so H2 is
        // specifically credential fail-closed, not incidental Worker drain.
        // H1 requires explicit current protocol on BOTH request and Worker; an
        // omitted legacy manifest must fail H0 before credential admission.
        // Runs the production register/heartbeat/admission code with only its
        // mutex replaced by Loom; complements WorkerCredentialLiveness.tla.
        // H4 full snapshots use the SAME existing actor stack policy as the
        // historical-registration model. Default-stack exhaustion is not an
        // invariant counterexample; preemptions/permutations stay exhaustive.
        loom::model(|| {
            let scenario = snapshot_actor(|| {
                use awaken_agent_contract::credential::CredentialRef;
                use awaken_credential_contract::CredentialObservationState;
                use awaken_worker_contract::{
                    PlacementRequirements, TRUSTED_HOST_SANDBOX_BACKEND,
                    WorkerCredentialObservation,
                };
                let credential = CredentialRef {
                    id: "host-login".into(),
                    revision: 1,
                };
                let required = PlacementRequirements {
                    sandbox_backend: Some(TRUSTED_HOST_SANDBOX_BACKEND.into()),
                    required_credentials: [credential.clone()].into(),
                    ..PlacementRequirements::remote_required()
                };
                let directory = Arc::new(MemoryWorkerDirectory::new());
                let mut host = registration("host");
                host.manifest.runtime_protocol = awaken_worker_contract::VersionRange {
                    min: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                    max: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
                };
                host.manifest
                    .sandbox_backends
                    .insert(TRUSTED_HOST_SANDBOX_BACKEND.into());
                let registered = directory.register_sync(host, 0, 100).unwrap();
                let identity = registered.snapshot.identity.clone();
                let available = WorkerCredentialObservation::available(credential.clone(), 0, 100);
                let mut baseline = registered.snapshot;
                baseline.state = WorkerState::Ready;
                baseline.credential_observations.insert(available.clone());
                assert!(baseline.accepts(&required, 1), "H1 positive control");
                let mut legacy = baseline.clone();
                legacy.manifest.runtime_protocol = awaken_worker_contract::VersionRange::ANY;
                legacy.capability_fingerprint = legacy.manifest.fingerprint().unwrap();
                assert!(
                    !legacy.accepts(&required, 1),
                    "H0 omitted protocol fails closed"
                );
                let mut cloud = baseline.clone();
                cloud.manifest.sandbox_backends = ["container".into()].into();
                cloud.capability_fingerprint = cloud.manifest.fingerprint().unwrap();
                assert!(!cloud.accepts(&required, 1), "H3 no trust fallback");
                let mut handles = Vec::new();
                for sequence in [1, 2] {
                    let directory = directory.clone();
                    let identity = identity.clone();
                    let mut observation = available.clone();
                    if sequence == 2 {
                        observation.state = CredentialObservationState::LoginRequired;
                    }
                    handles.push(snapshot_actor(move || {
                        directory
                            .heartbeat_sync(
                                &identity,
                                WorkerHeartbeat {
                                    sequence,
                                    ready: true,
                                    in_flight: 0,
                                    warm_environment_shapes: Default::default(),
                                    credential_observations: [observation].into(),
                                    acp_capability_observations: Default::default(),
                                    source_retention: Default::default(),
                                },
                                1,
                                100,
                            )
                            .unwrap();
                    }));
                }
                for handle in handles {
                    handle.join().unwrap();
                }
                let workers = directory.workers.lock().unwrap();
                let current = workers.get("worker").unwrap().last_key_value().unwrap().1;
                assert_eq!(current.heartbeat_sequence, 2);
                assert_eq!(current.snapshot.state, WorkerState::Ready);
                assert!(!current.snapshot.accepts(&required, 1), "H2 revoked login");
                assert!(
                    !cloud.accepts(&required, 1),
                    "H3 still no cloud substitution"
                );
            });
            scenario.join().unwrap();
        });
    }
}
