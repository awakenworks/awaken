// @formal evidence sandbox.k8s_process_observation_requires_complete_exit
// @formal evidence session_execution.one_physical_attempt_per_thread
//! Real HTTP + canonical Registry/Dispatch. Provider outcomes are deterministic
//! fixtures here; this suite does not claim Kubernetes or remote termination.
mod postgres_support;

use awaken_session_execution_contract::ApplicationError;
use awaken_worker_contract::WorkerCurrentSource;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::run::{Record as RunRecord, RunState};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_coordinator_client::HttpDispatchQueue;
use awaken_dispatch::{
    AttemptAdmission, AttemptExecutionBinding, AttemptStopStatus, ClosedAttemptExecution,
    ClosedWorkerExecution, MemoryDispatchStore, PhysicalAttempt, PostgresDispatchStore, RunClaim,
    RunDispatch, SqliteDispatchStore, WorkerDirectory, WorkerExecutionStopSource, WorkerHeartbeat,
    WorkerIdentity, WorkerProcessExit, WorkerProcessStopStatus, WorkerRegistration, WorkerState,
    has_active_physical_attempt_for_owner, stop_committed_terminal_attempt,
    stop_expired_predecessor,
};
use awaken_dispatch_http::{WorkerDispatchService, dispatch_transport_router_with_service};
use awaken_dispatch_testkit::worker_http::{activation, current_dispatch_codec_manifest};
use awaken_dispatch_testkit::{AuthoritativeWallClock, ConformanceClock};
use awaken_lease_testkit::ManualClock;
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
    /// The provider proves the end by a newer instance, not an exact exit.
    superseded: Mutex<bool>,
    replayed: Mutex<Option<WorkerProcessStopStatus>>,
    entered: Notify,
    release: Semaphore,
    attempt: Mutex<Option<Arc<AttemptStopProbe>>>,
    evidence_release_calls: Mutex<Vec<AttemptExecutionBinding>>,
    evidence_release_result: Mutex<Result<(), String>>,
    idle_release_calls: Mutex<Vec<WorkerIdentity>>,
    idle_observed_exit: Mutex<Option<WorkerProcessExit>>,
    idle_observe_may_stop: Mutex<Vec<bool>>,
    admission_seal_calls: Mutex<Vec<WorkerIdentity>>,
    admission_seal_result: Mutex<Result<(), String>>,
    owns_evidence: Mutex<bool>,
}

#[async_trait::async_trait]
impl WorkerExecutionStopSource for StopSource {
    fn owns_worker_evidence(&self, _identity: &WorkerIdentity) -> bool {
        *self.owns_evidence.lock().unwrap()
    }
    async fn seal_closed_worker_admission(
        &self,
        identity: &WorkerIdentity,
        _create_ledger: Option<&awaken_dispatch::WorkerCreateLedger>,
    ) -> Result<(), ApplicationError> {
        self.admission_seal_calls
            .lock()
            .unwrap()
            .push(identity.clone());
        self.admission_seal_result
            .lock()
            .unwrap()
            .clone()
            .map_err(ApplicationError::unavailable)
    }
    async fn observe_closed_idle_worker(
        &self,
        _identity: &WorkerIdentity,
        may_stop_active: bool,
        _create_ledger: Option<&awaken_dispatch::WorkerCreateLedger>,
    ) -> Result<Option<WorkerProcessExit>, ApplicationError> {
        self.idle_observe_may_stop
            .lock()
            .unwrap()
            .push(may_stop_active);
        Ok(self.idle_observed_exit.lock().unwrap().clone())
    }
    async fn release_closed_idle_worker_evidence(
        &self,
        identity: &WorkerIdentity,
        _original_create_ledger: Option<&awaken_dispatch::WorkerCreateLedger>,
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
            Some(reference) if *self.superseded.lock().unwrap() => {
                command.observed_superseded(reference)
            }
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
    // C0 current foreground codec support is fixed; exact protocol/identity,
    // provider closure and epoch causes below retain their original domains.
    let mut manifest = current_dispatch_codec_manifest();
    manifest.runtime_protocol.min = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    manifest.runtime_protocol.max = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let identity = directory
        .register(
            WorkerRegistration {
                worker_id: name.into(),
                incarnation_id: format!("{name}-boot"),
                manifest,
                create_ledger: None,
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
                source_retention: Default::default(),
            },
            now,
            ttl,
        )
        .await
        .unwrap();
    identity
}

struct Fixture {
    store: Arc<dyn WorkerDispatchAuthority>,
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
        store: Arc<dyn WorkerDispatchAuthority>,
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
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required())
            .with_worker_codec_requirements();
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
            superseded: Mutex::new(false),
            replayed: Mutex::new(None),
            entered: Notify::new(),
            release: Semaphore::new(0),
            attempt: Mutex::new(None),
            evidence_release_calls: Mutex::new(Vec::new()),
            evidence_release_result: Mutex::new(Ok(())),
            idle_release_calls: Mutex::new(Vec::new()),
            idle_observed_exit: Mutex::new(None),
            idle_observe_may_stop: Mutex::new(Vec::new()),
            admission_seal_calls: Mutex::new(Vec::new()),
            admission_seal_result: Mutex::new(Ok(())),
            owns_evidence: Mutex::new(false),
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

    async fn relinquish(
        &self,
        claim: &RunClaim,
        retry_after_ms: Option<u64>,
        attempt_failed: bool,
    ) -> (reqwest::StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/dispatch/relinquish", self.address))
            .header("x-awaken-worker-id", &self.worker.worker_id)
            .json(&json!({
                "identity": self.worker,
                "claim": claim,
                "retry_after_ms": retry_after_ms,
                "attempt_failed": attempt_failed
            }))
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

    async fn stop_terminal(
        &self,
        view: &dyn awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView,
        summary: &awaken_dispatch::DispatchSummary,
    ) -> Result<bool, ApplicationError> {
        stop_committed_terminal_attempt(
            self.store.as_ref(),
            Some((self.directory.as_ref(), self.stop.as_ref())),
            Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
                self.recovery.as_ref(),
            )),
            view,
            summary,
            200,
        )
        .await
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
async fn resolution_backoff_observes_predecessor_without_reaching_physical_begin() {
    // Cause/effect graph: C1 exact live successor claim; C2 failed resolution
    // requests backoff; C3 predecessor Worker expired; C4 local process exit;
    // C5 complete original execution closure. E1 observe through the canonical
    // stop application before releasing C1; E2 return only the mutation claim;
    // E3 retire the old physical slot iff C4+C5. No begin/handoff is invoked:
    // a Session Sandbox resolution failure never reaches those endpoints.
    // Exercise the actual HttpDispatchQueue defer call used by the Pool,
    // including its retry-hint serialization, rather than a test-only sender.
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effects |
    // | B1   | Y  | Y  | Y  | N  | N  | E1+E2, retain original open slot |
    // | B2   | Y  | Y  | Y  | Y  | N  | E1+E2, retain original closing slot |
    // | B3   | Y  | Y  | Y  | Y  | Y  | E1+E2+E3, no successor execution |
    // The existing Memory/SQLite/PostgreSQL adapter selection owns persistence
    // coverage. These deterministic trusted-provider fixtures prove HTTP
    // ordering and receipt consumption, not actual Kubernetes termination.
    for store in dispatch_stores().await {
        for (local_exit, scope_closed) in [(false, false), (true, false), (true, true)] {
            let f = Fixture::with_store(
                store.clone(),
                Arc::new(MemoryWorkerDirectory::new()),
                true,
                50,
                5_000,
            )
            .await;
            let probe = AttemptStopProbe::install(&f);
            if local_exit {
                *f.stop.result.lock().unwrap() = Ok(Some("original-process-exit".into()));
            }
            if scope_closed {
                *probe.result.lock().unwrap() = Ok(Some("complete-original-scope".into()));
            }
            f.stop.release.add_permits(1);
            probe.release.add_permits(1);
            let queue = HttpDispatchQueue::new(f.address.clone(), f.worker.clone());
            assert!(queue.defer_claim(&f.next, 1_000).await.unwrap().applied());
            assert!(!f.store.claim_is_current(&f.next).await.unwrap(), "E2");
            assert_eq!(
                f.stop.calls.lock().unwrap().as_slice(),
                std::slice::from_ref(&f.binding),
                "E1"
            );
            let slot = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
            if scope_closed {
                assert!(slot.is_none(), "B3 exact complete closure");
            } else {
                let slot = slot.expect("B1/B2 retain physical occupancy");
                assert_eq!(slot.claim, f.old);
                assert_eq!(slot.closing, local_exit);
            }
            assert_eq!(probe.calls.lock().unwrap().len(), usize::from(local_exit));
        }
    }
}

#[tokio::test]
async fn resolution_backoff_returns_mutation_claim_even_if_stop_observation_fails() {
    // Extend B1: C6 provider query fails or exceeds the owned request budget;
    // C7 release is defer/retry. B4=C6(error)+C7(defer), B5=C6(timeout)+C7(retry):
    // return 503 visibly, still release the exact mutation claim, retain the
    // original physical slot. B5's failed release keeps its recovery count: the
    // next recovery claim spends that attempt, so release must not count it
    // twice. B4's deferral is a recovery re-claim blocked behind the occupied
    // slot: it refunds the attempt its claim spent (bounded rule B1 of
    // `blocked_reclaim_attempt_count`; formerly it kept the spent attempt, which
    // burned the budget while the predecessor's closure was still progressing).
    // No permit in B5 forces an actually blocked provider Future; timeout is
    // never interpreted as a positive stop or complete-scope receipt.
    for timed_out in [false, true] {
        let f = Fixture::new(
            Arc::new(MemoryWorkerDirectory::new()),
            true,
            50,
            if timed_out { 100 } else { 5_000 },
        )
        .await;
        let before = f
            .store
            .dispatch_summary(&f.next.run_id)
            .await
            .unwrap()
            .unwrap();
        if !timed_out {
            *f.stop.result.lock().unwrap() = Err("provider stop query failed".into());
            f.stop.release.add_permits(1);
        }
        let (status, body) = f.relinquish(&f.next, Some(1_000), timed_out).await;
        assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert!(body.to_string().contains(if timed_out {
            "predecessor stop observation timed out"
        } else {
            "provider stop query failed"
        }));
        assert!(
            !f.store.claim_is_current(&f.next).await.unwrap(),
            "B4/B5 release"
        );
        assert_eq!(
            f.stop.calls.lock().unwrap().as_slice(),
            std::slice::from_ref(&f.binding)
        );
        let slot = f
            .store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(slot.claim, f.old);
        assert!(!slot.closing, "query failure is not exit evidence");
        let after = f
            .store
            .dispatch_summary(&f.next.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(before.attempt_count, 1, "the recovery claim spent one");
        assert_eq!(
            after.attempt_count,
            before.attempt_count - u64::from(!timed_out),
            "B4 refunds the blocked re-claim; B5 keeps it"
        );
    }
}

#[tokio::test]
async fn resolution_backoff_reuses_existing_authority_and_no_stop_boundaries() {
    // Causes: C1 authenticated claim owner; C2 current claim; C3 retry hint;
    // C4 failed attempt; C5 expired predecessor; C6 original binding/slot.
    // | Rule | Boundary                         | Effect |
    // | B6   | !C1                              | 400, claim unchanged, no I/O |
    // | B7   | !C2                              | false, no I/O |
    // | B8   | !C3 + !C4                        | ordinary release, no I/O |
    // | B9   | !C3 + C4                         | 400, claim unchanged, no I/O |
    // | B10  | !C5                              | release, no predecessor stop |
    // | B11  | missing binding                  | release, occupied, no I/O |
    // | B12  | absent slot                      | release, no I/O |
    // | B13  | C1+C2+C3+C4+C5+C6, Unknown stop | recovery count unchanged, occupied |
    // | B14  | failed release with absent slot | spend one through existing reducer |
    // An applied unfailed release behind the still-occupied predecessor slot
    // (ordinary, live, unbound) refunds the attempt its recovery claim spent
    // (rule B1 of `blocked_reclaim_attempt_count`); before that rule it kept
    // the spent attempt and burned retry budget while closure progressed.
    // These are decision-table partitions, not the Cartesian product: B1-B5
    // own the interactions once provider observation is authorized. The
    // existing durable-handoff tests retain their execution path unchanged.
    for rule in [
        "foreign",
        "stale",
        "ordinary",
        "invalid",
        "live",
        "unbound",
        "vacant",
        "retry",
        "retry-vacant",
    ] {
        let f = Fixture::new(
            Arc::new(MemoryWorkerDirectory::new()),
            rule != "unbound",
            if rule == "live" { 10_000 } else { 50 },
            5_000,
        )
        .await;
        let mut claim = f.next.clone();
        if rule == "foreign" {
            claim.owner = "foreign-owner".into();
        }
        let replacement = if rule == "stale" {
            f.store.relinquish_claim(&f.next).await.unwrap();
            AuthoritativeWallClock.advance_past(0).await;
            let mut snapshot = f
                .directory
                .current(&f.worker.worker_id)
                .await
                .unwrap()
                .unwrap()
                .snapshot;
            snapshot.expires_at_ms = u64::MAX;
            Some(RunClaim::from(
                &f.store
                    .claim_run_compatible(&f.next.run_id, &snapshot, 60_000)
                    .await
                    .unwrap()
                    .unwrap()
                    .lease,
            ))
        } else {
            None
        };
        if matches!(rule, "vacant" | "retry-vacant") {
            f.store.finish_attempt(&f.old, None).await.unwrap();
        }
        let before = f
            .store
            .dispatch_summary(&f.next.run_id)
            .await
            .unwrap()
            .unwrap();
        let retry_after = (!matches!(rule, "ordinary" | "invalid")).then_some(1_000);
        let failed = matches!(rule, "invalid" | "retry" | "retry-vacant");
        f.stop.release.add_permits(1);
        let (status, body) = f.relinquish(&claim, retry_after, failed).await;
        let rejected = matches!(rule, "foreign" | "invalid");
        assert_eq!(
            status,
            if rejected {
                reqwest::StatusCode::BAD_REQUEST
            } else {
                reqwest::StatusCode::OK
            },
            "{rule}: {body}"
        );
        assert_eq!(
            f.store.claim_is_current(&f.next).await.unwrap(),
            rejected,
            "{rule}"
        );
        if !rejected {
            assert_eq!(body["relinquished"], rule != "stale", "{rule}");
        }
        if let Some(replacement) = &replacement {
            assert!(
                f.store.claim_is_current(replacement).await.unwrap(),
                "B7 retains replacement"
            );
        }
        assert_eq!(
            f.stop.calls.lock().unwrap().len(),
            usize::from(rule == "retry"),
            "{rule}"
        );
        let slot = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        assert_eq!(
            slot.is_some(),
            !matches!(rule, "vacant" | "retry-vacant"),
            "{rule}"
        );
        if let Some(slot) = slot {
            assert_eq!(slot.claim, f.old, "{rule}");
            assert!(!slot.closing, "{rule}");
        }
        let after = f
            .store
            .dispatch_summary(&f.next.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.attempt_count,
            before.attempt_count + u64::from(rule == "retry-vacant")
                - u64::from(matches!(rule, "ordinary" | "live" | "unbound")),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn resolution_backoff_cannot_release_a_newer_claim_after_provider_io() {
    // B15: C1 exact backoff request enters the trusted predecessor observation;
    // C2 its mutation lease expires and another claim is durably acquired while
    // provider I/O is blocked; C3 the original observation returns Unknown.
    // Effect: the old HTTP release is fenced, the replacement stays current,
    // and the original physical slot remains occupied. No database/Thread lock
    // may span provider I/O: the public reclaim must complete before C3.
    // The semaphore/Notify orders the race; wall-clock waits only advance the
    // existing real store's explicit zero-deadline renewal, never fake expiry.
    let f = Arc::new(Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await);
    let pending = tokio::spawn({
        let f = f.clone();
        async move { f.relinquish(&f.next, Some(1_000), false).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), f.stop.entered.notified())
        .await
        .expect("B15 stop observation entered");
    assert_eq!(
        f.store.renew_lease(&f.next, 0).await.unwrap(),
        awaken_dispatch::LeaseRenewalOutcome::Current
    );
    AuthoritativeWallClock.advance_past(0).await;
    let mut snapshot = f
        .directory
        .current(&f.worker.worker_id)
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    snapshot.expires_at_ms = u64::MAX;
    let replacement = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.store
            .claim_run_compatible(&f.next.run_id, &snapshot, 60_000),
    )
    .await
    .expect("B15 no provider I/O lock blocks claim")
    .unwrap()
    .unwrap();
    let replacement = RunClaim::from(&replacement.lease);
    assert!(replacement.epoch > f.next.epoch);
    f.stop.release.add_permits(1);
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), pending)
        .await
        .expect("B15 old HTTP request returns")
        .unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
    assert_eq!(body["relinquished"], false);
    assert!(f.store.claim_is_current(&replacement).await.unwrap());
    assert!(!f.store.claim_is_current(&f.next).await.unwrap());
    let slot = f
        .store
        .load_physical_attempt(&f.old.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(slot.claim, f.old);
    assert!(!slot.closing);
    assert_eq!(
        f.stop.calls.lock().unwrap().as_slice(),
        std::slice::from_ref(&f.binding)
    );
}

#[tokio::test]
async fn closed_worker_exit_is_observed_before_occupied_retention_policy() {
    // Cause/effect graph: C1 authenticated exact Worker is closed; C2 its
    // original physical slot remains occupied; C3 provider independently
    // observes the exact exit. E1 persist/read back C3 despite C2; E2 retain
    // the slot and provider evidence; E3 do not authorize an active stop.
    // | Rule | C1 | C2 | C3 | Effect |
    // | O1   | Y  | Y  | Y  | E1+E2+E3 |
    // | O2   | Y  | Y  | N  | no exit + E2+E3 |
    // The existing idle/ACK test owns the empty-slot rules; ordinary attempt
    // tests own live-incarnation refusal and full-closure retirement. These
    // rules add no assertion that a local exit closes the complete attempt.
    for observed_exit in [true, false] {
        let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 100, 1_000).await;
        let identity = f.binding.worker().clone();
        if observed_exit {
            *f.stop.idle_observed_exit.lock().unwrap() = Some(
                WorkerProcessExit::new(
                    f.binding.provider_id(),
                    f.binding.execution_ref(),
                    "exact-natural-exit",
                )
                .unwrap(),
            );
        }
        let response = reqwest::Client::new()
            .post(format!("{}/v1/worker/deregister", f.address))
            .header("x-awaken-worker-id", &identity.worker_id)
            .json(&json!({"identity": identity}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let record = f.directory.incarnation(&identity).await.unwrap().unwrap();
        assert_eq!(record.snapshot.state, WorkerState::Dead);
        assert_eq!(
            record.process_exit.is_some(),
            observed_exit,
            "O1/O2 exit fact"
        );
        assert_eq!(*f.stop.idle_observe_may_stop.lock().unwrap(), [false], "E3");
        assert!(f.stop.idle_release_calls.lock().unwrap().is_empty(), "E2");
        assert!(
            f.stop.calls.lock().unwrap().is_empty(),
            "E3 no occupied stop"
        );
        f.assert_occupied().await;
    }
}

#[tokio::test]
async fn create_admission_and_worker_exit_observation_fail_independently() {
    // Independent-owner decision table: C1 exact Worker is closed with an
    // occupied physical slot; C2 original Create seal succeeds/fails; C3 exact
    // natural exit is observed/missing. F1 any C2+C3(observed) -> persist exit;
    // F2 failed C2 -> error and no retention release, regardless of C3; F3
    // successful C2+missing C3 -> keep exit unknown and slot occupied. F4 replay
    // with a retained exit -> retry C2 without observing or overwriting C3.
    // All rows retain the original physical slot and permit no active stop.
    for seal_fails in [false, true] {
        for observed_exit in [false, true] {
            let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 100, 1_000).await;
            let identity = f.binding.worker().clone();
            f.directory.deregister(&identity).await.unwrap();
            if seal_fails {
                *f.stop.admission_seal_result.lock().unwrap() = Err("fixture seal outage".into());
            }
            if observed_exit {
                *f.stop.idle_observed_exit.lock().unwrap() = Some(
                    WorkerProcessExit::new(
                        f.binding.provider_id(),
                        f.binding.execution_ref(),
                        "original-exit",
                    )
                    .unwrap(),
                );
            }
            let result = awaken_dispatch::release_closed_idle_worker_evidence(
                f.store.as_ref(),
                f.directory.as_ref(),
                f.stop.as_ref(),
                &identity,
                100,
            )
            .await;
            assert_eq!(result.is_err(), seal_fails, "F1/F2/F3");
            let record = f.directory.incarnation(&identity).await.unwrap().unwrap();
            assert_eq!(record.process_exit.is_some(), observed_exit, "F1/F3");
            assert_eq!(
                f.stop.admission_seal_calls.lock().unwrap().as_slice(),
                std::slice::from_ref(&identity)
            );
            assert_eq!(*f.stop.idle_observe_may_stop.lock().unwrap(), [false]);
            if observed_exit {
                *f.stop.admission_seal_result.lock().unwrap() = Ok(());
                awaken_dispatch::release_closed_idle_worker_evidence(
                    f.store.as_ref(),
                    f.directory.as_ref(),
                    f.stop.as_ref(),
                    &identity,
                    100,
                )
                .await
                .unwrap();
                assert_eq!(
                    *f.stop.admission_seal_calls.lock().unwrap(),
                    [identity.clone(), identity.clone()],
                    "F4 same owner retry"
                );
                assert_eq!(
                    *f.stop.idle_observe_may_stop.lock().unwrap(),
                    [false],
                    "F4 retained exit not reread"
                );
                assert_eq!(
                    f.directory
                        .incarnation(&identity)
                        .await
                        .unwrap()
                        .unwrap()
                        .process_exit,
                    record.process_exit,
                    "F4 original exit unchanged"
                );
            }
            assert!(f.stop.idle_release_calls.lock().unwrap().is_empty());
            assert!(f.stop.calls.lock().unwrap().is_empty());
            f.assert_occupied().await;
        }
    }
}

#[tokio::test]
async fn closed_worker_exit_survives_an_unavailable_physical_scan() {
    // O3 complements O1/O2: C1 exact closed Worker; C2 provider has its original
    // exit; C3 the real PostgreSQL physical reader is unavailable. E1 the
    // independent Registry fact survives C3; E2 release/active stop remain
    // refused; E3 reconnecting reads the unchanged original occupied slot.
    // Closing only the Dispatch pool keeps the independent Memory Registry
    // available and exercises a real adapter error without another reader mock.
    let Some(url) = postgres_support::database_url() else {
        return;
    };
    let pool = postgres_support::connect(sqlx::postgres::PgPoolOptions::new().connect(&url)).await;
    let store = Arc::new(
        PostgresDispatchStore::with_pool(pool.clone())
            .await
            .unwrap(),
    );
    let f = Fixture::with_store(
        store,
        Arc::new(MemoryWorkerDirectory::new()),
        true,
        100,
        1_000,
    )
    .await;
    let identity = f.binding.worker().clone();
    assert!(
        reqwest::Client::new()
            .post(format!("{}/v1/worker/deregister", f.address))
            .header("x-awaken-worker-id", &identity.worker_id)
            .json(&json!({"identity": identity}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    *f.stop.idle_observed_exit.lock().unwrap() = Some(
        WorkerProcessExit::new(
            f.binding.provider_id(),
            f.binding.execution_ref(),
            "exit-before-unavailable-scan",
        )
        .unwrap(),
    );
    f.stop.idle_observe_may_stop.lock().unwrap().clear();
    pool.close().await;
    assert!(
        awaken_dispatch::release_closed_idle_worker_evidence(
            f.store.as_ref(),
            f.directory.as_ref(),
            f.stop.as_ref(),
            &identity,
            200,
        )
        .await
        .is_err()
    );
    let record = f.directory.incarnation(&identity).await.unwrap().unwrap();
    assert!(record.process_exit.is_some(), "O3/E1");
    assert_eq!(
        *f.stop.idle_observe_may_stop.lock().unwrap(),
        [false],
        "O3/E2"
    );
    assert!(
        f.stop.idle_release_calls.lock().unwrap().is_empty(),
        "O3/E2"
    );
    let reread = postgres_support::connect(PostgresDispatchStore::connect(&url, 1)).await;
    assert_eq!(
        reread
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap()
            .claim,
        f.old,
        "O3/E3"
    );
}

#[tokio::test]
async fn closed_worker_releases_idle_evidence_only_after_the_original_physical_ack() {
    // C1 exact authenticated incarnation; C2 Registry closure; C3 original
    // Dispatch physical slot occupied/acknowledged; C4 provider has observed
    // the original Worker process exit. E1 C1+C2+occupied -> retain metadata
    // and slot, no provider stop or forged exit. E2 ACK without C4 still
    // retains evidence; E3 ACK+C4 persists the exit before releasing metadata.
    // R1 occupied; R2 original exact ACK; R3 retry without C4 -> E2;
    // R4 retry with C4 -> E3 without changing the successor.
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
        f.store
            .finish_attempt(&f.old, None)
            .await
            .unwrap()
            .applied(),
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
        "R3 retry before exit evidence"
    );
    assert!(
        f.stop.idle_release_calls.lock().unwrap().is_empty(),
        "R3 no physical exit"
    );
    *f.stop.idle_observed_exit.lock().unwrap() = Some(
        WorkerProcessExit::new(
            f.binding.provider_id(),
            f.binding.execution_ref(),
            "observed-idle-worker-exit",
        )
        .unwrap(),
    );
    assert!(
        request().send().await.unwrap().status().is_success(),
        "R4 retry after exact exit evidence"
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
            .with_placement(awaken_dispatch::PlacementRequirements::remote_required())
            .with_worker_codec_requirements();
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
            // The original max=1 pool is intentionally occupied by the guard.
            // Reuse the independent pool to observe the same DB authority,
            // preserving P4's expiry while the original guard remains held.
            postgres_support::postgres_clock(&foreign.pool())
                .advance_past(snapshot.expires_at_ms)
                .await;
        }
        let admission_store: &dyn WorkerDispatchAuthority = if rule == "composed" {
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
                assert!(store.finish_attempt(&claim, None).await.unwrap().applied());
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
                assert!(
                    f.store
                        .finish_attempt(&f.old, None)
                        .await
                        .unwrap()
                        .applied()
                );
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
                            create_ledger: None,
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
                f.store.finish_attempt(&f.old, None).await.unwrap();
            }
            _ => {}
        }
        let before = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        assert_eq!(
            stop_expired_predecessor(
                f.store.as_ref(),
                f.directory.as_ref(),
                f.stop.as_ref(),
                Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
                    f.recovery.as_ref()
                )),
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
                create_ledger: None,
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
async fn superseded_worker_closes_the_occupied_attempt_like_an_exact_exit() {
    // Dispatch consumer of a provider process end that is a supersession (a
    // newer instance of the bound process slot started; no exit code):
    // | Row | provider observation        | Registry receipt          | entry   | slot     |
    // | SU1 | Unknown (Pod gone / lost)   | none                      | open    | occupied |
    // | SU2 | Superseded, scope unknown   | Superseded, then released | closing | occupied |
    // | SU3 | retry, scope proven         | reused, no second stop    | closing | retired  |
    // SU2 is the exact-exit row with another receipt kind: the local process
    // fact closes the entry, and only complete-scope evidence retires the slot,
    // so the attempt's external effects stay Indeterminate until then.
    let directories: Vec<Arc<dyn WorkerDirectory>> = vec![
        Arc::new(MemoryWorkerDirectory::new()),
        Arc::new(SqliteWorkerDirectory::open_in_memory().await.unwrap()),
    ];
    for directory in directories {
        let f = Fixture::new(directory, true, 50, 5_000).await;
        *f.stop.result.lock().unwrap() = Ok(None);
        f.stop.release.add_permits(1);
        assert_eq!(f.begin().await.1["admission"], "blocked", "SU1");
        f.assert_occupied().await;
        let open = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        assert!(!open.as_ref().unwrap().closing, "SU1 entry stays open");
        assert!(
            f.directory
                .incarnation(f.binding.worker())
                .await
                .unwrap()
                .unwrap()
                .process_exit
                .is_none(),
            "SU1 no receipt"
        );

        *f.stop.superseded.lock().unwrap() = true;
        *f.stop.result.lock().unwrap() = Ok(Some("k8s-container-superseded:v1:fixture".into()));
        f.stop.release.add_permits(1);
        assert_eq!(f.begin().await.1["admission"], "blocked", "SU2");
        let receipt = f
            .directory
            .incarnation(f.binding.worker())
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .expect("SU2 durable receipt");
        assert_eq!(
            receipt.end(),
            awaken_worker_contract::WorkerProcessEnd::Superseded,
            "SU2 receipt kind"
        );
        assert_eq!(
            receipt.evidence_ref(),
            "k8s-container-superseded:v1:fixture"
        );
        assert_eq!(receipt.execution_ref(), f.binding.execution_ref());
        let closing = f
            .store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .expect("SU2 slot still occupied");
        assert!(closing.closing, "SU2 entry closed");
        assert_eq!(closing.claim, f.old);
        assert_eq!(
            *f.stop.evidence_release_calls.lock().unwrap(),
            vec![f.binding.clone()],
            "SU2 release after the durable receipt"
        );

        let probe = AttemptStopProbe::install(&f);
        *probe.result.lock().unwrap() = Ok(Some("all-participants-terminal".into()));
        probe.release.add_permits(1);
        assert_eq!(f.begin().await.1["admission"], "blocked", "SU3");
        assert_eq!(
            f.stop.calls.lock().unwrap().len(),
            2,
            "SU3 the durable receipt is reused"
        );
        let Some(AttemptStopStatus::Stopped(stopped)) = probe.last.lock().unwrap().clone() else {
            panic!("SU3 complete scope evidence");
        };
        assert_eq!(stopped.attempt().claim, f.old);
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            None,
            "SU3 retired"
        );
    }
}

#[tokio::test]
async fn maintenance_records_an_occupied_closed_incarnation_end_without_stopping_it() {
    // Maintenance-tick table (record_occupied_worker_ends), one occupied slot:
    // | Row | bound incarnation                                | effect                          |
    // | O1  | provider does not own its evidence               | no observation, no receipt      |
    // | O2  | lease still live                                 | no observation, not expired     |
    // | O4  | closed; provider reports a supersession          | Superseded receipt recorded     |
    // | O3  | the next pass, end already recorded              | no further provider read        |
    // | O5  | closed; provider reports nothing                 | nothing recorded                |
    // Every row observes only: no stop, no evidence release, and the slot and
    // its entry stay exactly as they were.
    async fn pass(f: &Fixture) -> Result<(), ApplicationError> {
        awaken_dispatch::record_occupied_worker_ends(
            f.store.as_ref(),
            f.directory.as_ref(),
            f.stop.as_ref(),
            200,
        )
        .await
    }
    fn receipt_of(f: &Fixture) -> WorkerProcessExit {
        WorkerProcessExit::ended(
            f.binding.provider_id(),
            f.binding.execution_ref(),
            "k8s-container-superseded:v1:fixture",
            awaken_worker_contract::WorkerProcessEnd::Superseded,
        )
        .unwrap()
    }
    async fn unchanged(f: &Fixture, row: &str) {
        f.assert_occupied().await;
        let slot = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        assert!(!slot.unwrap().closing, "{row} entry untouched");
        assert!(f.stop.calls.lock().unwrap().is_empty(), "{row} no stop");
        assert!(
            f.stop.evidence_release_calls.lock().unwrap().is_empty()
                && f.stop.idle_release_calls.lock().unwrap().is_empty(),
            "{row} no release"
        );
    }

    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    *f.stop.idle_observed_exit.lock().unwrap() = Some(receipt_of(&f));
    pass(&f).await.unwrap();
    assert!(
        f.stop.idle_observe_may_stop.lock().unwrap().is_empty(),
        "O1"
    );
    unchanged(&f, "O1").await;

    let live = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 1_000, 5_000).await;
    *live.stop.owns_evidence.lock().unwrap() = true;
    *live.stop.idle_observed_exit.lock().unwrap() = Some(receipt_of(&live));
    pass(&live).await.unwrap();
    assert!(
        live.stop.idle_observe_may_stop.lock().unwrap().is_empty(),
        "O2"
    );
    let record = live
        .directory
        .incarnation(live.binding.worker())
        .await
        .unwrap()
        .unwrap();
    assert!(
        record.process_exit.is_none() && record.snapshot.state != WorkerState::Dead,
        "O2 not expired, nothing recorded"
    );
    unchanged(&live, "O2").await;

    *f.stop.owns_evidence.lock().unwrap() = true;
    pass(&f).await.unwrap();
    assert_eq!(
        *f.stop.idle_observe_may_stop.lock().unwrap(),
        vec![false],
        "O4 observe only"
    );
    let recorded = f
        .directory
        .incarnation(f.binding.worker())
        .await
        .unwrap()
        .unwrap()
        .process_exit;
    assert_eq!(recorded, Some(receipt_of(&f)), "O4");
    unchanged(&f, "O4").await;
    pass(&f).await.unwrap();
    assert_eq!(f.stop.idle_observe_may_stop.lock().unwrap().len(), 1, "O3");
    unchanged(&f, "O3").await;

    let quiet = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    *quiet.stop.owns_evidence.lock().unwrap() = true;
    pass(&quiet).await.unwrap();
    assert_eq!(
        *quiet.stop.idle_observe_may_stop.lock().unwrap(),
        vec![false],
        "O5"
    );
    assert!(
        quiet
            .directory
            .incarnation(quiet.binding.worker())
            .await
            .unwrap()
            .unwrap()
            .process_exit
            .is_none(),
        "O5 nothing recorded"
    );
    unchanged(&quiet, "O5").await;
}

#[tokio::test]
async fn closing_entry_still_records_local_exit_while_the_complete_scope_is_unknown() {
    // CE causes: C1 Runtime already closed entry; C2 original Worker is
    // expired/live; C3 provider exit is exact/unknown; C4 evidence release
    // succeeds/fails; C5 complete-attempt evidence is unknown/missing/unavailable.
    // CE1 C1+expired+exact -> commit the original Worker exit, then release;
    // CE2 live|unknown -> no fabricated exit; CE3 release failure -> keep the
    // committed exit and return unavailable, then reuse it without another stop.
    // CE4 missing/unavailable full evidence still permits the independent local
    // fact, while the original full-scope error remains unavailable to callers.
    // The existing reported_local_entry_closure... case covers positive full
    // evidence without adding a whole-Worker-stop requirement.
    // Every rule retains the same occupied physical slot and refuses successor
    // entry: local exit never upgrades complete-scope uncertainty into closure.
    for (expired, exited, release_fails, scope) in [
        (true, true, false, "unknown"),
        (true, false, false, "unknown"),
        (false, true, false, "unknown"),
        (true, true, true, "unknown"),
        (true, true, false, "missing"),
        (true, true, false, "unavailable"),
    ] {
        let f = Fixture::new(
            Arc::new(MemoryWorkerDirectory::new()),
            true,
            if expired { 50 } else { 1_000 },
            5_000,
        )
        .await;
        f.store.close_attempt(&f.old).await.unwrap();
        let original = f.store.load_physical_attempt(&f.old.run_id).await.unwrap();
        *f.stop.result.lock().unwrap() = Ok(exited.then(|| "exact-local-exit".into()));
        if release_fails {
            *f.stop.evidence_release_result.lock().unwrap() = Err("release unavailable".into());
        }
        let full_probe = (scope == "unavailable").then(|| {
            let probe = AttemptStopProbe::install(&f);
            *probe.result.lock().unwrap() = Err("full scope unavailable".into());
            probe.release.add_permits(1);
            probe
        });
        f.stop.release.add_permits(1);
        if scope == "missing" {
            let observed = stop_expired_predecessor(
                f.store.as_ref(),
                f.directory.as_ref(),
                f.stop.as_ref(),
                None,
                &f.next,
                200,
            )
            .await
            .unwrap();
            assert!(
                matches!(observed, Some(WorkerProcessStopStatus::Exited(_))),
                "CE4"
            );
        } else {
            let result = f.begin().await;
            assert_eq!(
                result.0,
                if release_fails || scope == "unavailable" {
                    reqwest::StatusCode::SERVICE_UNAVAILABLE
                } else {
                    reqwest::StatusCode::OK
                }
            );
        }
        let recorded = f
            .directory
            .incarnation(f.binding.worker())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            recorded.process_exit.is_some(),
            expired && exited,
            "CE1/CE2"
        );
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            original
        );
        assert_eq!(f.stop.calls.lock().unwrap().len(), usize::from(expired));
        if expired && exited {
            *f.stop.evidence_release_result.lock().unwrap() = Ok(());
            if let Some(probe) = full_probe {
                *probe.result.lock().unwrap() = Ok(None);
                probe.release.add_permits(1);
            }
            assert_eq!(f.begin().await.1["admission"], "blocked", "CE1/CE3");
            assert_eq!(
                f.stop.calls.lock().unwrap().len(),
                1,
                "CE3 no repeated stop"
            );
            assert_eq!(f.stop.evidence_release_calls.lock().unwrap().len(), 2);
            assert_eq!(
                f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
                original
            );
        }
    }
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
        Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
            first.recovery.as_ref(),
        )),
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
            Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
                second.recovery.as_ref()
            )),
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
            Some(&awaken_dispatch::PhysicalAttemptRecoveryRead::new(
                second.recovery.as_ref()
            )),
            &second.next,
            200
        )
        .await
        .is_err()
    );
    first.assert_occupied().await;
    second.assert_occupied().await;
}

async fn dispatch_stores() -> Vec<Arc<dyn WorkerDispatchAuthority>> {
    let mut stores: Vec<Arc<dyn WorkerDispatchAuthority>> = vec![
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
    // Keep the Worker lease live independently of the expired Dispatch lease.
    // A heartbeat at 200 cannot renew a Worker lease that expired at 150;
    // ignoring that rejection would make the claimed healthy case invalid.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 1_000, 5_000).await;
    let old_worker = f.binding.worker().clone();
    let heartbeat = f
        .directory
        .heartbeat(
            &old_worker,
            WorkerHeartbeat {
                sequence: 2,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                source_retention: Default::default(),
            },
            200,
            10_000,
        )
        .await
        .unwrap();
    assert_eq!(
        heartbeat,
        awaken_dispatch::RegistryMutation::Applied,
        "H0 live Worker premise"
    );
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
        f.store.finish_attempt(&f.old, None).await.unwrap(),
        awaken_dispatch::SettleOutcome::Fenced
    );
    probe.release.add_permits(1);
    let pending = f.begin().await;
    assert_eq!(pending.0, reqwest::StatusCode::OK, "H2: {}", pending.1);
    assert_eq!(pending.1["admission"], "blocked");
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

#[tokio::test]
async fn committed_terminal_stop_rejects_unproven_or_changed_originals() {
    // Causes: C1 exact committed Ended Run/Thread; C2 exact occupied original
    // summary/live slot consistent; C3 original binding present; C4 Registry expired/closed, including
    // an already-closing slot. Effects: E0 no provider IO or slot mutation;
    // E1 identity conflict is explicit. Terminal DTO/queue state alone is no
    // stop grant. Each rule varies one cause from the positive T8 below.
    // Execution lowers C1 through the same SDK predicate; no Ingress copy or
    // caller-provided terminal value supplies that fact. The borrowed reader
    // publishes values only, preserving these identity/error partitions.
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | T0 | missing/running/awaiting | yes | yes | yes | E0 |
    // | T1 | foreign Thread or original Session affinity | yes | yes | yes | E0+E1 |
    // | T2 | exact | summary says empty but live slot occupied | yes | yes | E0+error |
    // | T3 | exact | yes | missing | yes | E0 |
    // | T4 | exact | open/closing | yes | live | E0 |
    for rule in [
        "missing",
        "running",
        "awaiting",
        "foreign-thread",
        "foreign-session",
        "changed",
        "historical",
        "live",
        "closing-live",
    ] {
        let f = Fixture::new(
            Arc::new(MemoryWorkerDirectory::new()),
            rule != "historical",
            if rule.contains("live") { 1_000 } else { 50 },
            5_000,
        )
        .await;
        if rule == "closing-live" {
            f.store.close_attempt(&f.old).await.unwrap();
        }
        let before = f
            .store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        let mut expected = f
            .store
            .dispatch_summary(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        if rule == "changed" {
            expected.physical_attempt_active = false;
        }
        if rule == "foreign-session" {
            expected.session_thread_id = Some(request_thread_id("foreign-parent"));
        }
        let record = (rule != "missing").then(|| RunRecord {
            id: f.old.run_id.clone(),
            thread_id: if rule == "foreign-thread" {
                request_thread_id("foreign")
            } else {
                f.thread_id.clone()
            },
            state: match rule {
                "running" => RunState::Running,
                "awaiting" => RunState::Awaiting,
                _ => RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
            },
        });
        let view = TerminalRunRead(record);
        let probe = AttemptStopProbe::install(&f);
        f.stop.release.add_permits(1);
        probe.release.add_permits(1);
        let stopped = f.stop_terminal(&view, &expected).await;
        if rule == "foreign-thread" {
            assert_eq!(
                stopped.unwrap_err().to_string(),
                "terminal closure names another committed Run or Thread",
                "T1"
            );
        } else if rule == "changed" {
            assert_eq!(
                stopped.unwrap_err().to_string(),
                "terminal summary and original physical attempt disagree",
                "T2"
            );
        } else if rule == "foreign-session" {
            assert_eq!(
                stopped.unwrap_err().to_string(),
                "terminal scan and original physical attempt have different affinity",
                "T1 affinity"
            );
        } else {
            assert!(!stopped.unwrap(), "{rule}");
        }
        assert!(f.stop.calls.lock().unwrap().is_empty(), "{rule}");
        assert!(probe.calls.lock().unwrap().is_empty(), "{rule}");
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            Some(before),
            "{rule}"
        );
    }
}

#[tokio::test]
async fn committed_terminal_closure_needs_no_current_successor_and_retains_unknown_domains() {
    // T5 exact Ended + original closed Worker + Unknown local exit => retain
    // original open slot, no complete-provider IO. T6 local exit alone =>
    // retain exact closing slot, no closure record. T7 failed provider read =>
    // same closing slot/no record. T8 complete exact proof => atomic one old
    // closure record+empty slot, never a Run claim or delivery settlement.
    // Original next claim is explicitly relinquished before every invocation;
    // no ordinary successor or fake admission coordinates authorize this path.
    // Cross existing Memory/SQLite/configured PostgreSQL adapters, unchanged
    // old HTTP successor matrices continue to cover their separate admission.
    // The Execution adapter reads the original parent partition/Thread/Run
    // afresh after closing; it never uses a previously sampled projection.
    // The original pre/post-snapshot slot fences still reject a replacement
    // during that IO before any complete-provider request or record append.
    // T8 stale occupied summary+now-empty live slot => explicit retryable error,
    // never a fabricated vacancy. Fresh empty summary => terminal claim readiness
    // only; no new stop/receipt. This is the opposite mismatch to T2.
    for store in dispatch_stores().await {
        let f = Fixture::with_store(
            store,
            Arc::new(MemoryWorkerDirectory::new()),
            true,
            50,
            5_000,
        )
        .await;
        f.store.relinquish_claim(&f.next).await.unwrap();
        assert!(
            !f.store.claim_is_current(&f.next).await.unwrap(),
            "no current successor"
        );
        let view = TerminalRunRead(Some(RunRecord {
            id: f.old.run_id.clone(),
            thread_id: f.thread_id.clone(),
            state: RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
        }));
        let probe = AttemptStopProbe::install(&f);
        let original = f
            .store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        let summary = f
            .store
            .dispatch_summary(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        f.stop.release.add_permits(1);
        assert!(
            !f.stop_terminal(&view, &summary).await.unwrap(),
            "T5 only delivery readiness is returned"
        );
        assert!(probe.calls.lock().unwrap().is_empty(), "T5");
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            Some(original.clone()),
            "T5"
        );
        *f.stop.result.lock().unwrap() = Ok(Some("original-process-exit".into()));
        f.stop.release.add_permits(1);
        probe.release.add_permits(1);
        assert!(
            !f.stop_terminal(&view, &summary).await.unwrap(),
            "T6 local exit alone is not delivery readiness"
        );
        let closing = f
            .store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(closing.closing, "T6");
        assert_eq!(closing.claim, original.claim, "T6 identity");
        *probe.result.lock().unwrap() = Err("lost domain observation".into());
        probe.release.add_permits(1);
        assert_eq!(
            f.stop_terminal(&view, &summary)
                .await
                .unwrap_err()
                .to_string(),
            "lost domain observation",
            "T7"
        );
        assert_eq!(
            f.store.load_physical_attempt(&f.old.run_id).await.unwrap(),
            Some(closing.clone()),
            "T7"
        );
        assert!(
            f.store
                .attempt_closure_records_after(
                    awaken_dispatch::DispatchCursor(0),
                    10,
                    Some(&f.old.run_id)
                )
                .await
                .unwrap()
                .events
                .is_empty(),
            "T6/T7"
        );
        *probe.result.lock().unwrap() = Ok(Some("all-original-participants-terminal".into()));
        probe.release.add_permits(1);
        assert!(
            f.stop_terminal(&view, &summary).await.unwrap(),
            "T8 original physical proof allows only trying terminal claim"
        );
        assert!(
            f.store
                .load_physical_attempt(&f.old.run_id)
                .await
                .unwrap()
                .is_none(),
            "T8"
        );
        let records = f
            .store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor(0),
                10,
                Some(&f.old.run_id),
            )
            .await
            .unwrap();
        assert_eq!(records.events.len(), 1, "T8");
        assert_eq!(
            records.events[0].record.attempt(),
            &closing,
            "T8 exact old identity"
        );
        assert_eq!(
            match &records.events[0].record {
                awaken_dispatch::AttemptPhysicalFact::IndependentClosure { record } =>
                    record.evidence_ref(),
                _ => panic!("T8 trusted stop must retain independent closure"),
            },
            "all-original-participants-terminal",
            "T8"
        );
        assert_eq!(
            f.stop_terminal(&view, &summary)
                .await
                .unwrap_err()
                .to_string(),
            "terminal summary and original physical attempt disagree",
            "T8 stale occupied summary cannot be vacancy"
        );
        let empty = f
            .store
            .dispatch_summary(&f.old.run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            f.stop_terminal(&view, &empty).await.unwrap(),
            "T8 canonical empty replay has no physical IO"
        );
        assert_eq!(
            probe.calls.lock().unwrap().len(),
            3,
            "T6/T7/T8 same original domain"
        );
        assert!(
            f.store
                .completion_events_after(0, 1_000)
                .await
                .unwrap()
                .iter()
                .all(|event| event.run_id != f.old.run_id),
            "closure is not Done for this exact Run"
        );
    }
}

#[tokio::test]
async fn cancelled_terminal_observation_retries_the_same_original_closing_slot() {
    // T9 the canonical domain source is pending after local exit and closing
    // CAS; dropping maintenance => no full closure receipt/retirement. T10
    // next cadence retries exact persisted binding without stopping the Worker
    // twice; one positive source receipt atomically clears only the old slot.
    // Barrier cancellation covers response loss without sleeps or new budgets.
    let f = Fixture::new(Arc::new(MemoryWorkerDirectory::new()), true, 50, 5_000).await;
    f.store.relinquish_claim(&f.next).await.unwrap();
    let view = TerminalRunRead(Some(RunRecord {
        id: f.old.run_id.clone(),
        thread_id: f.thread_id.clone(),
        state: RunState::Ended(awaken_agent_contract::agent::run::EndCause::NaturalEnd),
    }));
    let probe = AttemptStopProbe::install(&f);
    *f.stop.result.lock().unwrap() = Ok(Some("original-local-exit".into()));
    *probe.result.lock().unwrap() = Ok(Some("original-full-domain-exit".into()));
    let original = f
        .store
        .load_physical_attempt(&f.old.run_id)
        .await
        .unwrap()
        .unwrap();
    let summary = f
        .store
        .dispatch_summary(&f.old.run_id)
        .await
        .unwrap()
        .unwrap();
    f.stop.release.add_permits(1);
    {
        let pending = f.stop_terminal(&view, &summary);
        tokio::pin!(pending);
        tokio::select! {
            result = &mut pending => panic!("T9 domain unexpectedly completed: {result:?}"),
            permit = probe.entered.acquire() => permit.unwrap().forget(),
        }
    }
    let closing = f
        .store
        .load_physical_attempt(&f.old.run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(closing.closing, "T9");
    assert_eq!(closing.claim, original.claim, "T9");
    assert!(
        f.store
            .attempt_closure_records_after(
                awaken_dispatch::DispatchCursor(0),
                10,
                Some(&f.old.run_id)
            )
            .await
            .unwrap()
            .events
            .is_empty(),
        "T9"
    );
    probe.release.add_permits(1);
    f.stop_terminal(&view, &summary).await.unwrap();
    assert!(
        f.store
            .load_physical_attempt(&f.old.run_id)
            .await
            .unwrap()
            .is_none(),
        "T10"
    );
    assert_eq!(
        f.stop.calls.lock().unwrap().len(),
        1,
        "T10 no repeated local stop"
    );
    assert_eq!(
        *probe.calls.lock().unwrap(),
        vec![closing.clone(), closing],
        "T9/T10"
    );
}

#[test]
fn postgres_configuration_contract() {
    // C1 each adapter receives a silent PostgreSQL listener; C2 the child
    // deadline exceeds the shared connection deadline. R3=C1+C2 => E1 every
    // adapter entry reports a bounded configured-PG failure before termination.
    postgres_support::assert_configuration_contract(&[
        "blocked_http_closes_exact_expired_worker_but_local_exit_never_clears_remote_obligations",
        "complete_stop_uses_exact_retirement_and_requires_fresh_successor_admission",
        "closed_worker_exit_survives_an_unavailable_physical_scan",
    ]);
}

use awaken_dispatch_contract::{
    DispatchClaims as _, DispatchControlReads as _, DispatchEnqueue as _, DispatchExecution as _,
};

use awaken_dispatch_http::WorkerDispatchAuthority;

/// Input values for the terminal-stop application's identity/error matrix.
/// Foreign records deliberately reach that application port: a Worker's
/// recovery cache now refuses them at its own earlier installation boundary.
/// This fixture owns no cache or commit/stop authority, and unexpected reads
/// fail so the case cannot silently gain a dependency on another projection.
struct TerminalRunRead(Option<RunRecord>);

impl awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView
    for TerminalRunRead
{
    fn run(&self, _: &awaken_agent_contract::agent::run::Id) -> Option<RunRecord> {
        self.0.clone()
    }

    fn committed_messages(
        &self,
        _: &awaken_agent_contract::agent::thread::Id,
    ) -> Vec<awaken_agent_contract::agent::message::Message> {
        unreachable!("terminal-stop admission reads only its exact Run")
    }

    fn latest_run(&self, _: &awaken_agent_contract::agent::thread::Id) -> Option<RunRecord> {
        unreachable!("an exact terminal Run does not require the Thread head")
    }

    fn resume_ticket(
        &self,
        _: &awaken_agent_contract::agent::run::Id,
    ) -> Option<awaken_agent_contract::agent::awaiting::ResumeTicket> {
        unreachable!("terminal-stop admission does not resume work")
    }

    fn open_wait_for_thread(
        &self,
        _: &awaken_agent_contract::agent::thread::Id,
    ) -> Option<(
        awaken_agent_contract::agent::run::Id,
        awaken_agent_contract::agent::awaiting::ResumeTicket,
    )> {
        unreachable!("terminal-stop admission does not resume work")
    }
}
