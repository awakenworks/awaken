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

#[cfg(kani)]
/// @obligation ingress.commit_epoch_atomic_local
/// @obligation ingress.commit_epoch_atomic_remote_custom
/// @obligation remote_worker.stale_epoch_never_mutates_truth
#[kani::proof]
fn commit_epoch_completion_requires_observed_release() {
    // Cause/effect decision table:
    // C1 inner outcome has settled (enforced structurally by typestate);
    // C2 explicit backend release succeeds/fails. E1 exposes the already
    // settled inner outcome only for C2=true; E2 returns a release failure for
    // C2=false. R1/R2 exhaust Boolean C2, while C1=false cannot construct the
    // settled receiver and therefore has no completion transition.
    let release_succeeded: bool = kani::any();
    let decision = CommitEpochBoundary::held()
        .settle_inner()
        .complete(release_succeeded);
    assert_eq!(
        decision == CommitEpochCompletion::ReturnInner,
        release_succeeded
    );
}

#[cfg(kani)]
/// @obligation ingress.commit_epoch_atomic_remote_custom
/// @obligation remote_worker.committed_truth_is_coordinator_owned
/// @obligation remote_worker.execution_requires_claimed_snapshot
/// @obligation remote_worker.single_live_claim_owner
/// @obligation remote_worker.stale_epoch_never_mutates_truth
/// @proof-input-debt free_symbol classify_commit_epoch_admission epoch_matches
/// @proof-input-debt free_symbol classify_commit_epoch_admission expiry_present
/// @proof-input-debt free_symbol classify_commit_epoch_admission owner_matches
/// @proof-input-debt free_symbol classify_commit_epoch_admission owner_present
/// @proof-input-debt free_symbol classify_commit_epoch_admission row_in_required_state
#[kani::proof]
fn commit_epoch_guard_admits_only_complete_exact_claim() {
    let row_in_required_state: bool = kani::any();
    let owner_present: bool = kani::any();
    let expiry_present: bool = kani::any();
    let owner_matches: bool = kani::any();
    let epoch_matches: bool = kani::any();
    let admission = classify_commit_epoch_admission(
        row_in_required_state,
        owner_present,
        expiry_present,
        owner_matches,
        epoch_matches,
    );
    assert_eq!(
        admission == CommitEpochAdmission::Guarded,
        row_in_required_state && owner_present && expiry_present && owner_matches && epoch_matches
    );
    assert_eq!(
        admission == CommitEpochAdmission::Corrupt,
        row_in_required_state && (!owner_present || !expiry_present)
    );
}

#[cfg(test)]
mod commit_epoch_tests {
    use super::*;

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
