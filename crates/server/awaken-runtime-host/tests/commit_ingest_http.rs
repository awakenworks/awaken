//! Canonical typed Worker commit transport over real HTTP.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitOperationId};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchQueue, MemoryDispatchStore, RegisteredWorker,
    RegistryError, RegistryMutation, RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerState,
    commit_payload_hash,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    ClaimedCommitService, HeaderWorkerAuthenticator, RemoteClaimedRunCommit, claimed_commit_router,
};
use awaken_store_inmem::MemoryCommitCoordinator;

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

fn activation(run: &str, thread: &str) -> RunActivation {
    let fingerprint = CatalogFingerprint("typed-commit-fingerprint".into());
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("typed-commit-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("typed-commit-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: "test".into(),
                max_steps: 2,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "local"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        Vec::new(),
    )
}

fn terminal_commit() -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId("typed-commit-thread".into()),
        run: RunDisposition::ended(RunId("typed-commit-run".into()), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MessageId("typed-commit-message".into()),
            Role::Assistant,
            "typed commit",
        )],
        state: Vec::new(),
        events: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn registered_worker_commits_one_idempotent_versioned_operation() {
    let manifest = WorkerManifest::default();
    let identity = WorkerIdentity::new("typed-worker", "typed-boot", 1);
    let directory = Arc::new(CurrentWorkerDirectory(RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            available_credentials: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
    }));
    let dispatch = Arc::new(MemoryDispatchStore::new());
    dispatch
        .enqueue(RunDispatch::new(activation(
            "typed-commit-run",
            "typed-commit-thread",
        )))
        .await
        .unwrap();
    let claimed = dispatch
        .claim(&identity.lease_owner(), 30_000, 0)
        .await
        .unwrap()
        .expect("claim");
    let coordinator = Arc::new(MemoryCommitCoordinator::new());
    let service = Arc::new(ClaimedCommitService::new(
        dispatch,
        coordinator.clone(),
        directory,
        Arc::new(HeaderWorkerAuthenticator),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, claimed_commit_router(service))
            .await
            .unwrap()
    });

    let commit = terminal_commit();
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("typed-commit-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    };
    let command = ClaimedCommitCommand {
        claim: RunClaim::from(&claimed.lease),
        operation,
    };
    let remote = RemoteClaimedRunCommit::new(format!("http://{address}"), identity);
    let first = remote.commit_operation(command.clone()).await.unwrap();
    let duplicate = remote.commit_operation(command).await.unwrap();

    assert!(!first.duplicate);
    assert!(duplicate.duplicate);
    assert_eq!(first.commit_sequence, duplicate.commit_sequence);
    let messages =
        ThreadReader::committed_messages(&*coordinator, &ThreadId("typed-commit-thread".into()));
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text_content(), "typed commit");
}
