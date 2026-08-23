//! Canonical typed Worker commit transport over real HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_run_ingress::ClaimedCommitService;
use awaken_run_ingress::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchQueue, MemoryDispatchStore, RegisteredWorker,
    RegistryError, RegistryMutation, RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat,
    WorkerIdentity, WorkerManifest, WorkerObservationSource, WorkerRegistration, WorkerSnapshot,
    WorkerState,
};
use awaken_run_ingress_http::{ClaimedCommitHttpService, claimed_commit_router};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_worker_runtime::RemoteClaimedRunCommit;
use awaken_worker_transport_security::HeaderWorkerAuthenticator;

struct CurrentWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerObservationSource for CurrentWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }
}

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

/// Shared Worker-auth middleware decision table:
/// C1 exact identity header -> verified context reaches the claimed-commit
/// handler and an idempotent retry returns the durable receipt; C2 missing or
/// rejected identity -> HTTP 401 before the handler and no commit. Both rules
/// are covered here over real HTTP.
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
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 0,
        observation_sequence: 0,
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
        .claim(&identity.lease_owner(), 30_000, 0, &Default::default())
        .await
        .unwrap()
        .expect("claim");
    let coordinator = Arc::new(MemoryCommitCoordinator::new());
    let service = Arc::new(ClaimedCommitHttpService::new(
        Arc::new(ClaimedCommitService::new(dispatch, coordinator.clone())),
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

    let unauthenticated = reqwest::Client::new()
        .post(format!("http://{address}/v1/worker/commit-claimed"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("C2 unauthenticated request reaches middleware");
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "C2"
    );
    assert!(
        CommittedThreadView::committed_messages(
            &*coordinator,
            &ThreadId("typed-commit-thread".into())
        )
        .is_empty(),
        "C2 rejects before commit"
    );

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
    let messages = CommittedThreadView::committed_messages(
        &*coordinator,
        &ThreadId("typed-commit-thread".into()),
    );
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text_content(), "typed commit");
}

/// Cause graph for ambiguous remote commit responses:
/// C1=operation has stable identity, C2=server returns 5xx, C3=retry budget remains.
///
/// Decision table:
/// | Rule | C1 | C2 | C3 | Result |
/// | T1   | Y  | N  | -  | return receipt without retry |
/// | T2   | Y  | Y  | Y  | retry identical operation and return durable receipt |
/// | T3   | Y  | Y  | N  | fail after the bounded retry budget |
#[tokio::test(flavor = "multi_thread")]
async fn t2_retries_ambiguous_server_failure_with_the_same_operation() {
    let identity = WorkerIdentity::new("retry-worker", "retry-boot", 1);
    let commit = terminal_commit();
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("typed-commit-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    };
    let receipt = awaken_agent_contract::thread::commit::operation::CommitReceipt {
        operation_id: operation.operation_id.clone(),
        commit_sequence: 1,
        thread_version: 1,
        payload_hash: operation.payload_hash.clone(),
        duplicate: true,
    };
    let command = ClaimedCommitCommand {
        claim: RunClaim {
            run_id: RunId("typed-commit-run".into()),
            owner: identity.lease_owner(),
            epoch: 1,
        },
        operation: operation.clone(),
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let handler_receipt = receipt.clone();
    let app = axum::Router::new().route(
        "/v1/worker/commit-claimed",
        axum::routing::post({
            let attempts = attempts.clone();
            let bodies = bodies.clone();
            move |axum::Json(body): axum::Json<serde_json::Value>| {
                let attempts = attempts.clone();
                let bodies = bodies.clone();
                let receipt = handler_receipt.clone();
                async move {
                    bodies.lock().await.push(body);
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            axum::Json(serde_json::json!({"error": "receipt lost"})),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            axum::Json(serde_json::to_value(receipt).unwrap()),
                        )
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let got = RemoteClaimedRunCommit::new(format!("http://{address}"), identity)
        .commit_operation(command)
        .await
        .expect("T2 retries the ambiguous 5xx");

    assert_eq!(got, receipt);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], bodies[1], "retry preserves the exact operation");
    assert_eq!(
        bodies[0]["operation"],
        serde_json::to_value(operation).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn non_retryable_claimed_commit_preserves_the_server_cause() {
    let identity = WorkerIdentity::new("diagnostic-worker", "diagnostic-boot", 1);
    let app = axum::Router::new().route(
        "/v1/worker/commit-claimed",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "run claim is stale"})),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let commit = terminal_commit();
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("diagnostic-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    };
    let error = RemoteClaimedRunCommit::new(format!("http://{address}"), identity.clone())
        .commit_operation(ClaimedCommitCommand {
            claim: RunClaim {
                run_id: RunId("diagnostic-run".into()),
                owner: identity.lease_owner(),
                epoch: 1,
            },
            operation,
        })
        .await
        .expect_err("400 remains fail-closed");
    assert!(
        error
            .to_string()
            .contains("400 Bad Request: run claim is stale")
    );
}
