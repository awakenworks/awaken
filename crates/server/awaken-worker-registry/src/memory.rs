use std::collections::BTreeMap;
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

    async fn heartbeat(
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

    async fn begin_drain(
        &self,
        identity: &WorkerIdentity,
        deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        self.mutate(identity, |current| {
            transition::begin_drain(current, identity, deadline_ms)
        })
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
