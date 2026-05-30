pub use awaken_runtime_contract::contract::mailbox::*;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_runtime_contract::contract::storage::StorageError;

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

// ── DispatchSignal ─────────────────────────────────────────────────────────

/// Receipt for a durable dispatch delivery signal.
///
/// Implementations should ack only after the scheduler has attempted to claim
/// the indicated thread. Nack requests redelivery when the scheduler cannot
/// safely make a claim decision.
#[async_trait]
pub trait DispatchSignalReceipt: Send + Sync {
    fn redelivery_attempts(&self) -> Option<u64> {
        None
    }

    async fn ack(self: Box<Self>) -> Result<(), StorageError>;
    async fn nack(self: Box<Self>) -> Result<(), StorageError>;
    async fn nack_with_delay(self: Box<Self>, delay: Duration) -> Result<(), StorageError> {
        let _ = delay;
        self.nack().await
    }
}

/// One durable dispatch delivery signal pulled from a backend work queue.
pub struct DispatchSignalEntry {
    pub thread_id: String,
    pub dispatch_id: String,
    pub receipt: Box<dyn DispatchSignalReceipt>,
}

impl std::fmt::Debug for DispatchSignalEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchSignalEntry")
            .field("thread_id", &self.thread_id)
            .field("dispatch_id", &self.dispatch_id)
            .finish_non_exhaustive()
    }
}

// ── MailboxStore trait ──────────────────────────────────────────────

/// Persistent mailbox queue with lease-based distributed claim.
///
/// Implementations must guarantee:
/// - enqueue is durable before returning
/// - claim is atomic (exactly one consumer wins)
/// - interrupt atomically bumps dispatch epoch + supersedes stale dispatches
/// - ack/nack/dead_letter validate claim_token (reject stale claims)
#[async_trait]
pub trait MailboxStore: Send + Sync {
    // ── write path ──

    /// Persist a dispatch. Sets dispatch epoch from current thread state
    /// (auto-creates state if first dispatch for this thread_id).
    /// Rejects if dedupe_key matches an existing non-terminal dispatch.
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError>;

    /// Atomically claim up to `limit` Queued dispatches for a thread
    /// where `available_at <= now`. Sets status=Claimed, claim_token,
    /// claimed_by, lease_until = now + lease_ms.
    /// Returns claimed dispatches ordered by (priority ASC, created_at ASC).
    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Claim a specific dispatch by dispatch_id. Same semantics as `claim()`
    /// but targets a single known dispatch (used for inline streaming).
    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError>;

    /// Mark mailbox delivery as consumed and no longer retryable.
    ///
    /// This validates `claim_token` and only records dispatch consumption. Use
    /// `record_run_result` for the agent run outcome.
    async fn ack(&self, dispatch_id: &str, claim_token: &str, now: u64)
    -> Result<(), StorageError>;

    /// Record the runtime dispatch identity for a claimed dispatch.
    ///
    /// Implementations should validate the claim token and set
    /// `run_status=Running`, while clearing any prior terminal result fields.
    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Record the runtime result for a claimed dispatch.
    ///
    /// This is intentionally separate from `ack`: `Acked` means the mailbox
    /// delivery was consumed, while these fields describe the agent run outcome.
    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Return dispatch to queue for retry. Sets available_at = retry_at,
    /// increments attempt_count, records error.
    /// If attempt_count >= max_attempts, transitions to DeadLetter instead.
    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Permanently fail a dispatch. Terminal state.
    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError>;

    /// Cancel a specific dispatch. Works on Queued dispatches only.
    /// For Claimed dispatches, caller must also cancel the runtime run.
    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError>;

    /// Extend an active lease. Returns false if dispatch not Claimed
    /// or claim_token mismatch (lease already expired and reclaimed).
    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError>;

    // ── interrupt ──

    /// Atomically: bump dispatch epoch, supersede stale Queued dispatches,
    /// return the Claimed dispatch (if any) so caller can cancel its runtime run.
    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError>;

    /// Detailed interrupt result including the exact queued dispatches that
    /// were superseded.
    ///
    /// The default delegates to the 0.2-compatible summary method. Stores that
    /// can return authoritative superseded records should override this method.
    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        self.interrupt(thread_id, now).await.map(Into::into)
    }

    /// Return the authoritative dispatch epoch for a thread.
    ///
    /// Implementations that do not persist epochs may keep the default `0`;
    /// production mailbox stores must override this so dispatch workers can
    /// reject claimed work that became stale after an interrupt.
    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        let _ = thread_id;
        Ok(0)
    }

    /// Terminalize a claimed dispatch as superseded.
    ///
    /// Used when an interrupt wins the race after a dispatch was claimed but
    /// before (or while) the runtime starts. Implementations must validate the
    /// claim token and clear lease/claim ownership. Returning `Ok(None)` means
    /// the dispatch is gone or no longer claimed.
    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        let _ = (dispatch_id, claim_token, now, reason);
        Err(StorageError::Io(
            "supersede claimed dispatch is not supported by this mailbox store".into(),
        ))
    }

    // ── read path ──

    /// Load a single dispatch by ID.
    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError>;

    /// List dispatches for a thread, filtered by status.
    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Count dispatches by status for low-cardinality operational gauges.
    ///
    /// Implementations that cannot provide an efficient count may return a
    /// storage error; callers must treat this as a metrics-only failure.
    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        let _ = status;
        Err(StorageError::Io(
            "count dispatches by status is not supported by this mailbox store".into(),
        ))
    }

    /// List terminal dispatches across all threads.
    ///
    /// Used by recovery/maintenance reconciliation to repair run lifecycle
    /// records after a process crashes between a mailbox terminal transition
    /// and the corresponding run-store checkpoint.
    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let _ = (limit, offset);
        Err(StorageError::Io(
            "list terminal dispatches is not supported by this mailbox store".into(),
        ))
    }

    // ── maintenance ──

    /// Reclaim dispatches whose lease_until < now (orphaned by crashed consumers).
    /// Resets to Queued with incremented attempt_count.
    /// Returns reclaimed dispatches for immediate execution.
    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError>;

    /// Purge terminal dispatches (Acked, Cancelled, Superseded, DeadLetter)
    /// older than `older_than` timestamp. Returns count purged.
    async fn purge_terminal(&self, older_than: u64) -> Result<usize, StorageError>;

    /// List distinct thread_ids that have at least one Queued dispatch.
    /// Used by recover() at startup.
    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError>;

    // ── dispatch signals (durable wakeups) ──

    /// Whether this store exposes durable dispatch delivery signals.
    fn supports_dispatch_signals(&self) -> bool {
        false
    }

    /// Pull durable dispatch delivery signals, if supported by the backend.
    ///
    /// The default is empty so non-work-queue stores continue relying on local
    /// submit, startup recovery, and sweep.
    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        let _ = (max, expires);
        Ok(Vec::new())
    }

    // ── live-channel (ephemeral steering) ──
    //
    // Separate from durable dispatch: these deliver best-effort control
    // commands to whichever node currently owns the run. Default impls are
    // no-ops so stores that don't support live delivery (test fakes) opt out.

    /// Deliver a `LiveRunCommand` to the run currently active for `thread_id`.
    /// Implementations report `Delivered` when at least one subscriber has
    /// observed the command, or `NoSubscriber` when delivery would be a
    /// silent drop (the caller then owns durable-fallback policy). The
    /// default implementation is `NoSubscriber` so stores that opt out of
    /// live delivery force every caller to fall back automatically.
    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        let _ = (thread_id, cmd);
        Ok(LiveDeliveryOutcome::NoSubscriber)
    }

    /// Deliver a live command to an exact run target.
    ///
    /// Backends with targeted live subjects should override this. The default
    /// preserves compatibility for stores that only support thread-level live
    /// routing.
    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.deliver_live(&target.thread_id, cmd).await
    }

    /// Subscribe to the live-command stream for `thread_id`. Called by the
    /// runtime on the owning node when a run is registered.
    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        let _ = thread_id;
        Ok(Box::pin(futures::stream::empty()))
    }

    /// Subscribe to the live-command stream for an exact run target.
    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.open_live_channel(&target.thread_id).await
    }
}

/// Adapter exposing any [`MailboxStore`] as a runtime [`LiveRunCommandSource`].
///
/// The runtime consumes live commands through `LiveRunCommandSource` (defined
/// in runtime-contract); the mailbox store is the durable source of those
/// commands. With `MailboxStore` now living in server-contract, a blanket
/// `impl<T: MailboxStore> LiveRunCommandSource for T` would violate the orphan
/// rule (foreign trait over a generic type), so this concrete wrapper provides
/// the bridge instead.
pub struct MailboxLiveControlSource(Arc<dyn MailboxStore>);

impl MailboxLiveControlSource {
    pub fn new(store: Arc<dyn MailboxStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl LiveRunCommandSource for MailboxLiveControlSource {
    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, LiveControlError> {
        MailboxStore::open_live_channel_for(self.0.as_ref(), target)
            .await
            .map_err(|error| LiveControlError::Subscribe(error.to_string()))
    }
}

#[derive(Clone)]
pub struct ScopedMailboxStore {
    inner: Arc<dyn MailboxStore>,
    scope_id: ScopeId,
}

impl ScopedMailboxStore {
    pub fn new(inner: Arc<dyn MailboxStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn MailboxStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_dispatch(&self, dispatch: &RunDispatch) -> RunDispatch {
        let mut dispatch = dispatch.clone();
        dispatch.dispatch_id = self.scoped(&dispatch.dispatch_id);
        dispatch.thread_id = self.scoped(&dispatch.thread_id);
        dispatch.run_id = self.scoped(&dispatch.run_id);
        dispatch.dedupe_key = dispatch.dedupe_key.as_deref().map(|key| self.scoped(key));
        dispatch
    }

    fn decode_dispatch(&self, mut dispatch: RunDispatch) -> Option<RunDispatch> {
        dispatch.dispatch_id = self.unscoped(&dispatch.dispatch_id)?.to_string();
        dispatch.thread_id = self.unscoped(&dispatch.thread_id)?.to_string();
        dispatch.run_id = self.unscoped(&dispatch.run_id)?.to_string();
        dispatch.dedupe_key = dispatch
            .dedupe_key
            .as_deref()
            .map(|key| self.unscoped(key).map(str::to_string))
            .unwrap_or(None);
        Some(dispatch)
    }

    fn encode_target(&self, target: &LiveRunTarget) -> LiveRunTarget {
        LiveRunTarget {
            thread_id: self.scoped(&target.thread_id),
            run_id: self.scoped(&target.run_id),
            dispatch_id: target.dispatch_id.as_deref().map(|id| self.scoped(id)),
        }
    }

    fn encode_result(&self, result: &RunDispatchResult) -> RunDispatchResult {
        let mut result = result.clone();
        result.run_id = self.scoped(&result.run_id);
        result
    }
}

#[async_trait]
impl MailboxStore for ScopedMailboxStore {
    async fn enqueue(&self, dispatch: &RunDispatch) -> Result<(), StorageError> {
        self.inner.enqueue(&self.encode_dispatch(dispatch)).await
    }

    async fn claim(
        &self,
        thread_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim(&self.scoped(thread_id), consumer_id, lease_ms, now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        consumer_id: &str,
        lease_ms: u64,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .claim_dispatch(&self.scoped(dispatch_id), consumer_id, lease_ms, now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn ack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .ack(&self.scoped(dispatch_id), claim_token, now)
            .await
    }

    async fn record_dispatch_start(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        dispatch_instance_id: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_dispatch_start(
                &self.scoped(dispatch_id),
                claim_token,
                dispatch_instance_id,
                now,
            )
            .await
    }

    async fn record_run_result(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        result: &RunDispatchResult,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .record_run_result(
                &self.scoped(dispatch_id),
                claim_token,
                &self.encode_result(result),
                now,
            )
            .await
    }

    async fn nack(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        retry_at: u64,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .nack(&self.scoped(dispatch_id), claim_token, retry_at, error, now)
            .await
    }

    async fn dead_letter(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        error: &str,
        now: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .dead_letter(&self.scoped(dispatch_id), claim_token, error, now)
            .await
    }

    async fn cancel(
        &self,
        dispatch_id: &str,
        now: u64,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .cancel(&self.scoped(dispatch_id), now)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn extend_lease(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        extension_ms: u64,
        now: u64,
    ) -> Result<bool, StorageError> {
        self.inner
            .extend_lease(&self.scoped(dispatch_id), claim_token, extension_ms, now)
            .await
    }

    async fn interrupt(&self, thread_id: &str, now: u64) -> Result<MailboxInterrupt, StorageError> {
        let interrupt = self.inner.interrupt(&self.scoped(thread_id), now).await?;
        Ok(MailboxInterrupt {
            new_dispatch_epoch: interrupt.new_dispatch_epoch,
            active_dispatch: interrupt
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: interrupt.superseded_count,
        })
    }

    async fn interrupt_detailed(
        &self,
        thread_id: &str,
        now: u64,
    ) -> Result<MailboxInterruptDetails, StorageError> {
        let details = self
            .inner
            .interrupt_detailed(&self.scoped(thread_id), now)
            .await?;
        let superseded_dispatches: Vec<_> = details
            .superseded_dispatches
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(MailboxInterruptDetails {
            new_dispatch_epoch: details.new_dispatch_epoch,
            active_dispatch: details
                .active_dispatch
                .and_then(|dispatch| self.decode_dispatch(dispatch)),
            superseded_count: superseded_dispatches.len(),
            superseded_dispatches,
        })
    }

    async fn current_dispatch_epoch(&self, thread_id: &str) -> Result<u64, StorageError> {
        self.inner
            .current_dispatch_epoch(&self.scoped(thread_id))
            .await
    }

    async fn supersede_claimed(
        &self,
        dispatch_id: &str,
        claim_token: &str,
        now: u64,
        reason: &str,
    ) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .supersede_claimed(&self.scoped(dispatch_id), claim_token, now, reason)
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn load_dispatch(&self, dispatch_id: &str) -> Result<Option<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .load_dispatch(&self.scoped(dispatch_id))
            .await?
            .and_then(|dispatch| self.decode_dispatch(dispatch)))
    }

    async fn list_dispatches(
        &self,
        thread_id: &str,
        status_filter: Option<&[RunDispatchStatus]>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .list_dispatches(&self.scoped(thread_id), status_filter, limit, offset)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn count_dispatches_by_status(
        &self,
        status: RunDispatchStatus,
    ) -> Result<usize, StorageError> {
        match status {
            RunDispatchStatus::Queued => {
                let mut total = 0;
                for thread_id in self.queued_thread_ids().await? {
                    total += self
                        .list_dispatches(
                            &thread_id,
                            Some(&[RunDispatchStatus::Queued]),
                            usize::MAX,
                            0,
                        )
                        .await?
                        .len();
                }
                Ok(total)
            }
            status if status.is_terminal() => Ok(self
                .list_terminal_dispatches(usize::MAX, 0)
                .await?
                .into_iter()
                .filter(|dispatch| dispatch.status == status)
                .count()),
            _ => Err(StorageError::Io(
                "scoped claimed dispatch count is not supported".into(),
            )),
        }
    }

    async fn list_terminal_dispatches(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        let all: Vec<_> = self
            .inner
            .list_terminal_dispatches(usize::MAX, 0)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect();
        Ok(all.into_iter().skip(offset).take(limit).collect())
    }

    async fn reclaim_expired_leases(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<RunDispatch>, StorageError> {
        Ok(self
            .inner
            .reclaim_expired_leases(now, limit)
            .await?
            .into_iter()
            .filter_map(|dispatch| self.decode_dispatch(dispatch))
            .collect())
    }

    async fn purge_terminal(&self, _older_than: u64) -> Result<usize, StorageError> {
        Err(StorageError::Io(
            "scoped terminal dispatch purge is not supported".into(),
        ))
    }

    async fn queued_thread_ids(&self) -> Result<Vec<String>, StorageError> {
        Ok(self
            .inner
            .queued_thread_ids()
            .await?
            .into_iter()
            .filter_map(|thread_id| self.unscoped(&thread_id).map(str::to_string))
            .collect())
    }

    fn supports_dispatch_signals(&self) -> bool {
        self.inner.supports_dispatch_signals()
    }

    async fn pull_dispatch_signals(
        &self,
        max: usize,
        expires: Duration,
    ) -> Result<Vec<DispatchSignalEntry>, StorageError> {
        Ok(self
            .inner
            .pull_dispatch_signals(max, expires)
            .await?
            .into_iter()
            .filter_map(|entry| {
                Some(DispatchSignalEntry {
                    thread_id: self.unscoped(&entry.thread_id)?.to_string(),
                    dispatch_id: self.unscoped(&entry.dispatch_id)?.to_string(),
                    receipt: entry.receipt,
                })
            })
            .collect())
    }

    async fn deliver_live(
        &self,
        thread_id: &str,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner.deliver_live(&self.scoped(thread_id), cmd).await
    }

    async fn deliver_live_to(
        &self,
        target: &LiveRunTarget,
        cmd: LiveRunCommand,
    ) -> Result<LiveDeliveryOutcome, StorageError> {
        self.inner
            .deliver_live_to(&self.encode_target(target), cmd)
            .await
    }

    async fn open_live_channel(
        &self,
        thread_id: &str,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner.open_live_channel(&self.scoped(thread_id)).await
    }

    async fn open_live_channel_for(
        &self,
        target: &LiveRunTarget,
    ) -> Result<LiveRunCommandStream, StorageError> {
        self.inner
            .open_live_channel_for(&self.encode_target(target))
            .await
    }
}
