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
}

impl CommitEpochGuard {
    #[must_use]
    pub fn new(
        held: impl Send + 'static,
        request: RunDispatch,
        expires_ms: u64,
        cancellation_requested: bool,
    ) -> Self {
        Self {
            held: HeldAuthority::Drop(Box::new(held)),
            request,
            expires_ms,
            cancellation_requested,
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
    ) -> Self {
        Self {
            held: HeldAuthority::Release(Box::pin(release)),
            request,
            expires_ms,
            cancellation_requested,
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

    /// The authoritative dispatch payload protected by this exact claim guard.
    /// Recovery uses it to select the Thread without trusting a caller-supplied
    /// Thread id.
    #[must_use]
    pub fn request(&self) -> &RunDispatch {
        &self.request
    }

    /// Whether the guarded claim's lease is live at `now_ms`. The exact expiry
    /// boundary remains live, matching the queue's recovery rule.
    #[must_use]
    pub fn is_live_at(&self, now_ms: u64) -> bool {
        self.expires_ms >= now_ms
    }

    /// Inclusive expiry of the exact claim protected by this guard.
    #[must_use]
    pub fn expires_ms(&self) -> u64 {
        self.expires_ms
    }

    /// Whether the same locked dispatch row already requests cancellation.
    #[must_use]
    pub fn cancellation_requested(&self) -> bool {
        self.cancellation_requested
    }
}

#[cfg(kani)]
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

#[cfg(test)]
mod commit_epoch_tests {
    use super::*;

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
}
