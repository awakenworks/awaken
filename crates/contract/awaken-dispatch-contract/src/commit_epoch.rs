/// Opaque backend guard which keeps a dispatch epoch stable until it is dropped.
/// PostgreSQL stores a row-locking transaction in it; SQLite stores its shared
/// single-process authority mutex guard. The ingress layer needs only the
/// lifetime, never the backend-specific value.
pub struct CommitEpochGuard {
    _held: Box<dyn Send>,
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
            _held: Box::new(held),
            request,
            expires_ms,
            cancellation_requested,
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
