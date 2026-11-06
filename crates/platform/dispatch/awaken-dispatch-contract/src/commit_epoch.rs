// @formal entrypoint ingress.commit_epoch_atomic_local
// @formal entrypoint ingress.commit_epoch_atomic_remote_custom
// @formal entrypoint remote_worker.committed_truth_is_coordinator_owned
// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint remote_worker.stale_epoch_never_mutates_truth
/// Opaque backend guard which keeps a dispatch epoch stable until it is dropped.
/// PostgreSQL stores a row-locking transaction in it; SQLite stores its shared
/// single-process authority mutex guard. The ingress layer needs only the
/// lifetime, never the backend-specific value.
use std::future::Future;
use std::pin::Pin;

use awaken_agent_contract::thread::read::recovery::RunRecoverySource;
use awaken_time::WallClockExt as _;

type ReleaseFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Production-consumed typestate for the repository-owned part of a guarded
/// commit. The backend guard itself remains opaque, but no completion decision
/// exists until the inner commit has settled and the explicit release result is
/// available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitEpochBoundary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettledCommitEpochBoundary;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitEpochCompletion {
    ReturnInner,
    FailRelease,
}

/// Storage-neutral decision for installing an exact claim's commit guard.
///
/// A backend supplies facts from one locked row snapshot. An absent or
/// different claim is fenced; a row in the required leased state with a torn
/// owner/expiry pair is corrupt; only the complete exact owner/epoch pair can
/// guard committed-truth mutation. Expiry is retained as evidence but is not a
/// wall-clock admission check: holding the row/aggregate lock prevents reclaim
/// until the guarded commit finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitEpochAdmission {
    Fenced,
    Corrupt,
    Guarded,
}

#[must_use]
pub const fn classify_commit_epoch_admission(
    row_in_required_state: bool,
    owner_present: bool,
    expiry_present: bool,
    owner_matches: bool,
    epoch_matches: bool,
) -> CommitEpochAdmission {
    if !row_in_required_state {
        return CommitEpochAdmission::Fenced;
    }
    if !owner_present || !expiry_present {
        return CommitEpochAdmission::Corrupt;
    }
    if owner_matches && epoch_matches {
        CommitEpochAdmission::Guarded
    } else {
        CommitEpochAdmission::Fenced
    }
}

impl CommitEpochBoundary {
    #[must_use]
    pub const fn held() -> Self {
        Self
    }

    #[must_use]
    pub const fn settle_inner(self) -> SettledCommitEpochBoundary {
        SettledCommitEpochBoundary
    }
}

impl SettledCommitEpochBoundary {
    /// The exact release truth table. Inner success/failure is deliberately
    /// carried by the caller; this reducer decides only whether that outcome is
    /// safe to expose after the physical claim authority has been released.
    #[must_use]
    pub const fn complete(self, release_succeeded: bool) -> CommitEpochCompletion {
        if release_succeeded {
            CommitEpochCompletion::ReturnInner
        } else {
            CommitEpochCompletion::FailRelease
        }
    }
}

/// How a guarded prepared-activity admission proves the Run's history.
///
/// Decision table (`prepared_admission_requires_exact_absence_and_observed_release`,
/// on a live claim over an empty physical slot):
/// | Rule | recovery | original Thread | Effect |
/// |---|---|---|---|
/// | A1 | `OriginalAuthority` | exact Thread and Run, no Run record | the root runs after the read's store cursor |
/// | A2 | `OriginalAuthority` | foreign Thread or Run, or a Run record | refused as uncertain; no root |
/// | A3 | `OriginalAuthority` | the read fails | refused; no root |
/// | A4 | `ReceiptOnly` | not read | the root runs at cursor 0 and may only recover or refuse its receipt |
#[derive(Clone, Copy)]
pub enum PreparedRunRecovery<'a> {
    /// A projected revision exists: the original committed authority must
    /// certify that this Run has no history yet.
    OriginalAuthority(&'a dyn RunRecoverySource),
    /// No projected revision: nothing certifies historical absence, so the
    /// Session root may only recover an exact receipt or refuse.
    ReceiptOnly,
}

enum HeldAuthority {
    Drop(Box<dyn Send>),
    Release(ReleaseFuture),
}

pub struct CommitEpochGuard {
    held: HeldAuthority,
    request: RunDispatch,
    expires_ms: u64,
    cancellation_requested: bool,
    physical_attempt: crate::PhysicalAttemptSlot,
}

impl CommitEpochGuard {
    #[must_use]
    pub fn new(
        held: impl Send + 'static,
        request: RunDispatch,
        expires_ms: u64,
        cancellation_requested: bool,
        physical_attempt: crate::PhysicalAttemptSlot,
    ) -> Self {
        Self {
            held: HeldAuthority::Drop(Box::new(held)),
            request,
            expires_ms,
            cancellation_requested,
            physical_attempt,
        }
    }

    /// Construct a guard whose backend lock has an asynchronous release
    /// boundary. PostgreSQL transactions use this path so returning from a
    /// guarded commit means the row lock is observably gone, rather than only
    /// scheduled for rollback by `Drop`.
    pub fn new_releasing(
        release: impl Future<Output = Result<(), String>> + Send + 'static,
        request: RunDispatch,
        expires_ms: u64,
        cancellation_requested: bool,
        physical_attempt: crate::PhysicalAttemptSlot,
    ) -> Self {
        Self {
            held: HeldAuthority::Release(Box::pin(release)),
            request,
            expires_ms,
            cancellation_requested,
            physical_attempt,
        }
    }

    /// Release the exact backend authority before the guarded operation is
    /// reported complete. Drop-only backends release synchronously here;
    /// asynchronous backends must finish their explicit transaction close.
    pub async fn release(self) -> Result<(), String> {
        match self.held {
            HeldAuthority::Drop(held) => {
                drop(held);
                Ok(())
            }
            HeldAuthority::Release(release) => release.await,
        }
    }

    /// Return an already-settled operation result only after this exact backend
    /// authority has observably released. Both embedded commits and remote
    /// claimed-commit ingestion use this boundary, so backend topology cannot
    /// turn an asynchronous release into an early success response.
    pub async fn release_after<T, E>(self, settled: Result<T, E>) -> Result<Result<T, E>, String> {
        let boundary = CommitEpochBoundary::held().settle_inner();
        let release = self.release().await;
        match boundary.complete(release.is_ok()) {
            CommitEpochCompletion::ReturnInner => Ok(settled),
            CommitEpochCompletion::FailRelease => {
                Err(release.expect_err("release failure decision requires an error"))
            }
        }
    }

    /// The one guarded prepared-activity admission boundary. Callers prepare
    /// projections before acquiring this guard, select the original recovery
    /// authority, and supply only their Session root receipt/CAS operation.
    /// [`PreparedRunRecovery::ReceiptOnly`] permits receipt recovery/refusal
    /// only, never evidence of historical Run absence or permission to create
    /// a new activity.
    /// Queue resolution and publication start only after the returned explicit
    /// release result succeeds. This grants no Worker execution authority.
    pub async fn admit_prepared_session_run<T, E, F: Future<Output = Result<T, E>>>(
        self,
        clock: &dyn awaken_time::WallClock,
        recovery: Result<PreparedRunRecovery<'_>, E>,
        admit: impl FnOnce(u64, bool) -> F,
        failure: impl Fn(String) -> E,
    ) -> Result<Result<T, E>, String> {
        // Own observations before suspension: the opaque backend guard is
        // Send, but its storage-specific held value need not be Sync.
        let live = self.is_live_at(clock.now_ms()) && self.permits_settlement();
        let cancelled = self.cancellation_requested();
        let request = self.request().clone();
        let mut admission = async {
            let recovery = recovery?;
            if !live {
                return Err(failure(
                    "prepared reservation requires a live claim and empty physical slot".into(),
                ));
            }
            let cursor = match recovery {
                PreparedRunRecovery::OriginalAuthority(source) => {
                    let snapshot = source
                        .recovery_snapshot_in_session(
                            request.session_thread_id(),
                            request.thread_id(),
                            request.run_id(),
                        )
                        .await
                        .map_err(|error| failure(error.to_string()))?;
                    if snapshot.thread_id != *request.thread_id()
                        || snapshot.claimed_run_id != *request.run_id()
                        || snapshot
                            .run_record(request.run_id())
                            .map_err(|error| failure(error.to_string()))?
                            .is_some()
                    {
                        return Err(failure(
                            "prepared reservation already has committed or conflicting Run truth; admission remains uncertain".into(),
                        ));
                    }
                    snapshot.store_cursor
                }
                PreparedRunRecovery::ReceiptOnly => 0,
            };
            admit(cursor, cancelled).await
        }
        .await;
        if !self.is_live_at(clock.now_ms()) {
            // The root may have committed despite an expired claim or a lost
            // acknowledgement. Only exact receipt recovery may publish it.
            admission = Err(failure(
                "prepared reservation expired during root admission; recover its exact receipt"
                    .into(),
            ));
        }
        self.release_after(admission).await
    }

    /// The authoritative dispatch payload protected by this exact claim guard.
    /// Recovery uses it to select the Thread without trusting a caller-supplied
    /// Thread id.
    #[must_use]
    pub fn request(&self) -> &RunDispatch {
        &self.request
    }

    /// Compare a Resource effect's scope with the locked dispatch authority.
    /// Outputs belong to the execution Thread, not its parent Session recovery
    /// affinity. Local and remote publication reject caller-supplied scopes.
    #[must_use]
    pub fn matches_execution_scope(&self, workspace_id: &str, session_id: &str) -> bool {
        self.request
            .execution_scope
            .as_ref()
            .is_some_and(|scope| scope.0.0 == workspace_id)
            && self.request.thread_id().0 == session_id
    }

    /// Whether the guarded claim's lease is live at `now_ms`: the Expiry law,
    /// dead from the instant a successor may reclaim the row.
    #[must_use]
    pub fn is_live_at(&self, now_ms: u64) -> bool {
        Expiry::from_epoch_ms(self.expires_ms).is_live_at(UtcTime::from_epoch_ms(now_ms))
    }

    /// Expiry of the exact claim protected by this guard.
    #[must_use]
    pub fn expires_ms(&self) -> u64 {
        self.expires_ms
    }

    /// Physical evidence from the same locked row as this exact claim. A
    /// committed terminal is intentionally independent of this permission.
    #[must_use]
    pub fn permits_settlement(&self) -> bool {
        self.physical_attempt.permits_settlement()
    }

    /// Whether the same locked dispatch row already requests cancellation.
    #[must_use]
    pub fn cancellation_requested(&self) -> bool {
        self.cancellation_requested
    }
}

#[cfg(test)]
mod commit_epoch_tests {
    use super::*;

    struct AdmissionClock(std::sync::atomic::AtomicU64);

    impl awaken_time::WallClock for AdmissionClock {
        fn now(&self) -> awaken_time::UtcTime {
            awaken_time::UtcTime::from_epoch_ms(self.0.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    struct AdmissionRecovery {
        snapshot: awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
        fail: bool,
        reads: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl RunRecoverySource for AdmissionRecovery {
        async fn recovery_snapshot(
            &self,
            _thread: &awaken_agent_contract::ThreadId,
            _run: &awaken_agent_contract::agent::run::Id,
        ) -> Result<
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
            awaken_agent_contract::thread::read::recovery::RecoveryError,
        > {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail {
                Err(
                    awaken_agent_contract::thread::read::recovery::RecoveryError::Rejected(
                        "source unavailable".into(),
                    ),
                )
            } else {
                Ok(self.snapshot.clone())
            }
        }
    }

    #[tokio::test]
    async fn prepared_admission_requires_exact_absence_and_observed_release() {
        // Causes: C1 live/expired claim; C2 all four physical-slot states;
        // C3 exact absent / foreign Thread / foreign Run / existing Run /
        // duplicate Run / unavailable source / receipt replay / caller refusal;
        // C4 cancellation; C5 root callback success/failure; C6 expiry during
        // the callback; C7 asynchronous release success/failure.
        // Effects: E1 only C1(live)+C2(empty)+C3(absent/replay) enters the root;
        // E2 it receives exact cursor/cancellation; E3 callback error or expiry
        // stays uncertain; E4 every path releases before any result escapes;
        // E5 release failure suppresses either inner outcome.
        // Rules: R1=!C1 or !C2=>no read/root; R2=C1+C2+bad C3=>no root;
        // R3=C1+C2+good C3=>E1/E2; R4=R3+(!C5 or C6)=>E3;
        // R5=any+C7(success)=>E4/exact inner result; R6=any+!C7=>E4/E5.
        // Full product preserves interacting refusals. Receipt replay certifies
        // no absence here; the supplied Session callback still owns that truth.
        use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};

        for source_kind in 0..8 {
            for now in [99, 100] {
                for slot in [
                    crate::PhysicalAttemptSlot::Empty,
                    crate::PhysicalAttemptSlot::ExactClaim,
                    crate::PhysicalAttemptSlot::OtherClaim,
                    crate::PhysicalAttemptSlot::Corrupt,
                ] {
                    for cancelled in [false, true] {
                        for inner_fails in [false, true] {
                            for expire_inside in [false, true] {
                                for release_fails in [false, true] {
                                    let request = crate::dispatch::tests::a_request();
                                    let record = awaken_agent_contract::agent::run::Record {
                                        id: request.run_id().clone(),
                                        thread_id: request.thread_id().clone(),
                                        state: awaken_agent_contract::agent::run::RunState::Running,
                                    };
                                    let source = AdmissionRecovery {
                                        snapshot: RunRecoverySnapshot {
                                            thread_id: if source_kind == 1 {
                                                awaken_agent_contract::ThreadId("foreign".into())
                                            } else {
                                                request.thread_id().clone()
                                            },
                                            claimed_run_id: if source_kind == 2 {
                                                awaken_agent_contract::agent::run::Id(
                                                    "foreign".into(),
                                                )
                                            } else {
                                                request.run_id().clone()
                                            },
                                            runs: match source_kind {
                                                3 => vec![record.clone()],
                                                4 => vec![record.clone(), record],
                                                _ => Vec::new(),
                                            },
                                            latest_run_id: None,
                                            messages: Vec::new(),
                                            message_commit_cursors: Vec::new(),
                                            commit_timestamps: Default::default(),
                                            state: Vec::new(),
                                            state_commit_cursors: Vec::new(),
                                            events: Vec::new(),
                                            resume_tickets: Vec::new(),
                                            thread_version: 0,
                                            store_cursor: 37,
                                            next_commit_ordinal: 0,
                                        },
                                        fail: source_kind == 5,
                                        reads: AtomicUsize::new(0),
                                    };
                                    let clock = AdmissionClock(AtomicU64::new(now));
                                    let released = std::sync::Arc::new(AtomicUsize::new(0));
                                    let calls = AtomicUsize::new(0);
                                    let guard = CommitEpochGuard::new_releasing(
                                        {
                                            let released = released.clone();
                                            async move {
                                                released.fetch_add(1, SeqCst);
                                                if release_fails {
                                                    Err("release failed".into())
                                                } else {
                                                    Ok(())
                                                }
                                            }
                                        },
                                        request,
                                        100,
                                        cancelled,
                                        slot,
                                    );
                                    let recovery = match source_kind {
                                        6 => Ok(PreparedRunRecovery::ReceiptOnly),
                                        7 => Err("caller refused".to_string()),
                                        _ => Ok(PreparedRunRecovery::OriginalAuthority(&source)),
                                    };
                                    let operation = guard.admit_prepared_session_run(
                                        &clock,
                                        recovery,
                                        |cursor, observed_cancelled| {
                                            calls.fetch_add(1, SeqCst);
                                            assert_eq!(
                                                released.load(SeqCst),
                                                0,
                                                "E4 root precedes release"
                                            );
                                            assert_eq!(
                                                cursor,
                                                if source_kind == 6 { 0 } else { 37 },
                                                "E2 cursor"
                                            );
                                            assert_eq!(
                                                observed_cancelled, cancelled,
                                                "E2 cancellation"
                                            );
                                            if expire_inside {
                                                clock.0.store(100, SeqCst);
                                            }
                                            std::future::ready(if inner_fails {
                                                Err("root failed".to_string())
                                            } else {
                                                Ok(7_u8)
                                            })
                                        },
                                        std::convert::identity,
                                    );
                                    fn require_send<F: Send>(future: F) -> F {
                                        future
                                    }
                                    let result = require_send(operation).await;
                                    let live_empty =
                                        now == 99 && slot == crate::PhysicalAttemptSlot::Empty;
                                    let entered = live_empty && matches!(source_kind, 0 | 6);
                                    assert_eq!(calls.load(SeqCst), usize::from(entered), "R1-R3");
                                    assert_eq!(
                                        source.reads.load(SeqCst),
                                        usize::from(live_empty && source_kind < 6),
                                        "R1/R2 exact read"
                                    );
                                    assert_eq!(released.load(SeqCst), 1, "R5/R6");
                                    if release_fails {
                                        assert_eq!(result, Err("release failed".into()), "R6");
                                    } else if entered && !inner_fails && !expire_inside {
                                        assert_eq!(result, Ok(Ok(7)), "R3/R5");
                                    } else if entered && inner_fails && !expire_inside {
                                        assert_eq!(
                                            result,
                                            Ok(Err("root failed".into())),
                                            "R4/R5 exact callback error"
                                        );
                                    } else {
                                        assert!(result.unwrap().is_err(), "R1/R2/R4/R5");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn settled_result_is_hidden_until_exact_release_completes() {
        // Cause/effect graph: C1 the inner result is success/failure; C2 backend
        // release is pending/succeeds/fails. Effects: E1 a pending release keeps
        // both inner outcomes hidden; E2 successful release returns the exact
        // settled outcome; E3 failed release suppresses either inner outcome.
        // Decision rules: R1=C1(any)+C2(pending)=>E1,
        // R2=C1(any)+C2(success)=>E2, R3=C1(any)+C2(failure)=>E3. The pure
        // typestate/Kani table owns the same Boolean completion decision; this
        // async test checks its one production consumer boundary.
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let releasing = CommitEpochGuard::new_releasing(
            {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    tokio::time::timeout(std::time::Duration::from_secs(30), release.notified())
                        .await
                        .expect("timed out waiting for release.notified()");
                    Ok(())
                }
            },
            crate::dispatch::tests::a_request(),
            1,
            false,
            crate::PhysicalAttemptSlot::Empty,
        );
        let returning = tokio::spawn(releasing.release_after(Ok::<_, &'static str>(7_u8)));
        tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
            .await
            .expect("timed out waiting for entered.notified()");
        tokio::task::yield_now().await;
        assert!(!returning.is_finished(), "R1/E1");
        release.notify_one();
        assert_eq!(returning.await.unwrap().unwrap(), Ok(7), "R2/E2");

        let failed = CommitEpochGuard::new_releasing(
            async { Err("release failed".to_string()) },
            crate::dispatch::tests::a_request(),
            1,
            false,
            crate::PhysicalAttemptSlot::Empty,
        )
        .release_after::<(), _>(Err("inner failed"))
        .await;
        assert_eq!(failed, Err("release failed".to_string()), "R3/E3");
    }

    #[test]
    fn guard_liveness_follows_the_expiry_law_at_the_boundary() {
        // Boundary decision table for the guard's lease (expiry E = 100):
        // | now   | live | reason                                   |
        // | E - 1 | yes  | before the expiry                        |
        // | E     | no   | the Expiry law: expired from `now >= E`  |
        // | E + 1 | no   | after the expiry                         |
        // | 0 / E=0 | no | a zero expiry is already expired at 0    |
        let guard = |expires_ms| {
            CommitEpochGuard::new(
                (),
                crate::dispatch::tests::a_request(),
                expires_ms,
                false,
                crate::PhysicalAttemptSlot::Empty,
            )
        };
        let lease = guard(100);
        assert!(lease.is_live_at(99), "E - 1");
        assert!(!lease.is_live_at(100), "E");
        assert!(!lease.is_live_at(101), "E + 1");
        assert!(!guard(0).is_live_at(0), "E = 0");
        assert!(
            guard(u64::MAX).is_live_at(u64::MAX - 1),
            "E = u64::MAX, one before"
        );
    }

    #[test]
    fn completion_decision_table_requires_release_success() {
        // Cause/effect table: C1 the inner commit has settled (the receiver
        // typestate); C2 release succeeds/fails. R1 C1+C2 => expose the inner
        // result. R2 C1+!C2 => fail release. A pre-C1 completion call is not
        // representable, so the compile-time constraint owns that rule.
        let settled = CommitEpochBoundary::held().settle_inner();
        assert_eq!(
            settled.complete(true),
            CommitEpochCompletion::ReturnInner,
            "R1"
        );
        assert_eq!(
            settled.complete(false),
            CommitEpochCompletion::FailRelease,
            "R2"
        );
    }

    #[test]
    fn guard_admission_requires_one_complete_exact_claim_snapshot() {
        // Cause/effect graph: C1 row is in the required leased state; C2 owner
        // exists; C3 expiry exists; C4 owner matches; C5 epoch matches. Effects:
        // E1 exact complete claim obtains a guard; E2 absent/different claim is
        // fenced; E3 incomplete leased authority is corruption. Decision rules:
        // G1=C1+C2+C3+C4+C5=>E1; G2=!C1 or C1+C2+C3+(!C4 or !C5)=>E2;
        // G3=C1+(!C2 or !C3)=>E3. Backends own only snapshot locking/decoding.
        assert_eq!(
            classify_commit_epoch_admission(true, true, true, true, true),
            CommitEpochAdmission::Guarded,
            "G1/E1"
        );
        assert_eq!(
            classify_commit_epoch_admission(false, true, true, true, true),
            CommitEpochAdmission::Fenced,
            "G2/E2 absent/wrong state"
        );
        assert_eq!(
            classify_commit_epoch_admission(true, true, true, true, false),
            CommitEpochAdmission::Fenced,
            "G2/E2 stale epoch"
        );
        assert_eq!(
            classify_commit_epoch_admission(true, true, false, true, true),
            CommitEpochAdmission::Corrupt,
            "G3/E3"
        );
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive bool(2) = 2
    /// @obligation ingress.commit_epoch_atomic_local
    /// @obligation ingress.commit_epoch_atomic_remote_custom
    /// @obligation remote_worker.stale_epoch_never_mutates_truth
    #[test]
    fn commit_epoch_completion_requires_observed_release() {
        // Cause/effect decision table:
        // C1 inner outcome has settled (enforced structurally by typestate);
        // C2 explicit backend release succeeds/fails. E1 exposes the already
        // settled inner outcome only for C2=true; E2 returns a release failure for
        // C2=false. R1/R2 exhaust Boolean C2, while C1=false cannot construct the
        // settled receiver and therefore has no completion transition.
        for release_succeeded in [false, true] {
            let decision = CommitEpochBoundary::held()
                .settle_inner()
                .complete(release_succeeded);
            assert_eq!(
                decision == CommitEpochCompletion::ReturnInner,
                release_succeeded
            );
        }
    }

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) x bool(2) = 32
    /// @obligation ingress.commit_epoch_atomic_remote_custom
    /// @obligation remote_worker.committed_truth_is_coordinator_owned
    /// @obligation remote_worker.execution_requires_claimed_snapshot
    /// @obligation remote_worker.single_live_claim_owner
    /// @obligation remote_worker.stale_epoch_never_mutates_truth
    #[test]
    fn commit_epoch_guard_admits_only_complete_exact_claim() {
        for row_in_required_state in [false, true] {
            for owner_present in [false, true] {
                for expiry_present in [false, true] {
                    for owner_matches in [false, true] {
                        for epoch_matches in [false, true] {
                            let admission = classify_commit_epoch_admission(
                                row_in_required_state,
                                owner_present,
                                expiry_present,
                                owner_matches,
                                epoch_matches,
                            );
                            assert_eq!(
                                admission == CommitEpochAdmission::Guarded,
                                row_in_required_state
                                    && owner_present
                                    && expiry_present
                                    && owner_matches
                                    && epoch_matches
                            );
                            assert_eq!(
                                admission == CommitEpochAdmission::Corrupt,
                                row_in_required_state && (!owner_present || !expiry_present)
                            );
                        }
                    }
                }
            }
        }
    }
}
