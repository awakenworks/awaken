//! Two-process PostgreSQL acceptance for the registered remote Worker protocol.
//!
//! The parent test starts two copies of this test binary as independent Control
//! processes over one PostgreSQL schema. Requests deliberately alternate between
//! them. Control A exits after durably applying a claimed commit but before its
//! HTTP response reaches the Worker; Control B must return the durable duplicate
//! receipt, serve authoritative recovery, and settle the same claim.

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
const CHILD_EXIT_AFTER_COMMIT_ENV: &str = "AWAKEN_ACTIVE_ACTIVE_EXIT_AFTER_COMMIT";
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

struct ChildGuard {
    child: Option<Child>,
    output: tempfile::NamedTempFile,
}

impl ChildGuard {
    fn spawn(port: u16, database_url: &str, exit_after_commit: bool) -> Self {
        let mut command = Command::new(std::env::current_exe().expect("current test executable"));
        command
            .arg("--exact")
            .arg("active_active_control_child")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(CHILD_PORT_ENV, port.to_string())
            .env(CHILD_DATABASE_URL_ENV, database_url);
        if exit_after_commit {
            command.env(CHILD_EXIT_AFTER_COMMIT_ENV, "1");
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
async fn two_control_processes_retry_recover_and_settle_without_sticky_routing() {
    let schema = format!("t_worker_active_active_{}", std::process::id());
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
    let claim = RunClaim::from(&claimed.lease);
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
    ]);
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchEnqueue as _, DispatchSettlement as _,
};
