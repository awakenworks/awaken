/// Opaque backend guard which keeps a dispatch epoch stable until it is dropped.
/// PostgreSQL stores a row-locking transaction in it; SQLite stores its shared
/// single-process authority mutex guard. The ingress layer needs only the
/// lifetime, never the backend-specific value.
use std::future::Future;
use std::pin::Pin;

type ReleaseFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

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
