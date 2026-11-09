//! Claim fencing delegates operation identity, CAS and receipts to the real store.

use super::*;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::RunDisposition;
use awaken_dispatch_contract::{DispatchError, PhysicalAttemptSlot, RunDispatch};
use awaken_runtime_contract::{ExecutableAgentSnapshot, RunActivation};
use awaken_store_inmem::MemoryCommitCoordinator;
use awaken_worker_contract::WorkerIdentity;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Authority {
    held: Arc<AtomicBool>,
    releases: Arc<AtomicUsize>,
    reject_release: bool,
    release_entered: Arc<tokio::sync::Notify>,
    release_allowed: Option<Arc<tokio::sync::Notify>>,
}

impl Authority {
    fn new(reject_release: bool) -> Self {
        Self {
            held: Arc::new(AtomicBool::new(false)),
            releases: Arc::new(AtomicUsize::new(0)),
            reject_release,
            release_entered: Arc::new(tokio::sync::Notify::new()),
            release_allowed: None,
        }
    }
}

#[cfg(test)]
#[async_trait]
impl DispatchCommitAuthority for Authority {
    async fn worker_owns_run(
        &self,
        _: &WorkerIdentity,
        _: &RunId,
    ) -> Result<Option<RunClaim>, DispatchError> {
        unreachable!("claim is already selected in this fence-boundary fixture")
    }

    async fn lock_commit_epoch(
        &self,
        claim: &RunClaim,
    ) -> Result<Option<CommitEpochGuard>, DispatchError> {
        if claim.owner != "worker" || claim.epoch != 1 || claim.run_id != RunId("run".into()) {
            return Ok(None);
        }
        self.held.store(true, Ordering::SeqCst);
        let held = self.held.clone();
        let releases = self.releases.clone();
        let reject = self.reject_release;
        let entered = self.release_entered.clone();
        let allowed = self.release_allowed.clone();
        Ok(Some(CommitEpochGuard::new_releasing(
            async move {
                entered.notify_one();
                if let Some(allowed) = allowed {
                    tokio::time::timeout(std::time::Duration::from_secs(30), allowed.notified())
                        .await
                        .expect("release permission");
                }
                releases.fetch_add(1, Ordering::SeqCst);
                if reject {
                    return Err("injected exact release failure".into());
                }
                held.store(false, Ordering::SeqCst);
                Ok(())
            },
            RunDispatch::new(RunActivation::new(
                RunId("run".into()),
                ThreadId("thread".into()),
                ExecutableAgentSnapshot::builder("snapshot")
                    .model(
                        awaken_runtime_contract::resolved::ModelBinding::parse(
                            "test", "test", "native",
                        )
                        .expect("complete fixture model binding"),
                    )
                    .build(),
                Vec::new(),
            )),
            100,
            false,
            PhysicalAttemptSlot::Empty,
        )))
    }
}

fn command(ordinal: u64, version: u64) -> ClaimedCommitCommand {
    let run = RunId("run".into());
    let commit = ThreadCommit::assemble(
        ThreadId("thread".into()),
        RunDisposition::running(run.clone()),
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    );
    ClaimedCommitCommand {
        claim: RunClaim {
            run_id: run.clone(),
            owner: "worker".into(),
            epoch: 1,
        },
        operation: CommitOperation {
            operation_id: CommitOperationId::new(run, ordinal),
            expected_thread_version: version,
            payload_hash: commit_payload_hash(&commit).unwrap(),
            commit,
        },
    }
}

#[tokio::test]
async fn guarded_operations_return_original_replay_receipts_and_reject_hash_or_cas_conflicts() {
    // Causes: C1 claim is exact; C2 operation is new/replayed; C3 replay retains
    // its payload hash; C4 new operation uses the current Thread version and
    // ordinal. Effects: E1 exactly one append and original receipt; E2 replay
    // returns that same sequence/version with duplicate=true; E3 reject without
    // append. Rules: R1=C1+new+C4=>E1; R2=C1+replay+C3=>E2;
    // R3=C1+replay+!C3=>E3; R4=C1+new+!C4=>E3; R5=!C1=>E3.
    // The real Memory operation owner supplies every receipt; the fake supplies
    // only dispatch lock lifetime, never commit sequencing or replay truth.
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let authority = Arc::new(Authority::new(false));
    let service = GuardedRunCommit::new(store.clone(), authority.clone());
    let original = command(0, 0);
    let first = service.commit_operation(original.clone()).await.unwrap();
    assert!(!first.duplicate, "R1/E1");
    let replay = service.commit_operation(original.clone()).await.unwrap();
    assert!(replay.duplicate, "R2/E2");
    assert_eq!(replay.commit_sequence, first.commit_sequence, "R2/E2");
    assert_eq!(replay.thread_version, first.thread_version, "R2/E2");
    assert_eq!(replay.payload_hash, first.payload_hash, "R2/E2");
    assert_eq!(store.commit_count(), 1, "R1/R2 one append");
    let mut changed = original.clone();
    changed.operation.commit.run = RunDisposition::ended(RunId("run".into()), EndCause::NaturalEnd);
    changed.operation.payload_hash = commit_payload_hash(&changed.operation.commit).unwrap();
    assert!(service.commit_operation(changed).await.is_err(), "R3/E3");
    assert!(
        service.commit_operation(command(1, 0)).await.is_err(),
        "R4/E3 stale CAS"
    );
    assert!(
        service.commit_operation(command(2, 1)).await.is_err(),
        "R4/E3 stale ordinal"
    );
    for mismatch in ["epoch", "owner", "run"] {
        let mut stale = command(1, 1);
        match mismatch {
            "epoch" => stale.claim.epoch = 0,
            "owner" => stale.claim.owner = "foreign".into(),
            _ => stale.claim.run_id = RunId("foreign".into()),
        }
        assert!(
            service.commit_operation(stale).await.is_err(),
            "R5/E3 {mismatch}"
        );
    }
    assert_eq!(store.commit_count(), 1, "R3-R5 atomic stutters");
    assert_eq!(authority.releases.load(Ordering::SeqCst), 5);
    assert!(!authority.held.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn coordinator_writer_retries_only_ambiguous_commits_with_the_same_identity() {
    struct Writer {
        store: MemoryCommitCoordinator,
        calls: std::sync::Mutex<Vec<ClaimedCommitCommand>>,
        lose_receipt: bool,
    }

    #[async_trait]
    impl ClaimedRunCommit<RunError> for Writer {
        async fn commit_operation(
            &self,
            command: ClaimedCommitCommand,
        ) -> Result<CommitReceipt, RunError> {
            let first = {
                let mut calls = self.calls.lock().unwrap();
                let first = calls.is_empty();
                calls.push(command.clone());
                first
            };
            if !self.lose_receipt {
                return Err(RunError::conflict_classified(
                    awaken_dispatch_contract::COMMIT_CONFLICT,
                    "stale version",
                ));
            }
            let receipt = self
                .store
                .commit_operation(command.operation)
                .await
                .unwrap();
            if first {
                return Err(RunError::unavailable_classified(
                    awaken_dispatch_contract::COMMIT_OUTCOME_UNKNOWN,
                    "receipt lost",
                ));
            }
            Ok(receipt)
        }
    }

    for lose_receipt in [false, true] {
        let writer = Arc::new(Writer {
            store: MemoryCommitCoordinator::new(awaken_time::system_clock()),
            calls: std::sync::Mutex::new(Vec::new()),
            lose_receipt,
        });
        let request = command(0, 0);
        let result = CoordinatorRunCommits(writer.clone())
            .commit_operation(request.clone())
            .await;
        let calls = writer.calls.lock().unwrap();
        assert_eq!(calls.len(), if lose_receipt { 2 } else { 1 });
        assert!(
            calls
                .iter()
                .all(|call| call.claim == request.claim && call.operation == request.operation)
        );
        if lose_receipt {
            assert!(result.unwrap().duplicate);
            assert_eq!(writer.store.commit_count(), 1);
        } else {
            assert!(result.unwrap_err().to_string().contains("stale version"));
            assert_eq!(writer.store.commit_count(), 0);
        }
    }
}

#[tokio::test]
async fn embedded_commit_publishes_under_the_guard_and_releases_before_returning() {
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let authority = Arc::new(Authority::new(false));
    let service = GuardedRunCommit::from_plain(store.clone(), authority.clone());
    let command = command(0, 0);
    let record = service
        .commit(&command.claim, command.operation.commit)
        .await
        .unwrap();
    assert_eq!(record.sequence, 1, "P2/E2 exact real-store record");
    assert_eq!(store.commit_count(), 1, "P2/E2");
    assert_eq!(authority.releases.load(Ordering::SeqCst), 1, "P2/E2");
}

#[tokio::test]
async fn guarded_operation_waits_for_release_and_surfaces_release_failure_after_real_append() {
    // Causes: C1 original operation succeeds or binding/store rejects; C2 exact
    // asynchronous release is pending/succeeds/fails. Rules: B1=C1=*+pending
    // => hide result while held; B2=C1=*+success=>return original result;
    // B3=C1=*+failure=>explicit release rejection. A successful append remains
    // durable even when B3 prevents acknowledgement; its replay is still one
    // original store operation. Binding rejection must also await release.
    for rejected in ["none", "thread", "run", "store-cas"] {
        let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let allowed = Arc::new(tokio::sync::Notify::new());
        let mut authority = Authority::new(false);
        authority.release_allowed = Some(allowed.clone());
        let authority = Arc::new(authority);
        let service = Arc::new(GuardedRunCommit::new(store.clone(), authority.clone()));
        let mut request = command(0, 0);
        match rejected {
            "thread" => request.operation.commit.thread_id = ThreadId("foreign".into()),
            "run" => {
                request.operation.commit.run = RunDisposition::running(RunId("foreign".into()))
            }
            "store-cas" => request.operation.expected_thread_version = 1,
            _ => {}
        }
        let applying = tokio::spawn(async move { service.commit_operation(request).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            authority.release_entered.notified(),
        )
        .await
        .unwrap();
        assert!(authority.held.load(Ordering::SeqCst), "B1 held");
        assert!(!applying.is_finished(), "B1 hides settled result");
        assert_eq!(
            store.commit_count(),
            u64::from(rejected == "none"),
            "original append outcome"
        );
        allowed.notify_one();
        assert_eq!(
            applying.await.unwrap().is_ok(),
            rejected == "none",
            "B2 original result"
        );
        assert!(!authority.held.load(Ordering::SeqCst), "B2 released");
    }
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let authority = Arc::new(Authority::new(true));
    let service = GuardedRunCommit::new(store.clone(), authority.clone());
    let error = service.commit_operation(command(0, 0)).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("commit claim lock release failed"),
        "B3"
    );
    assert_eq!(
        store.commit_count(),
        1,
        "B3 durable append survives lost acknowledgement"
    );
    assert!(
        authority.held.load(Ordering::SeqCst),
        "B3 no fictitious release success"
    );
    // A replacement lock after the backend reports release is modeled by its
    // own original dispatch authority, while replay remains the same store.
    let released = GuardedRunCommit::new(store.clone(), Arc::new(Authority::new(false)));
    let receipt = released.commit_operation(command(0, 0)).await.unwrap();
    assert!(receipt.duplicate, "B3 retry returns original receipt");
    assert_eq!(store.commit_count(), 1, "B3 retry never repeats append");
}

#[tokio::test]
async fn guarded_operation_binding_failure_still_reports_failed_release_without_append() {
    // Causes: C1 commit binding differs from the guarded dispatch; C2 exact
    // release fails. Rule F1=C1+C2=>explicit release error, zero append. This
    // complements B3's lawful append-before-release-failure case: a release
    // error cannot erase durable truth or turn a rejected binding into truth.
    for wrong_run in [false, true] {
        let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
        let authority = Arc::new(Authority::new(true));
        let service = GuardedRunCommit::new(store.clone(), authority);
        let mut request = command(0, 0);
        if wrong_run {
            request.operation.commit.run = RunDisposition::running(RunId("foreign".into()));
        } else {
            request.operation.commit.thread_id = ThreadId("foreign".into());
        }
        let error = service.commit_operation(request).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("commit claim lock release failed"),
            "F1"
        );
        assert_eq!(store.commit_count(), 0, "F1 no append");
    }
}

struct BlockedCommit {
    store: Arc<MemoryCommitCoordinator>,
    held: Arc<AtomicBool>,
    entered: tokio::sync::Notify,
    allowed: tokio::sync::Notify,
}

#[async_trait]
impl CommitCoordinator for BlockedCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        assert!(self.held.load(Ordering::SeqCst));
        self.store.commit(commit).await
    }
}

#[cfg(test)]
#[async_trait]
impl OperationCoordinator for BlockedCommit {
    async fn commit_operation(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, CommitError> {
        assert!(
            self.held.load(Ordering::SeqCst),
            "epoch held before original operation"
        );
        self.entered.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(30), self.allowed.notified())
            .await
            .expect("operation permission");
        assert!(
            self.held.load(Ordering::SeqCst),
            "epoch held through original operation IO"
        );
        self.store.commit_operation(operation).await
    }
}

#[tokio::test]
async fn guarded_versioned_commit_holds_exact_epoch_across_original_operation_io() {
    // Causes: C1 original operation future has entered but not returned; C2
    // its IO is released. Rules H1=C1+!C2=>epoch held, no release or receipt;
    // H2=C1+C2=>real store append, then exact release, then original receipt.
    // The blocking decorator only delays IO; the real store owns the receipt.
    let store = Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()));
    let authority = Arc::new(Authority::new(false));
    let owner = Arc::new(BlockedCommit {
        store: store.clone(),
        held: authority.held.clone(),
        entered: tokio::sync::Notify::new(),
        allowed: tokio::sync::Notify::new(),
    });
    let service = GuardedRunCommit::new(owner.clone(), authority.clone());
    let applying = tokio::spawn(async move { service.commit_operation(command(0, 0)).await });
    tokio::time::timeout(std::time::Duration::from_secs(30), owner.entered.notified())
        .await
        .unwrap();
    assert!(authority.held.load(Ordering::SeqCst), "H1");
    assert_eq!(authority.releases.load(Ordering::SeqCst), 0, "H1");
    assert_eq!(store.commit_count(), 0, "H1");
    assert!(!applying.is_finished(), "H1");
    owner.allowed.notify_one();
    let receipt = applying.await.unwrap().unwrap();
    assert_eq!(receipt.commit_sequence, 1, "H2");
    assert_eq!(store.commit_count(), 1, "H2");
    assert_eq!(authority.releases.load(Ordering::SeqCst), 1, "H2");
    assert!(!authority.held.load(Ordering::SeqCst), "H2");
}
