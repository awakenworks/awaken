#![allow(dead_code)]

use awaken_run_ingress::{
    RegisteredWorker, RegistryError, RegistryMutation, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use std::sync::Arc;

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
