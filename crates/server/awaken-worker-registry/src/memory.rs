#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
use std::collections::BTreeMap;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_worker_contract::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerRegistration,
};

use crate::transition;

#[derive(Default)]
pub struct MemoryWorkerDirectory {
    workers: Mutex<BTreeMap<String, RegisteredWorker>>,
}

impl MemoryWorkerDirectory {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn register_sync(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let (record, changed) = transition::register(
            workers.get(&registration.worker_id),
            registration,
            now_ms,
            ttl_ms,
        )?;
        if changed {
            workers.insert(record.snapshot.identity.worker_id.clone(), record.clone());
        }
        Ok(record)
    }

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

    fn begin_drain_sync(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::begin_drain(current, identity, deadline_ms)
        })
    }

    fn mutate(
        &self,
        identity: &WorkerIdentity,
        decide: impl FnOnce(Option<&RegisteredWorker>) -> (Option<RegisteredWorker>, RegistryMutation),
    ) -> Result<RegistryMutation, RegistryError> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let (next, outcome) = decide(workers.get(&identity.worker_id));
        if let Some(next) = next {
            workers.insert(identity.worker_id.clone(), next);
        }
        Ok(outcome)
    }
}

#[async_trait]
impl WorkerDirectory for MemoryWorkerDirectory {
    async fn register(
        &self,
        registration: WorkerRegistration,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        self.register_sync(registration, now_ms, ttl_ms)
    }

    async fn heartbeat(
        &self,
        identity: &WorkerIdentity,
        heartbeat: WorkerHeartbeat,
        now_ms: u64,
        ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.heartbeat_sync(identity, heartbeat, now_ms, ttl_ms)
    }

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.begin_drain_sync(identity, deadline_ms)
    }

    async fn mark_quiesced(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| transition::quiesce(current, identity))
    }

    async fn deregister(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::deregister(current, identity)
        })
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok(self
            .workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?
            .get(worker_id)
            .cloned())
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(self
            .workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?
            .values()
            .cloned()
            .collect())
    }

    async fn expire(&self, now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        let mut workers = self
            .workers
            .lock()
            .map_err(|_| RegistryError::Persistence("worker registry mutex poisoned".into()))?;
        let mut expired = Vec::new();
        for record in workers.values_mut() {
            if let Some(next) = transition::expire(record, now_ms) {
                expired.push(next.snapshot.identity.clone());
                *record = next;
            }
        }
        Ok(expired)
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use awaken_worker_contract::{WorkerManifest, WorkerState};
    use loom::sync::Arc;
    use loom::thread;

    fn registration(incarnation: &str) -> WorkerRegistration {
        WorkerRegistration {
            worker_id: "worker".to_string(),
            incarnation_id: incarnation.to_string(),
            manifest: WorkerManifest::default(),
        }
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
                workers.get("worker").unwrap().snapshot.state,
                WorkerState::Draining
            );
        });
    }

    #[test]
    fn stale_heartbeat_cannot_mutate_a_concurrent_replacement() {
        loom::model(|| {
            let directory = Arc::new(MemoryWorkerDirectory::new());
            let first = directory
                .register_sync(registration("boot-1"), 0, 10)
                .unwrap();
            let stale_identity = first.snapshot.identity;
            let heartbeat_directory = directory.clone();
            let heartbeat = thread::spawn(move || {
                heartbeat_directory
                    .heartbeat_sync(
                        &stale_identity,
                        WorkerHeartbeat {
                            sequence: 1,
                            ready: true,
                            in_flight: 1,
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
            let heartbeat_result = heartbeat.join().unwrap();
            let replacement_result = replacement.join().unwrap();
            let workers = directory.workers.lock().unwrap();
            let current = workers.get("worker").unwrap();
            if current.snapshot.identity.incarnation_id == "boot-2" {
                assert_eq!(current.snapshot.identity.generation, 2);
                assert_eq!(current.heartbeat_sequence, 0);
                assert_eq!(heartbeat_result, RegistryMutation::StaleIncarnation);
                assert!(replacement_result.is_ok());
            } else {
                assert_eq!(current.snapshot.identity.incarnation_id, "boot-1");
                assert_eq!(current.snapshot.identity.generation, 1);
                assert_eq!(current.heartbeat_sequence, 1);
                assert!(matches!(
                    replacement_result,
                    Err(RegistryError::SlotOccupied { .. })
                ));
            }
        });
    }
}
