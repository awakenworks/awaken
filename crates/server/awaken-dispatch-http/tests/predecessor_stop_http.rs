// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Real HTTP + canonical Registry/Dispatch. Provider outcomes are deterministic
//! fixtures here; this suite does not claim Kubernetes or remote termination.
mod postgres_support;

use awaken_dispatch_contract::{ControlDispatchQueue, DispatchQueue};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::run::{Record as RunRecord, RunState};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_lease_testkit::ManualClock;
use awaken_dispatch::{
    ApplicationError, AttemptAdmission, AttemptExecutionBinding, AttemptStopStatus,
    ClosedAttemptExecution, ClosedWorkerExecution, MemoryDispatchStore, PhysicalAttempt,
    PostgresDispatchStore, RunClaim, RunDispatch, SqliteDispatchStore, WorkerDirectory,
    WorkerExecutionStopSource, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerProcessStopStatus, WorkerRegistration, WorkerState,
    has_active_physical_attempt_for_owner, stop_expired_predecessor,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_dispatch_testkit::worker_http::activation;
use awaken_dispatch_testkit::{AuthoritativeWallClock, ConformanceClock};
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};
use awaken_worker_transport_security::{FixedWorkerLeasePolicy, HeaderWorkerAuthenticator};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

struct StopSource {
    directory: Arc<dyn WorkerDirectory>,
    calls: Mutex<Vec<AttemptExecutionBinding>>,
    result: Mutex<Result<Option<String>, String>>,
    replayed: Mutex<Option<WorkerProcessStopStatus>>,
    entered: Notify,
    release: Semaphore,
    attempt: Mutex<Option<Arc<AttemptStopProbe>>>,
    evidence_release_calls: Mutex<Vec<AttemptExecutionBinding>>,
    evidence_release_result: Mutex<Result<(), String>>,
    idle_release_calls: Mutex<Vec<WorkerIdentity>>,
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for StopSource {
    async fn release_closed_idle_worker_evidence(
        &self,
        identity: &WorkerIdentity,
    ) -> Result<(), ApplicationError> {
        self.idle_release_calls
            .lock()
            .unwrap()
            .push(identity.clone());
        Ok(())
    }
    async fn stop_closed_worker(
        &self,
        command: &ClosedWorkerExecution,
    ) -> Result<WorkerProcessStopStatus, ApplicationError> {
        let binding = command.binding();
        let record = self
            .directory
            .current(&binding.worker().worker_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.snapshot.identity, *binding.worker());
        assert_eq!(
            record.snapshot.state,
            WorkerState::Dead,
            "closure precedes IO"
        );
        self.calls.lock().unwrap().push(binding.clone());
        self.entered.notify_one();
        self.release.acquire().await.unwrap().forget();
        if let Some(replayed) = self.replayed.lock().unwrap().clone() {
            return Ok(replayed);
        }
        match self
            .result
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)?
        {
            None => Ok(WorkerProcessStopStatus::Unknown),
            Some(reference) => command.observed_exit(reference),
        }
    }

    async fn release_closed_worker_evidence(
        &self,
        command: &ClosedWorkerExecution,
    ) -> Result<(), ApplicationError> {
        let binding = command.binding();
        let record = self
            .directory
            .incarnation(binding.worker())
            .await
            .unwrap()
            .unwrap();
        let receipt = record
            .process_exit
            .expect("process exit must be durable before evidence release");
        assert_eq!(receipt.provider_id(), binding.provider_id());
        assert_eq!(receipt.execution_ref(), binding.execution_ref());
        self.evidence_release_calls
            .lock()
            .unwrap()
            .push(binding.clone());
        self.evidence_release_result
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)
    }

    async fn close_attempt_execution(
        &self,
        command: &ClosedAttemptExecution,
        recovery: &RunRecoverySnapshot,
    ) -> Result<AttemptStopStatus, ApplicationError> {
        let probe = self.attempt.lock().unwrap().clone();
        let Some(probe) = probe else {
            return Ok(AttemptStopStatus::Unknown);
        };
        assert!(command.attempt().closing);
        if let Some(exit) = command.process_exit() {
            assert_eq!(command.attempt().binding.as_ref(), Some(exit.binding()));
            assert!(!exit.evidence_ref().is_empty());
        }
        assert!(!command.local_evidence_ref().is_empty());
        assert_eq!(recovery.claimed_run_id, command.attempt().claim.run_id);
        assert_eq!(
            Some(&recovery.thread_id),
            command.attempt().thread_id.as_ref()
        );
        probe.calls.lock().unwrap().push(command.attempt().clone());
        probe.entered.add_permits(1);
        probe.release.acquire().await.unwrap().forget();
        if let Some(replayed) = probe.replayed.lock().unwrap().clone() {
            return Ok(replayed);
        }
        let result = probe
            .result
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)?;
        let observed = match result {
            None => AttemptStopStatus::Unknown,
            Some(reference) => command.observed_all_execution_stopped(reference)?,
        };
        *probe.last.lock().unwrap() = Some(observed.clone());
        Ok(observed)
    }
}

struct StaticRecoverySource {
    snapshot: RunRecoverySnapshot,
}

#[async_trait::async_trait]
impl RunRecoverySource for StaticRecoverySource {
    async fn recovery_snapshot(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        claimed_run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        if thread_id != &self.snapshot.thread_id || claimed_run_id != &self.snapshot.claimed_run_id
        {
            return Err(RecoveryError::Rejected(
                "requested recovery subject does not match fixture".into(),
            ));
        }
        Ok(self.snapshot.clone())
    }
}

/// Deterministic TRUSTED provider fixture, not a production participant inventory
/// or proof that an actual remote model stopped. Tests below qualify consumption,
/// ordering, HTTP retry and the real Dispatch transaction only.
struct AttemptStopProbe {
    calls: Mutex<Vec<PhysicalAttempt>>,
    result: Mutex<Result<Option<String>, String>>,
    replayed: Mutex<Option<AttemptStopStatus>>,
    last: Mutex<Option<AttemptStopStatus>>,
    entered: Semaphore,
    release: Semaphore,
}

impl AttemptStopProbe {
    fn install(fixture: &Fixture) -> Arc<Self> {
        let probe = Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(None)),
            replayed: Mutex::new(None),
            last: Mutex::new(None),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        *fixture.stop.attempt.lock().unwrap() = Some(probe.clone());
        probe
    }
}

async fn register(
    directory: &dyn WorkerDirectory,
    name: &str,
    now: u64,
    ttl: u64,
) -> WorkerIdentity {
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let identity = directory
        .register(
            WorkerRegistration {
                worker_id: name.into(),
                incarnation_id: format!("{name}-boot"),
                manifest,
            },
            now,
            ttl,
        )
        .await
        .unwrap()
        .snapshot
        .identity;
    directory
        .heartbeat(
            &identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
            now,
            ttl,
        )
        .await
        .unwrap();
    identity
}

struct Fixture {
    store: Arc<dyn ControlDispatchQueue>,
    directory: Arc<dyn WorkerDirectory>,
    stop: Arc<StopSource>,
    old: RunClaim,
    next: RunClaim,
    binding: AttemptExecutionBinding,
    recovery: Arc<dyn RunRecoverySource>,
    thread_id: awaken_agent_contract::agent::thread::Id,
    address: String,
    worker: WorkerIdentity,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl Fixture {
    async fn new(
        directory: Arc<dyn WorkerDirectory>,
        bound: bool,
        old_ttl: u64,
        budget: u64,
    ) -> Self {
        Self::with_store(
            Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
            directory,
            bound,
            old_ttl,
            budget,
        )
        .await
    }

    async fn with_store(
        store: Arc<dyn ControlDispatchQueue>,
        directory: Arc<dyn WorkerDirectory>,
        bound: bool,
        old_ttl: u64,
        budget: u64,
    ) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = format!(
            "stop-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let old_worker = register(directory.as_ref(), &format!("{tag}-old"), 100, old_ttl).await;
        let worker = register(directory.as_ref(), &format!("{tag}-next"), 200, 10_000).await;
        let request = RunDispatch::new(activation(&tag))
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required());
        let run_id = request.run_id().clone();
        store.enqueue(request).await.unwrap();
        let mut old_snapshot = directory
            .current(&old_worker.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        // R30 SQL Dispatch owns a wall clock, while this fixture's Registry and
        // HTTP clock deliberately control closure at 100/200. Placement input
        // is kept live on either clock; Registry expiry itself is tested through
        // the original, unmodified directory record. Advance the actual lease
        // through the public renewal path instead of assuming caller time can
        // expire PostgreSQL or mutating its tables directly.
        // R31 a shared DB may contain another test's pending/recoverable Run:
        // both claims must name this fixture's exact Run, never scan the queue.
        old_snapshot.expires_at_ms = u64::MAX;
        let old = RunClaim::from(
            &store
                .claim_run_compatible(&run_id, &old_snapshot, 60_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let binding =
            AttemptExecutionBinding::new(old_worker, "cluster", "frozen-process").unwrap();
        assert_eq!(
            store
                .begin_attempt(&old, bound.then_some(&binding))
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        assert_eq!(
            store.renew_lease(&old, 0).await.unwrap(),
            awaken_dispatch::LeaseRenewalOutcome::Current
        );
        AuthoritativeWallClock.advance_past(0).await;
        let mut next_snapshot = directory
            .current(&worker.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        next_snapshot.expires_at_ms = u64::MAX;
        let next = RunClaim::from(
            &store
                .claim_run_compatible(&run_id, &next_snapshot, 60_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let stop = Arc::new(StopSource {
            directory: directory.clone(),
            calls: Mutex::new(Vec::new()),
            result: Mutex::new(Ok(None)),
            replayed: Mutex::new(None),
            entered: Notify::new(),
            release: Semaphore::new(0),
            attempt: Mutex::new(None),
            evidence_release_calls: Mutex::new(Vec::new()),
            evidence_release_result: Mutex::new(Ok(())),
            idle_release_calls: Mutex::new(Vec::new()),
        });
        let thread_id = request_thread_id(&tag);
        let recovery: Arc<dyn RunRecoverySource> = Arc::new(StaticRecoverySource {
            snapshot: RunRecoverySnapshot {
                thread_id: thread_id.clone(),
                claimed_run_id: old.run_id.clone(),
                runs: vec![RunRecord {
                    id: old.run_id.clone(),
                    thread_id: thread_id.clone(),
                    state: RunState::Running,
                }],
                latest_run_id: None,
                messages: Vec::new(),
                message_commit_cursors: Vec::new(),
                commit_timestamps: Default::default(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 1,
                next_commit_ordinal: 1,
            },
        });
        let service = WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(200)),
            Arc::new(FixedWorkerLeasePolicy::new(budget)),
        )
        .with_worker_directory(directory.clone(), 10_000)
        .with_execution_stop_source(stop.clone())
        .with_recovery_source(recovery.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let router = dispatch_transport_router_with_service(Arc::new(service));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            store,
            directory,
            stop,
            old,
            next,
            binding,
            recovery,
            thread_id,
            address,
            worker,
            server,
        }
    }

    async fn begin(&self) -> (reqwest::StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/dispatch/attempt/begin", self.address))
            .header("x-awaken-worker-id", &self.worker.worker_id)
            .json(&json!({"identity": self.worker, "claim": self.next}))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn settle(&self) -> (reqwest::StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/dispatch/settle", self.address))
            .header("x-awaken-worker-id", &self.worker.worker_id)
            .json(&json!({
                "identity": self.worker,
                "run_id": self.next.run_id.0,
                "epoch": self.next.epoch,
                "outcome": "Done",
                "consumed": []
            }))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }

    async fn assert_occupied(&self) {
        let slot = self
            .store
            .load_physical_attempt(&self.old.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot.claim, self.old);
        assert_eq!(
            self.store.begin_attempt(&self.next, None).await.unwrap(),
            AttemptAdmission::Blocked
        );
    }
}

fn request_thread_id(tag: &str) -> awaken_agent_contract::agent::thread::Id {
    awaken_agent_contract::agent::thread::Id(format!("thread-{tag}"))
}

#[tokio::test]
async fn closed_worker_releases_idle_evidence_only_after_the_original_physical_ack() {
    // C1 exact authenticated incarnation; C2 Registry closure; C3 original
    // Dispatch physical slot occupied/acknowledged. E1 C1+C2+occupied -> retain
    // metadata and slot, no provider stop or forged exit. E2 C1+C2+ACK -> release
    // metadata through the existing source. R1 occupied; R2 original exact ACK;
    // R3 retry closed deregistration converges without changing the successor.
    // Shared observation decision table: R4 occupied exact owner -> true;
    // R5 occupied foreign owner -> false; R6 original ACK -> false. These
    // reads leave the physical slot and Registry unchanged. Store faults or
    // inconsistent active rows are errors, never false (R7, adapter contract).
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 100, 1_000).await;
    let identity = f.binding.worker().clone();
    let request = || {
        reqwest::Client::new()
            .post(format!("{}/v1/worker/deregister", f.address))
            .header("x-awaken-worker-id", &identity.worker_id)
            .json(&json!({"identity":identity}))
    };
    assert!(
        request().send().await.unwrap().status().is_success(),
        "R1 closure"
    );
    assert!(
        f.stop.idle_release_calls.lock().unwrap().is_empty(),
        "R1 occupied"
    );
    assert!(
        has_active_physical_attempt_for_owner(f.store.as_ref(), &identity.lease_owner())
            .await
            .unwrap(),
        "R4 exact occupied owner"
    );
    assert!(
        !has_active_physical_attempt_for_owner(f.store.as_ref(), "foreign:1:boot")
            .await
            .unwrap(),
        "R5 foreign owner"
    );
    assert_eq!(
        f.store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap()
            .claim,
        f.old,
        "R1 no retirement"
    );
    assert!(
        f.store.finish_attempt(&f.old).await.unwrap().applied(),
        "R2 original ACK"
    );
    assert!(
        !has_active_physical_attempt_for_owner(f.store.as_ref(), &identity.lease_owner())
            .await
            .unwrap(),
        "R6 original slot acknowledged"
    );
    assert!(
        request().send().await.unwrap().status().is_success(),
        "R3 retry"
    );
    assert_eq!(*f.stop.idle_release_calls.lock().unwrap(), [identity]);
    assert!(
        f.stop.calls.lock().unwrap().is_empty(),
        "no independent stop proof was invented"
    );
}

#[tokio::test]
async fn postgres_guarded_admission_uses_one_process_connection_and_rejects_foreign_pools() {
    // P1 Registry and Dispatch share max_connections=1 -> exact guarded slot
    // admission completes on the original transaction, no nested acquisition.
    // P2 same database but independent pool -> rejection and zero slot effects.
    // P3 exact closed incarnation -> Fenced and no physical slot. P4 Ready
    // snapshot expires while its guard is held -> fresh DB time fences entry.
    // P5 handoff reuses the same transaction and canonical slot reducer. P6
    // production AnyDispatchStore preserves the original Control capability
    // and transaction instead of falling back to the Worker rung. Adapter-level
    // real database proof complements the HTTP closure/cancellation cases;
    // absence of a configured URL is an explicit skip, never PG evidence.
    let Some(url) = postgres_support::database_url() else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let directory = PostgresWorkerDirectory::with_pool(pool.clone())
        .await
        .unwrap();
    let store = PostgresDispatchStore::with_pool(pool.clone())
        .await
        .unwrap();
    let composed = awaken_dispatch::with_postgres_dispatch_pool(pool.clone())
        .await
        .unwrap();
    let foreign = PostgresWorkerDirectory::connect_existing(&url, 1)
        .await
        .unwrap();
    for rule in [
        "shared", "composed", "handoff", "foreign", "closed", "expired",
    ] {
        let tag = format!("one-connection-{rule}-{}", std::process::id());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let identity = register(
            &directory,
            &tag,
            now,
            if rule == "expired" { 10_000 } else { 60_000 },
        )
        .await;
        let snapshot = directory.current(&tag).await.unwrap().unwrap().snapshot;
        let request = RunDispatch::new(activation(&tag))
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required());
        let run_id = request.run_id().clone();
        store.enqueue(request).await.unwrap();
        let claim = RunClaim::from(
            &store
                .claim_run_compatible(&run_id, &snapshot, 60_000)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let binding =
            AttemptExecutionBinding::new(identity.clone(), "cluster", "frozen-original").unwrap();
        if rule == "closed" {
            directory.deregister(&identity).await.unwrap();
        }
        let guard = if rule == "foreign" {
            foreign.lock_incarnation(&identity).await.unwrap().unwrap()
        } else {
            directory
                .lock_incarnation(&identity)
                .await
                .unwrap()
                .unwrap()
        };
        if rule == "expired" {
            AuthoritativeWallClock
                .advance_past(snapshot.expires_at_ms)
                .await;
        }
        let admission_store: &dyn ControlDispatchQueue = if rule == "composed" {
            &composed
        } else {
            &store
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            admission_store.admit_attempt_guarded(&claim, Some(&binding), guard, rule == "handoff"),
        )
        .await
        .expect("P1 no nested pool acquisition");
        match rule {
            "shared" | "composed" | "handoff" => {
                assert_eq!(outcome.unwrap(), AttemptAdmission::Applied);
                assert_eq!(
                    store
                        .load_physical_attempt(&run_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .binding,
                    Some(binding)
                );
                assert!(store.finish_attempt(&claim).await.unwrap().applied());
            }
            "foreign" => {
                assert!(outcome.is_err());
                assert!(
                    store
                        .load_physical_attempt(&run_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            "closed" | "expired" => {
                assert_eq!(outcome.unwrap(), AttemptAdmission::Fenced);
                assert!(
                    store
                        .load_physical_attempt(&run_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            _ => unreachable!(),
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn blocked_http_closes_exact_expired_worker_but_local_exit_never_clears_remote_obligations() {
    // Cause graph: current successor + frozen predecessor + exact expired
    // Registry => close => provider IO. IO result is local only => slot unchanged.
    // R1 accepted/unknown; R2 exit; R3 error/response loss; R4 same-command retry;
    // all retain the old slot. Cross Memory/SQLite/Postgres Registry, not a copied
    // kernel. The PostgreSQL CI gate sets the URL and cannot silently skip it.
    // R5 late observation after normal finish/new begin must leave successor intact.
    let mut directories: Vec<Arc<dyn WorkerDirectory>> = vec![
        Arc::new(MemoryWorkerDirectory::new()) as Arc<dyn WorkerDirectory>,
        Arc::new(SqliteWorkerDirectory::open_in_memory().await.unwrap()),
    ];
    if let Some(url) = postgres_support::database_url() {
        directories.push(Arc::new(
            postgres_support::connect(PostgresWorkerDirectory::connect(&url, 5)).await,
        ));
        eprintln!("predecessor stop matrix: real PostgreSQL Registry enabled");
    }
    for directory in directories {
        let f = Fixture::new(directory, true, 50, 5_000).await;
        for result in [Ok(None), Err("lost provider reply".into())] {
            *f.stop.result.lock().unwrap() = result.clone();
            f.stop.release.add_permits(1);
            let (status, body) = f.begin().await;
            assert_eq!(
                status,
                if result.is_ok() {
                    reqwest::StatusCode::OK
                } else {
                    reqwest::StatusCode::SERVICE_UNAVAILABLE
                }
            );
            if result.is_ok() {
                assert_eq!(body["admission"], "blocked");
            }
            f.assert_occupied().await;
        }
        assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone(); 2]);
        // R14 simultaneous Coordinator observations close idempotently and both
        // target only the same frozen domain. Neither may release occupancy.
        *f.stop.result.lock().unwrap() = Ok(None);
        f.stop.release.add_permits(2);
        let (a, b) = tokio::join!(f.begin(), f.begin());
        assert_eq!(a.1["admission"], "blocked");
        assert_eq!(b.1["admission"], "blocked");
        assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone(); 4]);
        f.assert_occupied().await;
        let _ = f.stop.entered.notified().await; // consume prior notification
        let (reply, ()) = tokio::join!(f.begin(), async {
            f.stop.entered.notified().await;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                assert!(f.store.finish_attempt(&f.old).await.unwrap().applied());
                assert_eq!(
                    f.store.begin_attempt(&f.next, None).await.unwrap(),
                    AttemptAdmission::Applied
                );
            })
            .await
            .unwrap(); // no Dispatch lock held over provider IO
            f.stop.release.add_permits(1);
        });
        assert_eq!(
            reply.1["admission"], "blocked",
            "R5 retry will reread authority"
        );
        assert_eq!(
            f.store
                .load_physical_attempt(&f.next.run_id)
                .await
                .unwrap()
                .unwrap()
                .claim,
            f.next
        );
    }
}

#[tokio::test]
async fn stop_admission_rejects_live_replaced_historical_and_noncurrent_subjects() {
    // R6 live Registry (even with expired Run lease); R7 historical binding;
    // R8 replaced incarnation; R9 invented/expired successor; R10 same Worker
    // physical owner; R11 absent slot => no provider IO, no slot mutation.
    // These conditions are independently varied from the positive R1 witness.
    for rule in [
        "live",
        "historical",
        "replaced",
        "noncurrent",
        "same-owner",
        "empty",
    ] {
        let f = Fixture::new(
            Arc::new(MemoryWorkerDirectory::new()),
            rule != "historical",
            if rule == "live" { 1_000 } else { 50 },
            5_000,
        )
        .await;
        let mut successor = f.next.clone();
        match rule {
            "replaced" => {
                let record = f
                    .directory
                    .current(&f.binding.worker().worker_id)
                    .await
                    .unwrap()
                    .unwrap();
                f.directory
                    .register(
                        WorkerRegistration {
                            worker_id: f.binding.worker().worker_id.clone(),
                            incarnation_id: "replacement-boot".into(),
                            manifest: record.snapshot.manifest,
                        },
                        200,
                        10_000,
                    )
                    .await
                    .unwrap();
            }
            "noncurrent" => successor.epoch += 1,
            "same-owner" => {
                f.store.relinquish_claim(&f.next).await.unwrap();
                let mut snapshot = f
                    .directory
                    .current(&f.binding.worker().worker_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .snapshot;
                // Placement liveness is read against the store's own clock, so
                // this snapshot stays live on every backend clock, exactly as the
                // fixture's two other snapshots already do.
                snapshot.expires_at_ms = u64::MAX;
                successor = RunClaim::from(
                    &f.store
                        .claim_compatible(&snapshot, 1_000)
                        .await
                        .unwrap()
                        .unwrap()
                        .lease,
                );
            }
            "empty" => {
                f.store.finish_attempt(&f.old).await.unwrap();
            }
            _ => {}
        }
        let before = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        assert_eq!(
            stop_expired_predecessor(
                f.store.as_ref(),
                f.directory.as_ref(),
                f.stop.as_ref(),
                Some(f.recovery.as_ref()),
                &successor,
                200
            )
            .await
            .unwrap(),
            None,
            "{rule}"
        );
        assert!(f.stop.calls.lock().unwrap().is_empty(), "{rule}");
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            before,
            "{rule}"
        );
    }
}

#[tokio::test]
async fn timed_out_stop_remains_retryable_without_releasing_physical_occupancy() {
    // R12 provider stalls beyond the request budget => bounded 503, Registry
    // stays closed, slot stays occupied. R13 independent retry queries same
    // frozen binding; timeout cannot be treated as provider exit or new launch.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 20).await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(2), f.begin())
        .await
        .unwrap();
    assert_eq!(first.0, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    f.assert_occupied().await;
    f.stop.release.add_permits(1);
    assert_eq!(f.begin().await.1["admission"], "blocked");
    assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone(); 2]);
}

#[tokio::test]
async fn durable_process_exit_survives_release_failure_and_worker_replacement() {
    // Crash/retry cause/effect graph: C1 exact local exit is observed; C2 its
    // Registry commit succeeds; C3 provider finalizer release succeeds/fails;
    // C4 the logical Worker slot is replaced before retry; C5 complete-attempt
    // evidence is available. Effects: E1 C2 always precedes release; E2 release
    // failure returns 503 and retains Dispatch; E3 retry reuses historical C2,
    // never calls stop again, and may release after C4; E4 only C3+C5 retires
    // the exact attempt. Rules D1=C1+C2+!C3=>E1+E2; D2=D1+C4+C3+C5=>E3+E4.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let probe = AttemptStopProbe::install(&f);
    *f.stop.result.lock().unwrap() = Ok(Some("local-exit-rv".into()));
    *f.stop.evidence_release_result.lock().unwrap() = Err("release reply lost".into());
    *probe.result.lock().unwrap() = Ok(Some("all-participants-terminal".into()));
    f.stop.release.add_permits(1);

    assert_eq!(
        f.begin().await.0,
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "D1"
    );
    f.assert_occupied().await;
    let old = f
        .directory
        .incarnation(f.binding.worker())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        old.process_exit.as_ref().unwrap().evidence_ref(),
        "local-exit-rv",
        "D1/E1"
    );
    assert!(
        probe.calls.lock().unwrap().is_empty(),
        "D1 release gates proof query"
    );

    f.directory
        .register(
            WorkerRegistration {
                worker_id: f.binding.worker().worker_id.clone(),
                incarnation_id: "replacement-after-exit".into(),
                manifest: old.snapshot.manifest,
            },
            201,
            10_000,
        )
        .await
        .unwrap();
    *f.stop.evidence_release_result.lock().unwrap() = Ok(());
    probe.release.add_permits(1);
    assert_eq!(f.begin().await.1["admission"], "blocked", "D2");
    assert_eq!(
        *f.stop.calls.lock().unwrap(),
        vec![f.binding.clone()],
        "D2/E3"
    );
    assert_eq!(
        *f.stop.evidence_release_calls.lock().unwrap(),
        vec![f.binding.clone(); 2],
        "D1/D2 release retry"
    );
    assert_eq!(
        f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
        None,
        "D2/E4"
    );
}

#[tokio::test]
async fn local_exit_receipt_keeps_its_subject_and_rejects_empty_provider_evidence() {
    // R15 valid local receipt binds the exact original process and nonempty
    // provider revision. R16 cached receipt for another closed domain => reject
    // with both occupied slots unchanged. R17 blank evidence cannot mint even
    // a local receipt. This tests the production consumer, not JSON validation.
    let first = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    *first.stop.result.lock().unwrap() = Ok(Some("revision-7".into()));
    first.stop.release.add_permits(1);
    let observed = stop_expired_predecessor(
        first.store.as_ref(),
        first.directory.as_ref(),
        first.stop.as_ref(),
        Some(first.recovery.as_ref()),
        &first.next,
        200,
    )
    .await
    .unwrap()
    .unwrap();
    let WorkerProcessStopStatus::Exited(receipt) = &observed else {
        panic!("R15")
    };
    assert_eq!(receipt.binding(), &first.binding);
    assert_eq!(receipt.evidence_ref(), "revision-7");
    let second = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    *second.stop.replayed.lock().unwrap() = Some(observed);
    second.stop.release.add_permits(1);
    assert!(
        stop_expired_predecessor(
            second.store.as_ref(),
            second.directory.as_ref(),
            second.stop.as_ref(),
            Some(second.recovery.as_ref()),
            &second.next,
            200
        )
        .await
        .is_err()
    );
    *second.stop.replayed.lock().unwrap() = None;
    *second.stop.result.lock().unwrap() = Ok(Some("  ".into()));
    second.stop.release.add_permits(1);
    assert!(
        stop_expired_predecessor(
            second.store.as_ref(),
            second.directory.as_ref(),
            second.stop.as_ref(),
            Some(second.recovery.as_ref()),
            &second.next,
            200
        )
        .await
        .is_err()
    );
    first.assert_occupied().await;
    second.assert_occupied().await;
}

async fn dispatch_stores() -> Vec<Arc<dyn ControlDispatchQueue>> {
    let mut stores: Vec<Arc<dyn ControlDispatchQueue>> = vec![
        Arc::new(MemoryDispatchStore::new(awaken_time::system_clock())),
        Arc::new(
            SqliteDispatchStore::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        ),
    ];
    if let Some(url) = postgres_support::database_url() {
        stores.push(Arc::new(
            postgres_support::connect(PostgresDispatchStore::connect(&url, 5)).await,
        ));
        eprintln!("complete stop matrix: real PostgreSQL Dispatch enabled");
    }
    stores
}

#[tokio::test]
async fn complete_stop_uses_exact_retirement_and_requires_fresh_successor_admission() {
    // Cause/effect graph: C1 independent local exit; C2 full provider coverage;
    // C3 nonempty evidence; C4 exact subject. C1 & C2 & C3 & C4 => retire OLD slot;
    // retirement != admission/commit/replay authority for either Worker.
    // R18 !C1 => no complete-provider IO; R19 !C2 => occupied; R20 provider error
    // => retryable 503 + occupied; R21 !C3 => 400 + occupied; R22 all => empty but
    // original HTTP reply still Blocked. R23 next ordinary begin => Applied;
    // old claim remains fenced. Cross real Memory/SQLite/configured PostgreSQL.
    for store in dispatch_stores().await {
        let f = Fixture::with_store(
            store,
            Arc::new(MemoryWorkerDirectory::new()),
            true,
            50,
            5_000,
        )
        .await;
        let probe = AttemptStopProbe::install(&f);
        f.stop.release.add_permits(1);
        let (status, body) = f.begin().await;
        assert_eq!(status, reqwest::StatusCode::OK, "R18: {body}");
        assert_eq!(body["admission"], "blocked", "R18: {body}");
        assert!(probe.calls.lock().unwrap().is_empty(), "R18");
        f.assert_occupied().await;

        *f.stop.result.lock().unwrap() = Ok(Some("local-exit".into()));
        for (result, expected_status) in [
            (Ok(None), reqwest::StatusCode::OK),
            (
                Err("lost termination query response".into()),
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
            ),
            (Ok(Some(" \t".into())), reqwest::StatusCode::BAD_REQUEST),
        ] {
            *probe.result.lock().unwrap() = result;
            f.stop.release.add_permits(1);
            probe.release.add_permits(1);
            assert_eq!(f.begin().await.0, expected_status);
            f.assert_occupied().await;
        }
        *probe.result.lock().unwrap() = Ok(Some("all-participants-terminal-v7".into()));
        f.stop.release.add_permits(1);
        probe.release.add_permits(1);
        let (status, body) = f.begin().await;
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "R22 unexpected response: {body}"
        );
        assert_eq!(body["admission"], "blocked", "R22 is not admission");
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            None
        );
        assert!(!f.store.claim_is_current(&f.old).await.unwrap());
        assert_eq!(
            f.store
                .begin_attempt(&f.old, Some(&f.binding))
                .await
                .unwrap(),
            AttemptAdmission::Fenced
        );
        assert_eq!(
            f.begin().await.1["admission"],
            "applied",
            "R23 retries the original path"
        );
        assert_eq!(
            f.store
                .load_physical_attempt(&f.next.run_id)
                .await
                .unwrap()
                .unwrap()
                .claim,
            f.next
        );
        assert_eq!(
            *probe.calls.lock().unwrap(),
            vec![
                PhysicalAttempt {
                    closing: true,
                    claim: f.old.clone(),
                    binding: Some(f.binding.clone()),
                    thread_id: Some(f.thread_id.clone()),
                    session_thread_id: None,
                };
                4
            ]
        );
        let receipt = probe.last.lock().unwrap().clone().unwrap();
        let AttemptStopStatus::Stopped(receipt) = receipt else {
            panic!("R22")
        };
        assert_eq!(receipt.evidence_ref(), "all-participants-terminal-v7");
        assert_eq!(receipt.attempt().claim, f.old);
    }
}

#[tokio::test]
async fn terminal_recovery_settlement_progresses_the_same_exact_predecessor_stop() {
    // Cause/effect decision table: C1 a successor owns the recovered terminal
    // claim; C2 the predecessor's exact bound slot remains; C3 independent local
    // and complete-attempt evidence both match. R1=C1+C2+C3 => the first settle
    // invokes the canonical stop application service, retires only the old slot,
    // and still reports unsettled. R2=R1+same command retry => the existing
    // guarded settle removes the row without entering model/tool execution.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let probe = AttemptStopProbe::install(&f);
    *f.stop.result.lock().unwrap() = Ok(Some("local-exit".into()));
    *probe.result.lock().unwrap() = Ok(Some("all-participants-terminal".into()));
    f.stop.release.add_permits(1);
    probe.release.add_permits(1);

    let (status, body) = f.settle().await;
    assert_eq!(status, reqwest::StatusCode::OK, "R1: {body}");
    assert_eq!(body["settled"], false, "R1 retries through ordinary settle");
    assert_eq!(
        f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
        None,
        "R1 exact predecessor retired"
    );
    assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone()], "R1");
    assert_eq!(
        *probe.calls.lock().unwrap(),
        vec![PhysicalAttempt {
            closing: true,
            claim: f.old.clone(),
            binding: Some(f.binding.clone()),
            thread_id: Some(f.thread_id.clone()),
            session_thread_id: None,
        }],
        "R1"
    );

    let (status, body) = f.settle().await;
    assert_eq!(status, reqwest::StatusCode::OK, "R2: {body}");
    assert_eq!(body["settled"], true, "R2");
    assert!(f.store.list_dispatches().await.unwrap().is_empty(), "R2");
}

#[tokio::test]
async fn complete_stop_timeout_retains_occupancy_and_retries_the_same_subject() {
    // R24 local already exited, complete-provider reply stalls => existing HTTP
    // budget expires, not a stop receipt. R25 repeat same query with positive
    // evidence => retire once. Models lost response after provider completion;
    // no new remote operation or execution may be started by the observation.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 20).await;
    let probe = AttemptStopProbe::install(&f);
    *f.stop.result.lock().unwrap() = Ok(Some("local-exit".into()));
    *probe.result.lock().unwrap() = Ok(Some("terminal-evidence".into()));
    f.stop.release.add_permits(2);
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), f.begin())
        .await
        .unwrap();
    assert_eq!(response.0, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    f.assert_occupied().await;
    probe.release.add_permits(1);
    assert_eq!(f.begin().await.1["admission"], "blocked");
    assert_eq!(
        f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
        None
    );
    assert_eq!(
        *probe.calls.lock().unwrap(),
        vec![
            PhysicalAttempt {
                closing: true,
                claim: f.old.clone(),
                binding: Some(f.binding.clone()),
                thread_id: Some(f.thread_id.clone()),
                session_thread_id: None,
            };
            2
        ]
    );
}

#[tokio::test]
async fn concurrent_complete_receipts_cannot_clear_a_successor_slot() {
    // R26 two Coordinator requests independently reach the same complete source.
    // One response retires old, successor begins, then the other old receipt
    // arrives. Its original finish_attempt must be fenced; no newer slot clears.
    // Deterministic barriers (not sleeps) put the successor between receipt
    // applications. Successful DB access while IO waits also proves no database
    // transaction is held across provider IO. Cross actual store adapters.
    for store in dispatch_stores().await {
        let f = Fixture::with_store(
            store,
            Arc::new(MemoryWorkerDirectory::new()),
            true,
            50,
            5_000,
        )
        .await;
        let probe = AttemptStopProbe::install(&f);
        *f.stop.result.lock().unwrap() = Ok(Some("local-exit".into()));
        *probe.result.lock().unwrap() = Ok(Some("all-terminal".into()));
        f.stop.release.add_permits(2);
        let replies = Semaphore::new(0);
        let start_second = Semaphore::new(0);
        let completed = async {
            let response = f.begin().await;
            replies.add_permits(1);
            response
        };
        let second = async {
            start_second.acquire().await.unwrap().forget();
            let response = f.begin().await;
            replies.add_permits(1);
            response
        };
        let race = async {
            // The second request starts after the first committed closing.
            // Concurrent pre-close observations may safely coalesce/return
            // Blocked instead of both entering provider IO; that is H1, not
            // a requirement that two independent reads always occur.
            probe.entered.acquire().await.unwrap().forget();
            start_second.add_permits(1);
            probe.entered.acquire().await.unwrap().forget();
            probe.release.add_permits(1);
            replies.acquire().await.unwrap().forget();
            assert_eq!(
                f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
                None
            );
            assert_eq!(
                f.store.begin_attempt(&f.next, None).await.unwrap(),
                AttemptAdmission::Applied
            );
            probe.release.add_permits(1);
        };
        let (a, b, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(completed, second, race)
        })
        .await
        .unwrap();
        assert_eq!(a.1["admission"], "blocked");
        assert_eq!(b.1["admission"], "blocked");
        assert_eq!(
            f.store
                .load_physical_attempt(&f.next.run_id)
                .await
                .unwrap()
                .unwrap()
                .claim,
            f.next
        );
        assert_eq!(probe.calls.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn cached_complete_receipt_for_another_attempt_is_rejected() {
    // R27 otherwise positive evidence is cached for another Run/Worker/binding:
    // reject before finish_attempt, preserving both stores' current occupants.
    // These are opaque typed receipts, not deserialized worker-provided JSON.
    let first = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let old = AttemptStopProbe::install(&first);
    *first.stop.result.lock().unwrap() = Ok(Some("local-first".into()));
    *old.result.lock().unwrap() = Ok(Some("all-first".into()));
    first.stop.release.add_permits(1);
    old.release.add_permits(1);
    assert_eq!(first.begin().await.1["admission"], "blocked");
    assert_eq!(first.begin().await.1["admission"], "applied");

    let second = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let probe = AttemptStopProbe::install(&second);
    *second.stop.result.lock().unwrap() = Ok(Some("local-second".into()));
    *probe.replayed.lock().unwrap() = old.last.lock().unwrap().clone();
    second.stop.release.add_permits(1);
    probe.release.add_permits(1);
    assert_eq!(
        second.begin().await.0,
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    second.assert_occupied().await;
    assert_eq!(
        first
            .store
            .load_physical_attempt(&first.next.run_id)
            .await
            .unwrap()
            .unwrap()
            .claim,
        first.next
    );
}

#[tokio::test]
async fn worker_json_cannot_supply_independent_termination_evidence() {
    // R29 authenticated Worker + otherwise valid request + invented full-stop
    // JSON => reject at every attempt verb, zero source calls and unchanged slot.
    // Provider selection is trusted server composition, never a wire parameter.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let probe = AttemptStopProbe::install(&f);
    for verb in ["begin", "handoff", "finish", "close"] {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/dispatch/attempt/{verb}", f.address))
            .header("x-awaken-worker-id", &f.worker.worker_id)
            .json(&json!({
                "identity": f.worker, "claim": f.next,
                "termination": { "stopped": true, "evidence_ref": "invented" },
            }))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_client_error(), "{verb}");
    }
    assert!(probe.calls.lock().unwrap().is_empty());
    assert!(f.stop.calls.lock().unwrap().is_empty());
    f.assert_occupied().await;
}

#[tokio::test]
async fn reported_local_entry_closure_recovers_without_stopping_a_healthy_worker() {
    // C1 signed registered Worker reports its exact old entry closed after
    // mutation-lease loss; C2 independent effect closure missing/present; C3
    // repeated close and late ordinary ACK. H1=C1+C3 => one retained closing
    // slot, ACK fenced. H2=!C2 => no retirement/no process kill. H3=C2 => exact
    // Coordinator retirement; unrelated Worker activity remains admitted.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    let old_worker = f.binding.worker().clone();
    f.directory
        .heartbeat(
            &old_worker,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
            },
            200,
            10_000,
        )
        .await
        .unwrap();
    let probe = AttemptStopProbe::install(&f);
    for _ in 0..2 {
        let reply = reqwest::Client::new()
            .post(format!("{}/v1/worker/dispatch/attempt/close", f.address))
            .header("x-awaken-worker-id", &old_worker.worker_id)
            .json(&json!({"identity": old_worker, "claim": f.old}))
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), reqwest::StatusCode::OK);
        assert_eq!(reply.json::<Value>().await.unwrap()["closed"], true);
    }
    assert_eq!(
        f.store.finish_attempt(&f.old).await.unwrap(),
        awaken_dispatch::SettleOutcome::Fenced
    );
    probe.release.add_permits(1);
    assert_eq!(f.begin().await.1["admission"], "blocked");
    f.assert_occupied().await;
    assert!(
        f.stop.calls.lock().unwrap().is_empty(),
        "H2 no whole-Worker stop"
    );
    *probe.result.lock().unwrap() = Ok(Some("contained-effects-closed".into()));
    probe.release.add_permits(1);
    assert_eq!(f.begin().await.1["admission"], "blocked");
    assert!(
        f.store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .is_none(),
        "H3"
    );
    assert_eq!(
        f.directory
            .incarnation(&old_worker)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .state,
        WorkerState::Ready
    );
}

#[test]
fn postgres_configuration_contract() {
    // C1 each adapter receives a silent PostgreSQL listener; C2 the child
    // deadline exceeds the shared connection deadline. R3=C1+C2 => E1 both
    // adapters report a bounded configured-PG failure before child termination.
    postgres_support::assert_configuration_contract(&[
        "blocked_http_closes_exact_expired_worker_but_local_exit_never_clears_remote_obligations",
        "complete_stop_uses_exact_retirement_and_requires_fresh_successor_admission",
    ]);
}
