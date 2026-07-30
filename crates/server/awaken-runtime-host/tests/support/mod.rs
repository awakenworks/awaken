#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use awaken_resource_contract::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind,
    ResourceLifecycleRepository, ResourcePurgeError, ResourcePurgeIntent, ResourcePurgeRepository,
    ResourceReclamationFence, ResourceReference, ResourceReferenceIndex, ResourceReferenceKind,
    ResourceReferenceRecord, ResourceTarget,
};
use awaken_run_ingress::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};

pub async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    address
}

pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

pub fn activation(tag: &str) -> RunActivation {
    RunActivation::new(
        awaken_agent_contract::agent::run::Id(format!("run-{tag}")),
        awaken_agent_contract::agent::thread::Id(format!("thread-{tag}")),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(format!("snapshot-{tag}")),
            metadata: Default::default(),
            root_agent_id: AgentId(format!("agent-{tag}")),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint(format!("catalog-{tag}")),
                instructions: "test Worker boundary".into(),
                max_steps: 1,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("test", "model", "native"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint(format!("snapshot-{tag}-fingerprint")),
        },
        Vec::new(),
    )
}

pub async fn ready_worker(
    worker_id: &str,
) -> (Arc<dyn WorkerDirectory>, awaken_run_ingress::WorkerIdentity) {
    let manifest = WorkerManifest::default();
    let identity = WorkerIdentity::new(worker_id, format!("{worker_id}-boot"), 1);
    let registered = RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 1,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
    };
    (Arc::new(CurrentWorkerDirectory(registered)), identity)
}

struct CurrentWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerDirectory for CurrentWorkerDirectory {
    async fn register(
        &self,
        _registration: WorkerRegistration,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegisteredWorker, RegistryError> {
        Ok(self.0.clone())
    }

    async fn heartbeat(
        &self,
        _identity: &WorkerIdentity,
        _heartbeat: WorkerHeartbeat,
        _now_ms: u64,
        _ttl_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::NotFound)
    }

    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
    }

    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }

    async fn expire(&self, _now_ms: u64) -> Result<Vec<WorkerIdentity>, RegistryError> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct TestResourceLifecycle {
    intents: Mutex<BTreeMap<String, ResourcePurgeIntent>>,
    references: Mutex<BTreeSet<ResourceReferenceRecord>>,
    fences: Mutex<BTreeMap<(ResourceKind, String), String>>,
}

#[async_trait::async_trait]
impl ResourcePurgeRepository for TestResourceLifecycle {
    async fn put(
        &self,
        intent: ResourcePurgeIntent,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        let mut intents = self.intents.lock().unwrap();
        if let Some(existing) = intents.get(&intent.intent_id) {
            return if existing.same_request(&intent) {
                Ok(PutResourcePurgeOutcome::Existing)
            } else {
                Err(ResourcePurgeError::IdempotencyConflict(
                    intent.idempotency_key,
                ))
            };
        }
        intents.insert(intent.intent_id.clone(), intent);
        Ok(PutResourcePurgeOutcome::Inserted)
    }

    async fn get(
        &self,
        intent_id: &str,
    ) -> Result<Option<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(self.intents.lock().unwrap().get(intent_id).cloned())
    }

    async fn recoverable(
        &self,
        _now_unix_ms: u64,
        _limit: usize,
    ) -> Result<Vec<ResourcePurgeIntent>, ResourcePurgeError> {
        Ok(Vec::new())
    }

    async fn save(
        &self,
        _expected_revision: u64,
        intent: ResourcePurgeIntent,
    ) -> Result<(), ResourcePurgeError> {
        self.intents
            .lock()
            .unwrap()
            .insert(intent.intent_id.clone(), intent);
        Ok(())
    }
}

#[async_trait::async_trait]
impl ResourceReferenceIndex for TestResourceLifecycle {
    async fn add_reference(
        &self,
        record: ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self.references.lock().unwrap().insert(record))
    }

    async fn remove_reference(
        &self,
        record: &ResourceReferenceRecord,
    ) -> Result<bool, ResourcePurgeError> {
        Ok(self.references.lock().unwrap().remove(record))
    }

    async fn replace_references(
        &self,
        kind: ResourceReferenceKind,
        reference_id: &str,
        records: Vec<ResourceReferenceRecord>,
    ) -> Result<(), ResourcePurgeError> {
        let mut references = self.references.lock().unwrap();
        references.retain(|record| {
            record.reference.kind != kind || record.reference.reference_id != reference_id
        });
        references.extend(records);
        Ok(())
    }

    async fn references(
        &self,
        target: &ResourceTarget,
    ) -> Result<Vec<ResourceReference>, ResourcePurgeError> {
        Ok(self
            .references
            .lock()
            .unwrap()
            .iter()
            .filter(|record| &record.target == target)
            .map(|record| record.reference.clone())
            .collect())
    }

    async fn references_for_resource(
        &self,
        kind: ResourceKind,
        resource_id: &str,
    ) -> Result<Vec<ResourceReferenceRecord>, ResourcePurgeError> {
        Ok(self
            .references
            .lock()
            .unwrap()
            .iter()
            .filter(|record| record.target.kind == kind && record.target.resource_id == resource_id)
            .cloned()
            .collect())
    }
}

#[async_trait::async_trait]
impl ResourceReclamationFence for TestResourceLifecycle {
    async fn acquire_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<AcquireResourceReclamationOutcome, ResourcePurgeError> {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if let Some(owner) = fences.get(&key) {
            return Ok(if owner == intent_id {
                AcquireResourceReclamationOutcome::AlreadyOwned
            } else {
                AcquireResourceReclamationOutcome::Contended
            });
        }
        fences.insert(key, intent_id.into());
        Ok(AcquireResourceReclamationOutcome::Acquired)
    }

    async fn release_reclamation(
        &self,
        intent_id: &str,
        target: &ResourceTarget,
    ) -> Result<bool, ResourcePurgeError> {
        let key = (target.kind, target.resource_id.clone());
        let mut fences = self.fences.lock().unwrap();
        if fences.get(&key).is_some_and(|owner| owner == intent_id) {
            fences.remove(&key);
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub fn resource_lifecycle() -> Arc<dyn ResourceLifecycleRepository> {
    Arc::new(TestResourceLifecycle::default())
}
