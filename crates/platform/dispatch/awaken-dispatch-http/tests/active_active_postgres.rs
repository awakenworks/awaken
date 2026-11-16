//! Two-process PostgreSQL acceptance for the registered remote Worker protocol.
//!
//! The parent test starts two copies of this test binary as independent Control
//! processes over one PostgreSQL schema. Requests deliberately alternate between
//! them. Control A exits after durably applying a claimed commit but before its
//! HTTP response reaches the Worker; Control B must return the durable duplicate
//! receipt, serve authoritative recovery, and settle the same claim.

#[path = "active_active_postgres/claim_receipt_races.rs"]
mod claim_receipt_races;
mod postgres_support;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitOperationId, commit_payload_hash,
};
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use awaken_agent_contract::thread::read::recovery::LoadedRunRecovery;
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_coordinator_client::RemoteClaimedRunCommit;
use awaken_dispatch::ClaimedCommitService;
use awaken_dispatch::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchOutcome, PostgresDispatchStore,
    RegisteredWorker, RegistryError, RegistryMutation, RunClaim, RunDispatch, SettleOutcome,
    WorkerDirectory, WorkerHeartbeat, WorkerIdentity, WorkerObservationSource, WorkerRegistration,
    WorkerSnapshot, WorkerState,
};
use awaken_dispatch_contract::ClaimedCheckpointTransport as _;
use awaken_dispatch_http::{
    RegisteredDispatchDependencies, WorkerDispatchService, dispatch_transport_router_with_service,
    registered_worker_transport_router_with_services,
};
use awaken_dispatch_testkit::worker_dispatch::worker_dispatch_dependencies;
use awaken_dispatch_testkit::worker_http::current_dispatch_codec_manifest;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, HeaderWorkerAuthenticator};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use sqlx::Executor;
use sqlx::postgres::{PgPool, PgPoolOptions};

const CHILD_PORT_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_CHILD_PORT";
const CHILD_DATABASE_URL_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_CHILD_DATABASE_URL";
const CHILD_EXIT_AFTER_RESPONSE_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_EXIT_AFTER_RESPONSE";
const WORKER_ID: &str = "active-active-worker";
const WORKER_INCARNATION_ID: &str = "active-active-incarnation";

struct ConfiguredWorkerDirectory(RegisteredWorker);

#[async_trait::async_trait]
impl WorkerObservationSource for ConfiguredWorkerDirectory {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        Ok(vec![self.0.clone()])
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerCurrentSource for ConfiguredWorkerDirectory {
    async fn current(&self, worker_id: &str) -> Result<Option<RegisteredWorker>, RegistryError> {
        Ok((worker_id == self.0.snapshot.identity.worker_id).then(|| self.0.clone()))
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for ConfiguredWorkerDirectory {
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
impl WorkerDirectory for ConfiguredWorkerDirectory {
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

fn configured_worker() -> (WorkerIdentity, Arc<dyn WorkerDirectory>) {
    // P1 the two processes serve the current binary => explicitly advertise its
    // execution protocol. ANY/omitted is intentionally not positive evidence;
    // the transport admission decision table covers rejection of those peers.
    let mut manifest = current_dispatch_codec_manifest();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let identity = WorkerIdentity::new(WORKER_ID, WORKER_INCARNATION_ID, 1);
    let worker = RegisteredWorker {
        snapshot: WorkerSnapshot {
            identity: identity.clone(),
            state: WorkerState::Ready,
            capability_fingerprint: manifest.fingerprint().expect("manifest fingerprint"),
            manifest,
            in_flight: 0,
            warm_environment_shapes: Default::default(),
            credential_observations: Default::default(),
            acp_capability_observations: Default::default(),
            expires_at_ms: u64::MAX,
        },
        heartbeat_sequence: 1,
        observation_sequence: 0,
        registered_at_ms: 0,
        heartbeat_at_ms: 0,
        drain_deadline_ms: None,
        process_exit: None,
        create_ledger: None,
        source_retention: Default::default(),
    };
    (identity, Arc::new(ConfiguredWorkerDirectory(worker)))
}

fn database_url_in_schema(base: &str, schema: &str) -> String {
    let separator = if base.contains('?') { '&' } else { '?' };
    format!("{base}{separator}options=-c%20search_path%3D{schema}")
}

async fn schema_pool(schema: &str) -> Option<(PgPool, String)> {
    assert!(
        !schema.is_empty()
            && !schema.as_bytes()[0].is_ascii_digit()
            && schema
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    let url = postgres_support::database_url()?;
    let admin = postgres_support::connect(PgPool::connect(&url)).await;
    // Only the checked active-active fixture identifier enters schema DDL.
    admin
        .execute(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE"
        )))
        .await
        .expect("drop owned active-active schema");
    admin
        .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .await
        .expect("create active-active schema");
    admin.close().await;
    let schema = schema.to_string();
    let pool = postgres_support::connect(
        PgPoolOptions::new()
            .after_connect(move |connection, _metadata| {
                let schema = schema.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path', $1, false)")
                        .bind(schema)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url),
    )
    .await;
    Some((pool, url))
}

async fn exit_after_committed_response(
    State((fault_path, exit_once)): State<(String, Arc<AtomicBool>)>,
    request: Request,
    next: Next,
) -> Response {
    let fault_response = request.uri().path() == fault_path;
    let response = next.run(request).await;
    if fault_response && response.status().is_success() && exit_once.swap(false, Ordering::SeqCst) {
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
        PostgresCommitCoordinator::connect(&database_url, 10)
            .await
            .expect("child commit coordinator"),
    );
    let dispatch = Arc::new(
        PostgresDispatchStore::connect(&database_url, 10)
            .await
            .expect("child dispatch store"),
    );
    let (_, directory) = configured_worker();
    let authenticator = Arc::new(HeaderWorkerAuthenticator);
    let dispatch_service = Arc::new(WorkerDispatchService::new(RegisteredDispatchDependencies {
        authenticator: authenticator.clone(),
        lease_policy: Arc::new(FixedWorkerLeasePolicy::new(120_000)),
        registry_ttl_ms: 120_000,
        recovery: coordinator.clone(),
        ..worker_dispatch_dependencies(
            dispatch.clone(),
            directory.clone(),
            awaken_time::system_clock(),
        )
    }));
    let commit_service = Arc::new(awaken_dispatch_http::ClaimedCommitHttpService::new(
        Arc::new(ClaimedCommitService::new(dispatch, coordinator)),
        directory,
        authenticator,
        awaken_time::system_clock(),
    ));
    let router = registered_worker_transport_router_with_services(
        dispatch_transport_router_with_service(dispatch_service),
        axum::Router::new(),
        commit_service,
    );
    let router = if let Ok(fault_path) = std::env::var(CHILD_EXIT_AFTER_RESPONSE_ENV) {
        router.layer(axum::middleware::from_fn_with_state(
            (fault_path, Arc::new(AtomicBool::new(true))),
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

struct ChildGuard {
    child: Option<Child>,
    output: tempfile::NamedTempFile,
}

impl ChildGuard {
    fn spawn(port: u16, database_url: &str, exit_after_commit: bool) -> Self {
        Self::spawn_with_response_fault(
            port,
            database_url,
            exit_after_commit.then_some("/v1/worker/commit-claimed"),
        )
    }

    fn spawn_with_response_fault(port: u16, database_url: &str, fault_path: Option<&str>) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("active_active_control_child")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_PORT_ENV, port.to_string())
            .env(CHILD_DATABASE_URL_ENV, database_url);
        if let Some(path) = fault_path {
            command.env(CHILD_EXIT_AFTER_RESPONSE_ENV, path);
        }
        Self::spawn_command(&mut command)
    }

    fn spawn_command(command: &mut Command) -> Self {
        // A nested libtest must never write directly to the parent's result
        // stream. Files also avoid pipe backpressure while the server runs.
        let output = tempfile::NamedTempFile::new().expect("child diagnostic file");
        let child = command
            .stdin(Stdio::null())
            .stdout(output.as_file().try_clone().expect("child stdout file"))
            .stderr(output.as_file().try_clone().expect("child stderr file"))
            .spawn()
            .expect("spawn Control child");
        Self {
            child: Some(child),
            output,
        }
    }

    fn diagnostics(&self) -> std::io::Result<String> {
        const LIMIT: u64 = 64 * 1024;
        let file = std::fs::File::open(self.output.path())?;
        let truncated = file.metadata()?.len() > LIMIT;
        let mut bytes = Vec::new();
        file.take(LIMIT).read_to_end(&mut bytes)?;
        let mut text = String::new();
        for line in String::from_utf8_lossy(&bytes).lines() {
            // Prefix EVERY line, including nested test summaries, so retained
            // diagnostics cannot impersonate the parent's named outcomes.
            text.push_str("active-active child: ");
            text.push_str(line);
            text.push('\n');
        }
        if truncated {
            text.push_str("active-active child: [output truncated at 64 KiB]\n");
        }
        Ok(text)
    }

    async fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let child = self.child.as_mut().expect("child is present");
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
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        match self.diagnostics() {
            Ok(output) if !output.is_empty() => eprintln!("{output}"),
            Err(error) => eprintln!("active-active child: diagnostic read failed: {error}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn active_active_child_diagnostics_are_isolated_and_bounded() {
    const MODE: &str = "AWAKEN_ACTIVE_ACTIVE_DIAGNOSTIC_CHILD";
    const BYTES: usize = 128 * 1024;
    if std::env::var(MODE).as_deref() == Ok("1") {
        // Raw writes bypass libtest capture, as inherited child output did.
        std::io::stdout()
            .write_all(b"test active_active_control_child ... ok\n")
            .unwrap();
        std::io::stderr()
            .write_all(b"child stderr retained\n")
            .unwrap();
        std::io::stdout().write_all(&vec![b'O'; BYTES]).unwrap();
        std::io::stderr().write_all(&vec![b'E'; BYTES]).unwrap();
        return;
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "active_active_child_diagnostics_are_isolated_and_bounded",
            "--nocapture",
        ])
        .env(MODE, "1");
    let mut child = ChildGuard::spawn_command(&mut command);
    assert!(child.wait_for_exit().await.success());
    assert!(child.output.as_file().metadata().unwrap().len() >= (2 * BYTES) as u64);
    let output = child.diagnostics().unwrap();
    assert!(output.contains("active-active child: test active_active_control_child ... ok"));
    assert!(output.contains("active-active child: child stderr retained"));
    assert!(output.contains("[output truncated at 64 KiB]"));
    assert!(output.len() < 66 * 1024);
    assert!(
        output
            .lines()
            .all(|line| line.starts_with("active-active child: "))
    );
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
                .child
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
                max_steps: Some(2),
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
async fn lost_first_claim_response_replays_through_a_different_coordinator() {
    lost_claim_response(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_directed_claim_response_replays_through_a_different_coordinator() {
    lost_claim_response(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_session_retires_receipts_with_an_independent_dispatch_backend() {
    lost_claim_response(true, true).await;
}

async fn lost_claim_response(directed: bool, independent: bool) {
    let schema = format!(
        "t_worker_claim_response_loss_{}_{directed}_{independent}",
        std::process::id()
    );
    let Some((pool, url)) = schema_pool(&schema).await else {
        return;
    };
    let scoped_url = database_url_in_schema(&url, &schema);
    let port_a = reserve_port().await;
    let port_b = reserve_port().await;
    let claim_path = "/v1/worker/dispatch/claim_for_request";
    let mut control_a =
        ChildGuard::spawn_with_response_fault(port_a, &scoped_url, Some(claim_path));
    wait_for_port(port_a, &mut control_a).await;
    let mut control_b = ChildGuard::spawn(port_b, &scoped_url, false);
    wait_for_port(port_b, &mut control_b).await;
    let base_a = format!("http://127.0.0.1:{port_a}");
    let (identity, _) = configured_worker();
    let run_id = RunId("claim-response-lost".into());
    let thread_id = ThreadId("claim-response-lost-thread".into());
    let mut dispatch = RunDispatch::new(activation(&run_id, &thread_id)).with_execution_scope(
        awaken_agent_contract::ExecutionScopeRef(awaken_agent_contract::ScopeId(
            "claim-scope".into(),
        )),
    );
    dispatch.placement = awaken_dispatch::PlacementRequirements::remote_preferred();
    dispatch.placement.runtime_protocol_version = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    HttpDispatchQueue::new(&base_a, identity.clone())
        .enqueue(dispatch.with_worker_codec_requirements())
        .await
        .expect("enqueue before failure");

    let client = reqwest::Client::new();
    let mut request = serde_json::json!({
        "identity": identity,
        "request_id": uuid::Uuid::now_v7().to_string()
    });
    if directed {
        request["run_id"] = serde_json::json!(run_id);
    }
    let send_request = |port, request: &serde_json::Value| {
        client
            .post(format!("http://127.0.0.1:{port}{claim_path}"))
            .header("x-awaken-worker-id", WORKER_ID)
            .json(request)
            .send()
    };
    let send = |port| send_request(port, &request);
    assert!(
        send(port_a).await.is_err(),
        "first HTTP receipt must be lost"
    );
    assert_eq!(control_a.wait_for_exit().await.code(), Some(86));

    let decoy = RunId("claim-response-decoy".into());
    let mut other = RunDispatch::new(activation(&decoy, &ThreadId("other-thread".into())));
    other.placement = awaken_dispatch::PlacementRequirements::remote_preferred();
    other.placement.runtime_protocol_version = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    HttpDispatchQueue::new(format!("http://127.0.0.1:{port_b}"), identity.clone())
        .enqueue(other.with_worker_codec_requirements())
        .await
        .unwrap();
    let persisted = || {
        sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT lease_owner, lease_epoch, attempt_count FROM runtime_dispatch WHERE run_id = $1",
        )
        .bind(&run_id.0)
        .fetch_one(&pool)
    };
    let original = persisted().await.expect("first claim is durably visible");
    assert_eq!(original.0, identity.lease_owner());
    assert!(
        original.1 > 0,
        "the first acquisition issued a nonzero epoch"
    );

    let body: serde_json::Value = send(port_b)
        .await
        .expect("retry via peer")
        .error_for_status()
        .expect("peer accepted the same logical request")
        .json()
        .await
        .expect("decode peer reply");
    let recovered: awaken_dispatch::Claimed = serde_json::from_value(body["claimed"].clone())
        .expect("retry must recover the first claim receipt");
    assert_eq!(recovered.request.run_id(), &run_id);
    assert_eq!(recovered.lease.epoch(), u64::try_from(original.1).unwrap());
    assert_eq!(
        persisted().await.unwrap(),
        original,
        "replay never reacquires"
    );

    let conflict = client
        .post(format!("http://127.0.0.1:{port_b}{claim_path}"))
        .header("x-awaken-worker-id", WORKER_ID)
        .json(&serde_json::json!({
            "identity": identity, "request_id": request["request_id"], "run_id": "different-run"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    let decoy_owner: Option<String> =
        sqlx::query_scalar("SELECT lease_owner FROM runtime_dispatch WHERE run_id = $1")
            .bind(&decoy.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        decoy_owner, None,
        "retry cannot acquire a different pending Run"
    );

    sqlx::query("UPDATE runtime_dispatch SET lease_until = 1 WHERE run_id = $1")
        .bind(&run_id.0)
        .execute(&pool)
        .await
        .unwrap();
    let stale: serde_json::Value = send(port_b)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        stale["claimed"].is_null(),
        "the original request cannot reacquire after expiry"
    );
    assert_eq!(persisted().await.unwrap(), original);

    let mut next_request = request.clone();
    next_request["request_id"] = serde_json::json!(uuid::Uuid::now_v7().to_string());
    let next: serde_json::Value = send_request(port_b, &next_request)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let next: awaken_dispatch::Claimed = serde_json::from_value(next["claimed"].clone())
        .expect("a new request can recover the expired Run");
    assert_eq!(next.request.run_id(), &run_id);
    assert!(next.lease.epoch() > recovered.lease.epoch());
    let stale: serde_json::Value = send(port_b)
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(stale["claimed"].is_null());

    if directed {
        // Keep the live row to check the delete fence independently of
        // terminal Dispatch settlement.
        delete_session_for_claim_test(&pool, &thread_id, independent).await;
        let payloads: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM runtime_dispatch_claim_request WHERE run_id = $1 AND claimed IS NOT NULL",
        )
        .bind(&run_id.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            payloads, 0,
            "Session deletion must remove claim receipt payloads"
        );
        let replay: serde_json::Value = send_request(port_b, &next_request)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            replay["claimed"].is_null(),
            "Session deletion fences a still-live receipt"
        );
    } else {
        // Seed a logically expired key, then exercise the real cleanup tick
        // and try a directed claim on a Run that can still be acquired.
        let old_id = "00000000-0000-7000-8000-000000000001";
        sqlx::query(
            "INSERT INTO runtime_dispatch_claim_request (lease_owner, request_id, expires_at_ms)
             VALUES ($1, $2, 900000)",
        )
        .bind(identity.lease_owner())
        .bind(old_id)
        .execute(&pool)
        .await
        .unwrap();
        let store = PostgresDispatchStore::with_pool(pool.clone())
            .await
            .unwrap();
        let mut expired = request.clone();
        expired["run_id"] = serde_json::json!(decoy);
        expired["request_id"] = serde_json::json!(old_id);
        for phase in ["before purge", "after purge"] {
            let replay: serde_json::Value = send_request(port_b, &expired)
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(replay["claimed"].is_null(), "expired request {phase}");
            if phase == "before purge" {
                assert_eq!(
                    awaken_dispatch_contract::Outbox::relay(&store)
                        .await
                        .unwrap(),
                    0
                );
                let keys: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM runtime_dispatch_claim_request WHERE request_id = $1",
                )
                .bind(old_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(keys, 0, "the existing relay tick physically purges the key");
            }
        }
        // Simulate a rollback of wall time relative to the time observed by a
        // prior sweep. This ID is young by the current clock and absent in the
        // ledger, but at the durable cutoff established by that sweep.
        let fresh = uuid::Uuid::now_v7();
        let (seconds, nanos) = fresh.get_timestamp().unwrap().to_unix();
        let expires_at =
            i64::try_from(seconds * 1000 + u64::from(nanos / 1_000_000)).unwrap() + 900000;
        sqlx::query("UPDATE runtime_dispatch_claim_retention SET expired_through_ms = $1")
            .bind(expires_at)
            .execute(&pool)
            .await
            .unwrap();
        expired["request_id"] = serde_json::json!(fresh.to_string());
        let fenced: serde_json::Value = send_request(port_b, &expired)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            fenced["claimed"].is_null(),
            "a remembered expiry survives clock rollback"
        );
        sqlx::query("UPDATE runtime_dispatch_claim_retention SET expired_through_ms = 0")
            .execute(&pool)
            .await
            .unwrap();
        let decoy_claim: serde_json::Value = send_request(port_b, &expired)
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            decoy_claim["claimed"]["lease"]["run_id"],
            serde_json::json!(decoy)
        );
    }
}

async fn delete_session_for_claim_test(pool: &PgPool, thread: &ThreadId, independent: bool) {
    use awaken_session_contract::{SessionMutationResult, SessionRepository as _};
    use awaken_store_conformance::session_store::{
        complete_terminal_cleanup, create_session, delete_mutation, replace_session, session,
    };

    const SCOPE: &str = "claim-scope";
    let session_pool = if independent {
        let schema = format!("t_worker_claim_session_only_{}", std::process::id());
        schema_pool(&schema).await.unwrap().0
    } else {
        pool.clone()
    };
    let repo = awaken_session_store::PostgresManagedSessionRepository::with_pool(session_pool)
        .await
        .unwrap();
    let mut root = create_session(
        &repo,
        SCOPE,
        session(&thread.0, "claim receipt"),
        Vec::new(),
    )
    .await;
    root.request_delete(SCOPE, 0).unwrap().unwrap();
    complete_terminal_cleanup(&mut root);
    let root = replace_session(&repo, SCOPE, root, "test:claim-terminal", Vec::new()).await;

    if independent {
        let dispatch = PostgresDispatchStore::with_pool(pool.clone())
            .await
            .unwrap();
        for scope in ["foreign-scope", SCOPE, SCOPE] {
            awaken_dispatch_contract::DispatchMaintenance::retire_deleted_session_dispatch(
                &dispatch, scope, &thread.0,
            )
            .await
            .unwrap();
            let members: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM runtime_dispatch_session_membership WHERE scope_id = $1 AND session_id = $2",
            )
            .bind(SCOPE)
            .bind(&thread.0)
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(members, i64::from(scope != SCOPE));
        }
    }
    let deletion = delete_mutation(&root, "test:claim-delete");
    assert!(matches!(
        repo.commit(SCOPE, deletion).await.unwrap(),
        SessionMutationResult::Applied { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_control_processes_retry_recover_and_settle_without_sticky_routing() {
    assert_committed_response_loss(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_commit_response_survives_claim_takeover() {
    assert_committed_response_loss(true).await;
}

async fn assert_committed_response_loss(takeover: bool) {
    let schema = format!("t_worker_active_active_{}_{takeover}", std::process::id());
    let Some((pool, url)) = schema_pool(&schema).await else {
        return;
    };
    let scoped_url = database_url_in_schema(&url, &schema);
    let port_a = reserve_port().await;
    let port_b = reserve_port().await;
    let mut control_a = ChildGuard::spawn(port_a, &scoped_url, true);
    wait_for_port(port_a, &mut control_a).await;
    let mut control_b = ChildGuard::spawn(port_b, &scoped_url, false);
    wait_for_port(port_b, &mut control_b).await;
    let base_a = format!("http://127.0.0.1:{port_a}");
    let base_b = format!("http://127.0.0.1:{port_b}");

    let (identity, directory) = configured_worker();

    // C0 both actual Control processes and the current client support these
    // codecs; freeze the EXACT registered manifest, not an invented offer.
    // Real PostgreSQL acceptance still requires its configured owned fixture.
    let manifest = directory
        .current(WORKER_ID)
        .await
        .unwrap()
        .unwrap()
        .snapshot
        .manifest;
    let queue_a =
        HttpDispatchQueue::new(&base_a, identity.clone()).with_worker_manifest(manifest.clone());
    let queue_b =
        HttpDispatchQueue::new(&base_b, identity.clone()).with_worker_manifest(manifest.clone());
    let run_id = RunId("active-active-run".into());
    let thread_id = ThreadId("active-active-thread".into());
    // P2 a newly authored remote dispatch and this current Worker must share
    // the same execution protocol. RunDispatch::new preserves the legacy v0
    // decoder posture; that is not the request this acceptance scenario tests.
    let mut dispatch = RunDispatch::new(activation(&run_id, &thread_id));
    dispatch.placement = awaken_dispatch::PlacementRequirements::remote_preferred();
    dispatch.placement.runtime_protocol_version = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let dispatch = dispatch.with_worker_codec_requirements();
    awaken_dispatch::can_claim(&manifest, &dispatch.placement)
        .expect("P1/P2 current fixture passes the authoritative compatibility kernel");
    queue_a
        .enqueue(dispatch)
        .await
        .expect("enqueue through Control A");
    let claimed = queue_b
        .claim(&identity.lease_owner(), 120_000, &Default::default())
        .await
        .expect("claim through Control B")
        .expect("shared dispatch is claimable");
    let mut claim = RunClaim::from(&claimed.lease);
    let LoadedRunRecovery::Full {
        snapshot: initial,
        prefix: Some(initial_prefix),
    } = queue_a
        .load_recovery(&claim, None, None)
        .await
        .expect("initial recovery through Control A")
    else {
        panic!("initial recovery must include a complete body and its version");
    };
    assert_eq!(initial.thread_version, 0);
    assert!(matches!(
        queue_b
            .load_recovery(&claim, None, Some(initial_prefix))
            .await
            .expect("the same prefix is valid through Control B"),
        LoadedRunRecovery::Reuse { reused } if reused.prefix == initial_prefix
    ));

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
        matches!(
            ambiguous,
            Err(awaken_agent_contract::thread::commit::coordinator::Error::OutcomeUnknown(_))
        ),
        "the Worker cannot determine the outcome after losing the response: {ambiguous:?}"
    );
    let exited = control_a.wait_for_exit().await;
    assert_eq!(exited.code(), Some(86), "fault injector exited Control A");

    let remote = RemoteClaimedRunCommit::new(&base_b, identity.clone());
    let mut command = command;
    if takeover {
        // Force the persisted deadline past its own clock. Leave the running
        // row intact so the production claim path must advance the epoch.
        let expired = sqlx::query(
            "UPDATE runtime_dispatch SET lease_until = 1 WHERE run_id = $1 AND lease_epoch = $2",
        )
        .bind(&run_id.0)
        .bind(i64::try_from(claim.epoch).unwrap())
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(expired.rows_affected(), 1);
        let successor = queue_b
            .claim(&identity.lease_owner(), 120_000, &Default::default())
            .await
            .expect("take over over HTTP")
            .expect("the expired logical claim is recoverable");
        assert!(successor.lease.epoch() > claim.epoch);
        assert!(remote.commit_operation(command.clone()).await.is_err());
        assert!(queue_b.load_recovery(&claim, None, None).await.is_err());
        claim = RunClaim::from(&successor.lease);
        command.claim = claim.clone();
    }
    let duplicate = remote
        .commit_operation(command)
        .await
        .expect("retry the same logical operation through Control B");
    assert!(
        duplicate.duplicate,
        "Control B returned the durable receipt"
    );
    let LoadedRunRecovery::Full {
        snapshot: recovered,
        prefix: Some(committed_prefix),
    } = queue_b
        .load_recovery(&claim, None, Some(initial_prefix))
        .await
        .expect("authoritative recovery through Control B")
    else {
        panic!("a peer commit invalidates the old cached prefix");
    };
    assert_eq!(recovered.thread_version, 1);
    assert_eq!(recovered.messages.len(), 1);
    assert_eq!(committed_prefix.last_commit, duplicate.commit_sequence);
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

#[test]
fn postgres_configuration_contract() {
    postgres_support::assert_configuration_contract(&[
        "two_control_processes_retry_recover_and_settle_without_sticky_routing",
        "lost_commit_response_survives_claim_takeover",
    ]);
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchSettlement as _,
};
