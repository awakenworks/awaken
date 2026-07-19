//! The write-plane worker seam over real HTTP: a database-less worker's
//! `RemoteCoordinator` pushes a staged `ThreadCommit` to the cell server's
//! `commit_ingest_router`, which applies it through the thread's single writer.
//! The committed facts are then readable from the server's store, and a redelivery
//! is idempotent (at-least-once → exactly-once effect).

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Coordinator;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_run_ingress::{
    ClaimedRunCommit, DispatchQueue, MemoryDispatchStore, RegisteredWorker, RegistryError,
    RegistryMutation, RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity,
    WorkerManifest, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    HeaderWorkerAuthenticator, RemoteClaimedRunCommit, RemoteCoordinator, SharedHost,
    claimed_commit_ingest_router, claimed_commit_ingest_router_with_directory,
    commit_ingest_router,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

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

struct OkModel;

#[async_trait::async_trait]
impl LlmExecutor for OkModel {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text("ok"),
            usage: None,
            stop_reason: None,
        })
    }
}

fn thread_commit() -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId("t1".into()),
        run: RunDisposition::ended(RunId("run-A".into()), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MessageId("a1".into()),
            Role::Assistant,
            "committed by a db-less worker",
        )],
        state: vec![],
        events: vec![],
    }
}

fn activation(run: &str, thread: &str) -> RunActivation {
    RunActivation::new(
        RunId(run.into()),
        ThreadId(thread.into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snap".into()),
            root_agent_id: AgentId("agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("fp".into()),
                instructions: "test".into(),
                max_steps: 2,
                delegation_limits: Default::default(),
                model_binding: ModelBinding::new("provider", "model", "local"),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("fp".into()),
        },
        Vec::new(),
    )
}

fn claimed_commit(run: &str, thread: &str, text: &str) -> ThreadCommit {
    ThreadCommit {
        thread_id: ThreadId(thread.into()),
        run: RunDisposition::ended(RunId(run.into()), EndCause::NaturalEnd),
        messages: vec![Message::text(
            MessageId(format!("message-{run}")),
            Role::Assistant,
            text,
        )],
        state: Vec::new(),
        events: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn db_less_worker_pushes_facts_and_the_server_commits_them() {
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = commit_ingest_router(host.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // The worker holds only this HTTP client — no store, no coordinator of its own.
    let coordinator = RemoteCoordinator::new(format!("http://{addr}"));

    let record = coordinator
        .commit(thread_commit())
        .await
        .expect("commit over http");
    assert!(
        record.sequence >= 1,
        "the server sequenced the commit: {record:?}"
    );

    // The fact is now committed truth on the SERVER's store, readable back.
    let committed = host.committed_messages("t1").await;
    assert_eq!(
        committed.len(),
        1,
        "the worker's message committed on the server"
    );
    assert!(
        committed[0].text_content().contains("db-less worker"),
        "the committed message is the one the worker pushed"
    );

    // At-least-once redelivery is idempotent: the same commit does not duplicate.
    coordinator
        .commit(thread_commit())
        .await
        .expect("redelivered commit accepted");
    let after = host.committed_messages("t1").await;
    assert_eq!(
        after.len(),
        1,
        "a redelivered commit is a no-op (idempotent), not a duplicate: {after:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_claim_and_commit_is_one_atomic_server_operation() {
    let memory = Arc::new(MemoryDispatchStore::new());
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    memory
        .enqueue(RunDispatch::new(activation("atomic-run", "atomic-thread")))
        .await
        .unwrap();
    let stale = memory
        .claim("worker-a", 100, 0)
        .await
        .unwrap()
        .expect("first claim");
    let current = memory
        .claim("worker-b", 100, 200)
        .await
        .unwrap()
        .expect("recovery claim");
    let router = claimed_commit_ingest_router(
        host.clone(),
        memory.clone() as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
    );

    let unclaimed = Request::builder()
        .method("POST")
        .uri("/v1/worker/commit")
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", "worker-b")
        .body(Body::from(serde_json::to_vec(&thread_commit()).unwrap()))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(unclaimed).await.unwrap().status(),
        StatusCode::NOT_FOUND,
        "the production router does not expose unclaimed commits"
    );

    let wrong_identity = Request::builder()
        .method("POST")
        .uri("/v1/worker/commit-claimed")
        .header("content-type", "application/json")
        .header("x-awaken-worker-id", "worker-a")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "claim": RunClaim::from(&current.lease),
                "commit": claimed_commit("atomic-run", "atomic-thread", "forged")
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(wrong_identity)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED,
        "authenticated identity must match the claim owner"
    );
    assert!(host.committed_messages("atomic-thread").await.is_empty());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let remote = RemoteClaimedRunCommit::new(format!("http://{addr}"));

    assert!(
        remote
            .commit(
                &RunClaim::from(&stale.lease),
                claimed_commit("atomic-run", "atomic-thread", "stale"),
            )
            .await
            .is_err(),
        "a stale remote claim is rejected before its ThreadCommit"
    );
    assert!(host.committed_messages("atomic-thread").await.is_empty());

    remote
        .commit(
            &RunClaim::from(&current.lease),
            claimed_commit("atomic-run", "atomic-thread", "current"),
        )
        .await
        .expect("current claim commits atomically");
    let messages = host.committed_messages("atomic-thread").await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text_content(), "current");
}

#[tokio::test(flavor = "multi_thread")]
async fn registered_claimed_commit_requires_the_current_worker_incarnation() {
    let manifest = WorkerManifest::default();
    let identity = WorkerIdentity::new("worker-a", "boot-current", 1);
    let directory = Arc::new(CurrentWorkerDirectory(RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().unwrap(),
            manifest,
            in_flight: 0,
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
    }));
    let memory = Arc::new(MemoryDispatchStore::new());
    memory
        .enqueue(RunDispatch::new(activation(
            "registered-run",
            "registered-thread",
        )))
        .await
        .unwrap();
    let claimed = memory
        .claim(&identity.lease_owner(), 1_000, 0)
        .await
        .unwrap()
        .unwrap();
    let host = Arc::new(SharedHost::new(Arc::new(OkModel), "stub"));
    let router = claimed_commit_ingest_router_with_directory(
        host.clone(),
        memory as Arc<dyn DispatchQueue>,
        Arc::new(HeaderWorkerAuthenticator),
        directory,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let claim = RunClaim::from(&claimed.lease);

    let stale = RemoteClaimedRunCommit::new(format!("http://{address}"))
        .with_worker_identity(WorkerIdentity::new("worker-a", "boot-stale", 0));
    assert!(
        stale
            .commit(
                &claim,
                claimed_commit("registered-run", "registered-thread", "stale")
            )
            .await
            .is_err()
    );
    let current =
        RemoteClaimedRunCommit::new(format!("http://{address}")).with_worker_identity(identity);
    current
        .commit(
            &claim,
            claimed_commit("registered-run", "registered-thread", "current"),
        )
        .await
        .unwrap();
    assert_eq!(host.committed_messages("registered-thread").await.len(), 1);
}
