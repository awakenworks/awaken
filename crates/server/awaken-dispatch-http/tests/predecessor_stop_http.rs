//! Real HTTP + canonical Registry/Dispatch. Provider outcomes are deterministic
//! fixtures here; this suite does not claim Kubernetes or remote termination.
use std::sync::{Arc, Mutex};

use awaken_dispatch::{
    ApplicationError, AttemptAdmission, AttemptExecutionBinding, ClosedWorkerExecution,
    DispatchQueue, MemoryDispatchStore, RunClaim, RunDispatch, WorkerDirectory,
    WorkerExecutionStopSource, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
    WorkerProcessStopStatus, WorkerRegistration, WorkerState, stop_expired_predecessor,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_dispatch_testkit::worker_http::activation;
use awaken_worker_registry::{
    MemoryWorkerDirectory, PostgresWorkerDirectory, SqliteWorkerDirectory,
};
use awaken_worker_transport_security::{
    FixedWorkerLeasePolicy, HeaderWorkerAuthenticator, ManualWorkerClock,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

struct StopSource {
    directory: Arc<dyn WorkerDirectory>,
    calls: Mutex<Vec<AttemptExecutionBinding>>,
    result: Mutex<Result<Option<String>, String>>,
    replayed: Mutex<Option<WorkerProcessStopStatus>>,
    entered: Notify,
    release: Semaphore,
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for StopSource {
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
    store: Arc<MemoryDispatchStore>,
    directory: Arc<dyn WorkerDirectory>,
    stop: Arc<StopSource>,
    old: RunClaim,
    next: RunClaim,
    binding: AttemptExecutionBinding,
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tag = format!(
            "stop-http-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let old_worker = register(directory.as_ref(), &format!("{tag}-old"), 100, old_ttl).await;
        let worker = register(directory.as_ref(), &format!("{tag}-next"), 200, 10_000).await;
        let store = Arc::new(MemoryDispatchStore::new());
        let request = RunDispatch::new(activation("stop-predecessor"))
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required());
        store.enqueue(request).await.unwrap();
        let old_snapshot = directory
            .current(&old_worker.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        let old = RunClaim::from(
            &store
                .claim_compatible(&old_snapshot, 50, 100)
                .await
                .unwrap()
                .unwrap()
                .lease,
        );
        let binding =
            AttemptExecutionBinding::new(old_worker, "cluster", "frozen-process").unwrap();
        assert_eq!(
            store
                .begin_attempt(&old, 100, bound.then_some(&binding))
                .await
                .unwrap(),
            AttemptAdmission::Applied
        );
        let next_snapshot = directory
            .current(&worker.worker_id)
            .await
            .unwrap()
            .unwrap()
            .snapshot;
        let next = RunClaim::from(
            &store
                .claim_compatible(&next_snapshot, 10_000, 200)
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
        });
        let service = WorkerDispatchService::new(
            store.clone(),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualWorkerClock::new(200)),
            Arc::new(FixedWorkerLeasePolicy::new(budget)),
        )
        .with_worker_directory(directory.clone(), 10_000)
        .with_execution_stop_source(stop.clone());
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

    async fn assert_occupied(&self) {
        let slot = self
            .store
            .load_physical_attempt(&self.old.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot.claim, self.old);
        assert_eq!(
            self.store
                .begin_attempt(&self.next, 200, None)
                .await
                .unwrap(),
            AttemptAdmission::Blocked
        );
    }
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
        Arc::new(SqliteWorkerDirectory::open_in_memory().unwrap()),
    ];
    if let Ok(url) = std::env::var("AWAKEN_TEST_POSTGRES_URL") {
        directories.push(Arc::new(
            PostgresWorkerDirectory::connect(&url, 5).await.unwrap(),
        ));
        eprintln!("predecessor stop matrix: real PostgreSQL Registry enabled");
    }
    for directory in directories {
        let f = Fixture::new(directory, true, 50, 5_000).await;
        for result in [
            Ok(None),
            Ok(Some("provider-revision-7".into())),
            Err("lost provider reply".into()),
            Ok(Some("provider-revision-7".into())),
        ] {
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
        assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone(); 4]);
        // R14 simultaneous Coordinator observations close idempotently and both
        // target only the same frozen domain. Neither may release occupancy.
        f.stop.release.add_permits(2);
        let (a, b) = tokio::join!(f.begin(), f.begin());
        assert_eq!(a.1["admission"], "blocked");
        assert_eq!(b.1["admission"], "blocked");
        assert_eq!(*f.stop.calls.lock().unwrap(), vec![f.binding.clone(); 6]);
        f.assert_occupied().await;
        let _ = f.stop.entered.notified().await; // consume prior notification
        let (reply, ()) = tokio::join!(f.begin(), async {
            f.stop.entered.notified().await;
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                assert!(f.store.finish_attempt(&f.old).await.unwrap().applied());
                assert_eq!(
                    f.store.begin_attempt(&f.next, 200, None).await.unwrap(),
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
                snapshot.expires_at_ms = 10_000;
                successor = RunClaim::from(
                    &f.store
                        .claim_compatible(&snapshot, 1_000, 200)
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
            &second.next,
            200
        )
        .await
        .is_err()
    );
    first.assert_occupied().await;
    second.assert_occupied().await;
}
