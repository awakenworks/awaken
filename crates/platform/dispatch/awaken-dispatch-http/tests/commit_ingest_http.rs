// @formal evidence ingress.commit_epoch_atomic_remote_custom
// @formal evidence session_run.stale_work_epoch_cannot_observe_authoritative_thread_commit
//! Canonical typed Worker commit transport over real HTTP.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::coordinator::Error as CommitError;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, CommitReceipt, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_coordinator_client::RemoteClaimedRunCommit;
use awaken_dispatch::ClaimedCommitService;
use awaken_dispatch::{
    ClaimedCommitCommand, ClaimedRunCommit, MemoryDispatchStore, RegisteredWorker, RegistryError,
    RegistryMutation, RunClaim, RunDispatch, WorkerDirectory, WorkerHeartbeat, WorkerIdentity,
    WorkerManifest, WorkerObservationSource, WorkerRegistration, WorkerSnapshot, WorkerState,
};
use awaken_dispatch_http::{ClaimedCommitHttpService, claimed_commit_router};
use awaken_dispatch_testkit::ConformanceClock as _;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::runs::RunError;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_worker_transport_security::HeaderWorkerAuthenticator;

struct ObservedCommitWriter {
    writer: ClaimedCommitService,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl ClaimedRunCommit<RunError> for ObservedCommitWriter {
    async fn commit(
        &self,
        claim: &RunClaim,
        commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        self.writer.commit(claim, commit).await
    }

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, RunError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.writer.commit_operation(command).await
    }
}

struct CurrentWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerObservationSource for CurrentWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for CurrentWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for CurrentWorkerDirectory {
    async fn incarnation(
        &self,
        identity: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<
        Option<awaken_worker_contract::RegisteredWorker>,
        awaken_worker_contract::RegistryError,
    > {
        awaken_worker_contract::read_current_incarnation(self, identity).await
    }
}

#[async_trait::async_trait]
impl WorkerDirectory for CurrentWorkerDirectory {
    async fn lock_incarnation(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<Option<awaken_worker_contract::WorkerIncarnationGuard>, RegistryError> {
        Err(RegistryError::Persistence(
            "fixture forbids physical admission".into(),
        ))
    }

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

    async fn expire_incarnation(
        &self,
        _identity: &WorkerIdentity,
        _now_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::InvalidTransition)
    }

    async fn record_process_exit(
        &self,
        _identity: &WorkerIdentity,
        _receipt: awaken_dispatch::WorkerProcessExit,
    ) -> Result<RegistryMutation, RegistryError> {
        Err(RegistryError::Persistence(
            "process-exit persistence is outside this read-oriented fixture".into(),
        ))
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
                max_steps: Some(2),
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

/// Shared remote claimed-commit decision table. Causes: C1 Worker identity is
/// exact/missing; C2 the Dispatch epoch is current/stale after same-incarnation
/// reclaim; C3 the stable operation is fresh/replayed. Effects: E1 reject before
/// committed truth; E2 append exactly once; E3 return the durable duplicate
/// receipt. Rules: R1=!C1=>E1; R2=C1+stale C2=>E1; R3=C1+current C2+fresh
/// C3=>E2; R4=C1+current C2+replayed C3=>E3. Run/Thread/operation/hash mismatch
/// partitions are owned by the existing Host claimed-child table; this real HTTP
/// table cross-checks authentication, ABA fencing and idempotent transport.
/// C4 payload is above the incidental 2 MiB JSON default but below the finite
/// commit wire budget; C5 it exceeds that budget. R5=C1+current C2+C4=>E2/E3
/// with exact large bytes; R6=C1+C5=>413 before any additional committed truth.
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
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    }));
    let clock = awaken_dispatch_testkit::ManualStoreClock::new(0);
    let dispatch = Arc::new(MemoryDispatchStore::new(clock.clock()));
    dispatch
        .enqueue(RunDispatch::new(activation(
            "typed-commit-run",
            "typed-commit-thread",
        )))
        .await
        .unwrap();
    clock.set(0);
    let stale = dispatch
        .claim(&identity.lease_owner(), 30_000, &Default::default())
        .await
        .unwrap()
        .expect("initial claim");
    clock.set(stale.lease.expires_ms().saturating_add(1));
    let current = dispatch
        .claim(&identity.lease_owner(), 30_000, &Default::default())
        .await
        .unwrap()
        .expect("same-incarnation reclaim");
    assert_eq!(current.lease.epoch(), stale.lease.epoch() + 1, "R2/C2");
    let coordinator = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let selected = Arc::new(ObservedCommitWriter {
        writer: ClaimedCommitService::new(dispatch, coordinator.clone()),
        calls: AtomicUsize::new(0),
    });
    let service = Arc::new(ClaimedCommitHttpService::new(
        selected.clone(),
        directory,
        Arc::new(HeaderWorkerAuthenticator),
        awaken_time::system_clock(),
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
    assert_eq!(selected.calls.load(Ordering::Relaxed), 0);
    assert!(
        CommittedThreadView::committed_messages(
            &*coordinator,
            &ThreadId("typed-commit-thread".into())
        )
        .is_empty(),
        "R1/E1"
    );

    let mut commit = terminal_commit();
    let large_text = "x".repeat(3 * 1024 * 1024);
    commit.messages = vec![Message::text(
        MessageId("typed-commit-message".into()),
        Role::Assistant,
        large_text.clone(),
    )];
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(RunId("typed-commit-run".into()), 0),
        expected_thread_version: 0,
        payload_hash: commit_payload_hash(&commit).unwrap(),
        commit,
    };
    let command = ClaimedCommitCommand {
        claim: RunClaim::from(&current.lease),
        operation,
    };
    let remote = RemoteClaimedRunCommit::new(format!("http://{address}"), identity);
    let stale_error = remote
        .commit_operation(ClaimedCommitCommand {
            claim: RunClaim::from(&stale.lease),
            operation: command.operation.clone(),
        })
        .await
        .expect_err("R2 stale epoch is fenced before commit");
    assert!(
        stale_error.to_string().contains("run claim is stale"),
        "R2/E1"
    );
    assert!(
        CommittedThreadView::committed_messages(
            &*coordinator,
            &ThreadId("typed-commit-thread".into())
        )
        .is_empty(),
        "R2/E1"
    );
    // R5 exact identity/current epoch with body B but retained hash(body A)
    // => existing InvalidRequest text, no durable prefix change. R3 following
    // the refusal succeeds, so the claim guard is not retained by the failure.
    // The server and local stores consume the same canonical validation owner.
    let before_invalid = coordinator
        .recovery_snapshot(
            &command.operation.commit.thread_id,
            &command.operation.operation_id.run_id,
        )
        .await
        .unwrap();
    let before_count = coordinator.commit_count();
    let mut invalid = command.clone();
    invalid.operation.commit.messages.clear();
    let hash_error = remote.commit_operation(invalid).await.unwrap_err();
    assert!(
        hash_error
            .to_string()
            .contains("commit operation payload hash does not match ThreadCommit"),
        "R5 original public error"
    );
    assert_eq!(coordinator.commit_count(), before_count, "R5 zero append");
    assert_eq!(
        coordinator
            .recovery_snapshot(
                &command.operation.commit.thread_id,
                &command.operation.operation_id.run_id
            )
            .await
            .unwrap(),
        before_invalid,
        "R5 complete recovery prefix unchanged"
    );
    let first = remote.commit_operation(command.clone()).await.unwrap();
    let duplicate = remote.commit_operation(command).await.unwrap();

    assert!(!first.duplicate, "R3/E2");
    assert!(duplicate.duplicate, "R4/E3");
    assert_eq!(first.commit_sequence, duplicate.commit_sequence, "R4/E3");
    let messages = CommittedThreadView::committed_messages(
        &*coordinator,
        &ThreadId("typed-commit-thread".into()),
    );
    assert_eq!(messages.len(), 1, "R3-R4/E2-E3");
    assert_eq!(messages[0].text_content(), large_text, "R3-R5/E2-E3");
    let oversized = reqwest::Client::new()
        .post(format!("http://{address}/v1/worker/commit-claimed"))
        .header(
            awaken_worker_transport_security::WORKER_ID_HEADER,
            "typed-worker",
        )
        .header("content-type", "application/json")
        .body(vec![
            b' ';
            awaken_dispatch_contract::MAX_CLAIMED_COMMIT_BODY_BYTES
                + 1
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(
        oversized.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "R6"
    );
    assert_eq!(
        CommittedThreadView::committed_messages(
            &*coordinator,
            &ThreadId("typed-commit-thread".into())
        )
        .len(),
        1,
        "R6 leaves truth unchanged"
    );
    // Stale epoch, invalid body, fresh operation, exact retry. Only
    // authenticated requests within the wire limit reach the selected writer.
    assert_eq!(selected.calls.load(Ordering::Relaxed), 4);
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

use awaken_dispatch_contract::{DispatchClaims as _, DispatchEnqueue as _};
