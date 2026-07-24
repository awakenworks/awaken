//! Two-process PostgreSQL acceptance for the registered remote Worker protocol.
//!
//! The parent test starts two copies of this test binary as independent Control
//! processes over one PostgreSQL schema. Requests deliberately alternate between
//! them. Control A exits after durably applying a claimed commit but before its
//! HTTP response reaches the Worker; Control B must return the durable duplicate
//! receipt, serve authoritative recovery, and settle the same claim.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitOperationId};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_run_ingress::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchOutcome, DispatchQueue, HttpDispatchQueue,
    PostgresDispatchStore, RegisteredWorker, RegistryError, RegistryMutation, RunClaim,
    RunDispatch, SettleOutcome, WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerRegistration, WorkerSnapshot, WorkerState, commit_payload_hash,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_host::{
    ClaimedCommitService, FixedWorkerLeasePolicy, HeaderWorkerAuthenticator,
    RemoteClaimedRunCommit, SystemWorkerClock, WorkerDispatchService,
    dispatch_transport_router_with_service, registered_worker_transport_router_with_services,
};
use awaken_store_postgres::PostgresCommitCoordinator;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

const CHILD_PORT_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_CHILD_PORT";
const CHILD_DATABASE_URL_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_CHILD_DATABASE_URL";
const CHILD_EXIT_AFTER_COMMIT_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_EXIT_AFTER_COMMIT";
const WORKER_ID: &str = "active-active-worker";
const WORKER_INCARNATION_ID: &str = "active-active-incarnation";

struct ConfiguredWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerDirectory for ConfiguredWorkerDirectory {
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
        Ok(RegistryMutation::Applied)
    }

    async fn begin_drain(
        &self,
        _identity: &WorkerIdentity,
        _deadline_ms: u64,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn mark_quiesced(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
    }

    async fn deregister(
        &self,
        _identity: &WorkerIdentity,
    ) -> Result<RegistryMutation, RegistryError> {
        Ok(RegistryMutation::Applied)
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

fn configured_worker() -> (WorkerIdentity, Arc<dyn WorkerDirectory>) {
    let manifest = WorkerManifest::default();
    let identity = WorkerIdentity::new(WORKER_ID, WORKER_INCARNATION_ID, 1);
    let worker = RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().expect("manifest fingerprint"),
            manifest,
            in_flight: 0,
            available_credentials: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 1,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
    };
    (identity, Arc::new(ConfiguredWorkerDirectory(worker)))
}

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

fn database_url_in_schema(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{schema}")
}

async fn schema_pool(schema: &str) -> Option<PgPool> {
    let admin = PgPool::connect(&database_url()).await.ok()?;
    let _ = admin
        .execute(format!("DROP SCHEMA IF EXISTS {schema} CASCADE").as_str())
        .await;
    admin
        .execute(format!("CREATE SCHEMA {schema}").as_str())
        .await
        .expect("create active-active schema");
    admin.close().await;
    let schema = schema.to_string();
    PgPoolOptions::new()
        .after_connect(move |connection, _metadata| {
            let schema = schema.clone();
            Box::pin(async move {
                connection
                    .execute(format!("SET search_path = {schema}").as_str())
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url())
        .await
        .ok()
}

async fn exit_after_committed_response(
    State(exit_once): State<Arc<AtomicBool>>,
    request: Request,
    next: Next,
) -> Response {
    let claimed_commit = request.uri().path() == "/v1/worker/commit-claimed";
    let response = next.run(request).await;
    if claimed_commit && response.status().is_success() && exit_once.swap(false, Ordering::SeqCst) {
        // The handler has committed and built its success response. Exiting here
        // models a process/network failure in the ambiguous response window.
        std::process::exit(86);
    }
    response
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_active_control_child() {
    let Ok(port) = std::env::var(CHILD_PORT_ENV) else {
        return;
    };
    let database_url =
        std::env::var(CHILD_DATABASE_URL_ENV).expect("child database URL is configured");
    let coordinator = Arc::new(
        PostgresCommitCoordinator::connect(&database_url)
            .await
            .expect("child commit coordinator"),
    );
    let dispatch = Arc::new(
        PostgresDispatchStore::connect(&database_url)
            .await
            .expect("child dispatch store"),
    );
    let (_, directory) = configured_worker();
    let authenticator = Arc::new(HeaderWorkerAuthenticator);
    let dispatch_service = Arc::new(
        WorkerDispatchService::new(
            dispatch.clone(),
            authenticator.clone(),
            Arc::new(SystemWorkerClock),
            Arc::new(FixedWorkerLeasePolicy::new(120_000)),
        )
        .with_worker_directory(directory.clone(), 120_000)
        .with_recovery_source(coordinator.clone()),
    );
    let commit_service = Arc::new(ClaimedCommitService::new(
        dispatch,
        coordinator,
        directory,
        authenticator,
    ));
    let router = registered_worker_transport_router_with_services(
        dispatch_transport_router_with_service(dispatch_service),
        commit_service,
    );
    let router = if std::env::var(CHILD_EXIT_AFTER_COMMIT_ENV).as_deref() == Ok("1") {
        router.layer(axum::middleware::from_fn_with_state(
            Arc::new(AtomicBool::new(true)),
            exit_after_committed_response,
        ))
    } else {
        router
    };
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("bind active-active child");
    axum::serve(listener, router)
        .await
        .expect("serve active-active child");
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(port: u16, database_url: &str, exit_after_commit: bool) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("active_active_control_child")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_PORT_ENV, port.to_string())
            .env(CHILD_DATABASE_URL_ENV, database_url)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if exit_after_commit {
            command.env(CHILD_EXIT_AFTER_COMMIT_ENV, "1");
        }
        Self(Some(command.spawn().expect("spawn Control child")))
    }

    async fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let child = self.0.as_mut().expect("child is present");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = child.try_wait().expect("poll child") {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("Control A exits in the lost-response window")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn reserve_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve port");
    listener.local_addr().expect("reserved address").port()
}

async fn wait_for_port(port: u16, child: &mut ChildGuard) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            if let Some(status) = child
                .0
                .as_mut()
                .expect("child is present")
                .try_wait()
                .expect("poll child")
            {
                panic!("Control child exited before readiness: {status}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Control child becomes ready");
}

fn activation(run_id: &RunId, thread_id: &ThreadId) -> RunActivation {
    RunActivation::new(
        run_id.clone(),
        thread_id.clone(),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("active-active-snapshot".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("active-active-agent".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("active-active-catalog".into()),
                instructions: "active-active protocol acceptance".into(),
                max_steps: 2,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "native"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("active-active-catalog".into()),
        },
        Vec::new(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_control_processes_retry_recover_and_settle_without_sticky_routing() {
    let schema = format!("t_worker_active_active_{}", std::process::id());
    let Some(pool) = schema_pool(&schema).await else {
        eprintln!("skip: no PostgreSQL reachable");
        return;
    };
    let scoped_url = database_url_in_schema(&database_url(), &schema);
    let port_a = reserve_port().await;
    let port_b = reserve_port().await;
    let mut control_a = ChildGuard::spawn(port_a, &scoped_url, true);
    wait_for_port(port_a, &mut control_a).await;
    let mut control_b = ChildGuard::spawn(port_b, &scoped_url, false);
    wait_for_port(port_b, &mut control_b).await;
    let base_a = format!("http://127.0.0.1:{port_a}");
    let base_b = format!("http://127.0.0.1:{port_b}");

    let (identity, _) = configured_worker();

    let queue_a = HttpDispatchQueue::new(&base_a, identity.clone());
    let queue_b = HttpDispatchQueue::new(&base_b, identity.clone());
    let run_id = RunId("active-active-run".into());
    let thread_id = ThreadId("active-active-thread".into());
    queue_a
        .enqueue(RunDispatch::new(activation(&run_id, &thread_id)))
        .await
        .expect("enqueue through Control A");
    let claimed = queue_b
        .claim(&identity.lease_owner(), 120_000, 0)
        .await
        .expect("claim through Control B")
        .expect("shared dispatch is claimable");
    let claim = RunClaim::from(&claimed.lease);
    let initial = queue_a
        .load_recovery_snapshot(&claim)
        .await
        .expect("initial recovery through Control A");
    assert_eq!(initial.thread_version, 0);

    let commit = ThreadCommit::assemble(
        thread_id.clone(),
        RunDisposition::ended(run_id.clone(), EndCause::NaturalEnd),
        true,
        vec![Message::text(
            MessageId("active-active-message".into()),
            Role::Assistant,
            "committed before Control A failed",
        )],
        Vec::new(),
        Vec::new(),
    );
    let operation = CommitOperation {
        operation_id: CommitOperationId::new(run_id.clone(), initial.next_commit_ordinal),
        expected_thread_version: initial.thread_version,
        payload_hash: commit_payload_hash(&commit).expect("hash commit"),
        commit,
    };
    let command = ClaimedCommitCommand {
        claim: claim.clone(),
        operation,
    };
    let ambiguous = RemoteClaimedRunCommit::new(&base_a, identity.clone())
        .commit_operation(command.clone())
        .await;
    assert!(
        ambiguous.is_err(),
        "Control A must disappear before its durable receipt reaches the Worker"
    );
    let exited = control_a.wait_for_exit().await;
    assert_eq!(exited.code(), Some(86), "fault injector exited Control A");

    let duplicate = RemoteClaimedRunCommit::new(&base_b, identity)
        .commit_operation(command)
        .await
        .expect("retry the same logical operation through Control B");
    assert!(
        duplicate.duplicate,
        "Control B returned the durable receipt"
    );
    let recovered = queue_b
        .load_recovery_snapshot(&claim)
        .await
        .expect("authoritative recovery through Control B");
    assert_eq!(recovered.thread_version, 1);
    assert_eq!(recovered.messages.len(), 1);
    assert_eq!(
        queue_b
            .settle(&run_id, claim.epoch, DispatchOutcome::Done, &[])
            .await
            .expect("settle through Control B"),
        SettleOutcome::Applied
    );

    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_commit_receipt")
        .fetch_one(&pool)
        .await
        .expect("count receipts");
    let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_message")
        .fetch_one(&pool)
        .await
        .expect("count messages");
    let completions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runtime_dispatch_completion")
        .fetch_one(&pool)
        .await
        .expect("count completions");
    assert_eq!((receipts, messages, completions), (1, 1, 1));
}
