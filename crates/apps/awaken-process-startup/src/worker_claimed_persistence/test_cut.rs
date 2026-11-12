// @formal surface exclude=test_only
// @formal owner crates/apps/awaken-process-startup/src/worker_claimed_persistence/gate.rs
// @formal reason: A per-instance one-shot poll cut exposes an existing final-admission boundary; it authorizes nothing.
//! Controlled cuts only for this crate's original actual-HTTP fixtures.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinalAdmissionCutKind {
    AbsentActivityReceipt,
    PreparedResumeReadValidation,
    PreparedResumeRootCommit,
}

pub(crate) struct FinalAdmissionTestCut {
    kind: FinalAdmissionCutKind,
    armed: AtomicBool,
    read_basis_moves: AtomicUsize,
    reached: Notify,
    proceed: Semaphore,
}
impl FinalAdmissionTestCut {
    pub(crate) fn new(kind: FinalAdmissionCutKind) -> Self {
        Self {
            kind,
            armed: AtomicBool::new(true),
            read_basis_moves: AtomicUsize::new(0),
            reached: Notify::new(),
            proceed: Semaphore::new(0),
        }
    }
    pub(super) async fn pause(&self, kind: FinalAdmissionCutKind) {
        if self.kind == kind && self.armed.swap(false, Ordering::AcqRel) {
            // This point is before BEGIN/claim locking. The concurrent real
            // request or takeover must retain the original cap1 connection.
            self.reached.notify_one();
            self.proceed
                .acquire()
                .await
                .expect("the fixture keeps its gate")
                .forget();
        }
    }
    pub(super) fn observe_claim_result(
        &self,
        result: &Result<(), awaken_session_application::SessionMutationError>,
    ) {
        if matches!(
            result,
            Err(awaken_session_application::SessionMutationError::Conflict)
        ) {
            self.read_basis_moves.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub(crate) fn observed_read_basis_moves(&self) -> usize {
        self.read_basis_moves.load(Ordering::SeqCst)
    }
    pub(crate) async fn reached(&self) {
        self.reached.notified().await;
    }
    pub(crate) fn release(&self) {
        self.proceed.add_permits(1);
    }
}
