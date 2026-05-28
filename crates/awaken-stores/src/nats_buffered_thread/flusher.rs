//! Background flusher: drains JetStream WAL into the inner ThreadRunStore.
//!
//! Uses `consumer.messages()` for a long-lived message stream (with server-side
//! heartbeats) and batches within a `flush_interval` window to coalesce
//! per-thread writes.
//!
//! # Coalescing semantics
//!
//! Within a single batch, for each thread:
//! - Messages: the snapshot from the entry with the highest `thread_seq`
//!   (checkpoint semantic is full-overwrite, so the latest snapshot subsumes
//!   earlier ones).
//! - Run records: one per unique `run_id` (latest version by `thread_seq`).
//!   This preserves all distinct run records even when multiple runs complete
//!   within a single flush window.
use std::collections::HashMap;
use std::sync::Arc;

use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use awaken_server_contract::thread::Thread;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::{
    config::NatsBufferedThreadConfig, entry, hierarchy_claim, hot_meta, keys, metrics, wal_state,
};

#[derive(Debug, Clone, Default)]
pub(crate) struct FlusherTestHooks {
    after_read_flushed_pause: Arc<tokio::sync::Mutex<Option<PausePoint>>>,
    after_claim_check_pause: Arc<tokio::sync::Mutex<Option<PausePoint>>>,
}

#[derive(Debug, Clone)]
struct PausePoint {
    thread_id: String,
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl FlusherTestHooks {
    pub(crate) async fn set_pause_after_read_flushed(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.after_read_flushed_pause.lock().await = Some(PausePoint {
            thread_id: thread_id.to_string(),
            reached,
            release,
        });
    }

    async fn pause_after_read_flushed_if_configured(&self, thread_id: &str) {
        pause_if_configured(&self.after_read_flushed_pause, thread_id).await;
    }

    pub(crate) async fn set_pause_after_claim_check(
        &self,
        thread_id: &str,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.after_claim_check_pause.lock().await = Some(PausePoint {
            thread_id: thread_id.to_string(),
            reached,
            release,
        });
    }

    async fn pause_after_claim_check_if_configured(&self, thread_id: &str) {
        pause_if_configured(&self.after_claim_check_pause, thread_id).await;
    }
}

async fn pause_if_configured(slot: &tokio::sync::Mutex<Option<PausePoint>>, thread_id: &str) {
    let pause = {
        let mut slot = slot.lock().await;
        match slot.as_ref() {
            Some(pause) if pause.thread_id == thread_id => slot.take(),
            _ => None,
        }
    };
    let Some(pause) = pause else {
        return;
    };
    pause.reached.notify_waiters();
    pause.release.notified().await;
}

struct BufferedWalEntry {
    checkpoint: entry::CheckpointEntry,
    stream_seq: u64,
    msg: async_nats::jetstream::Message,
}

/// Accumulator for one thread within a single flush batch.
struct ThreadBatch {
    entries: Vec<BufferedWalEntry>,
}

#[derive(Clone)]
struct CommittedFlushEntry {
    checkpoint: entry::CheckpointEntry,
    stream_seq: u64,
}

struct AckableCommittedWalEntry {
    committed: CommittedFlushEntry,
    msg: async_nats::jetstream::Message,
}

struct FlushProjection {
    latest_messages: Vec<Message>,
    latest_thread_seq: u64,
    latest_thread_js_seq: u64,
    latest_projected_thread: Option<Thread>,
    runs_by_id: HashMap<String, (RunRecord, u64)>,
}

struct WalMessagePlan {
    msg: async_nats::jetstream::Message,
    action: WalAckAction,
    delete_state: Option<(String, u64)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalAckAction {
    Ack,
    Nak,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalEntryDecision {
    Committed,
    AckIgnore,
    Retry,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PoisonWalReason {
    MissingMetadata,
    DecodeFailed,
}

impl PoisonWalReason {
    fn metric_label(self) -> &'static str {
        match self {
            Self::MissingMetadata => "missing_metadata",
            Self::DecodeFailed => "decode_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoisonWalRecord {
    reason: PoisonWalReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream_seq: Option<u64>,
    subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    payload_len: usize,
    payload_preview_hex: String,
    quarantined_at: u64,
}

impl PoisonWalRecord {
    fn from_message(
        reason: PoisonWalReason,
        msg: &async_nats::jetstream::Message,
        stream_seq: Option<u64>,
        error: Option<String>,
    ) -> Self {
        Self {
            reason,
            stream_seq,
            subject: msg.subject.to_string(),
            thread_id: keys::thread_id_from_thread_subject(msg.subject.as_str()),
            error,
            payload_len: msg.payload.len(),
            payload_preview_hex: payload_preview_hex(&msg.payload),
            quarantined_at: now_millis(),
        }
    }
}

enum DecodeOutcome {
    Buffered(Box<BufferedWalEntry>),
    Quarantined(async_nats::jetstream::Message),
    Retry(async_nats::jetstream::Message),
}

pub(super) struct FlusherLoop<T: ThreadRunStore + Send + Sync + 'static> {
    pub(super) inner: Arc<T>,
    pub(super) consumer: async_nats::jetstream::consumer::PullConsumer,
    pub(super) kv_hot: async_nats::jetstream::kv::Store,
    pub(super) config: NatsBufferedThreadConfig,
    pub(super) claim_options: hierarchy_claim::ClaimOptions,
    pub(super) test_hooks: FlusherTestHooks,
    pub(super) flush_notify: Arc<tokio::sync::Notify>,
    pub(super) shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

pub fn spawn_flusher<T: ThreadRunStore + Send + Sync + 'static>(
    flusher: FlusherLoop<T>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(flusher.run())
}

impl<T: ThreadRunStore + Send + Sync + 'static> FlusherLoop<T> {
    async fn run(mut self) {
        let mut messages = match self.consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "flusher failed to open message stream");
                return;
            }
        };

        loop {
            let mut by_thread: HashMap<String, ThreadBatch> = HashMap::new();
            let window = tokio::time::sleep(self.config.flush_interval);
            tokio::pin!(window);
            let mut shutdown_requested = false;

            loop {
                tokio::select! {
                    _ = self.shutdown_rx.changed() => {
                        if *self.shutdown_rx.borrow() {
                            shutdown_requested = true;
                            break;
                        }
                    }
                    _ = self.flush_notify.notified() => break,
                    _ = &mut window => break,
                    next = messages.next() => match next {
                        None => return,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "flusher message stream error");
                        }
                        Some(Ok(msg)) => {
                            match self.decode_message(msg).await {
                                DecodeOutcome::Buffered(decoded) => {
                                    merge_entry(&mut by_thread, *decoded);
                                    if by_thread.len() >= self.config.flush_batch_size {
                                        break;
                                    }
                                }
                                DecodeOutcome::Quarantined(msg) => {
                                    finish_decode_outcome(msg, WalAckAction::Ack).await;
                                }
                                DecodeOutcome::Retry(msg) => {
                                    finish_decode_outcome(msg, WalAckAction::Nak).await;
                                }
                            }
                        }
                    }
                }
            }

            if !by_thread.is_empty() {
                flush_batch(
                    &self.inner,
                    &self.kv_hot,
                    &self.claim_options,
                    &self.test_hooks,
                    by_thread,
                )
                .await;
            }
            if shutdown_requested {
                return;
            }
        }
    }

    async fn decode_message(&self, msg: async_nats::jetstream::Message) -> DecodeOutcome {
        let stream_seq = match msg.info() {
            Ok(info) => info.stream_sequence,
            Err(error) => {
                return self
                    .quarantine_or_retry(
                        msg,
                        None,
                        PoisonWalReason::MissingMetadata,
                        Some(error.to_string()),
                    )
                    .await;
            }
        };

        match entry::decode(&msg.payload) {
            Ok(checkpoint) => DecodeOutcome::Buffered(
                BufferedWalEntry {
                    checkpoint,
                    stream_seq,
                    msg,
                }
                .into(),
            ),
            Err(error) => {
                self.quarantine_or_retry(
                    msg,
                    Some(stream_seq),
                    PoisonWalReason::DecodeFailed,
                    Some(error.to_string()),
                )
                .await
            }
        }
    }

    async fn quarantine_or_retry(
        &self,
        msg: async_nats::jetstream::Message,
        stream_seq: Option<u64>,
        reason: PoisonWalReason,
        error: Option<String>,
    ) -> DecodeOutcome {
        match quarantine_poison_wal(&self.kv_hot, &msg, stream_seq, reason, error).await {
            Ok(()) => {
                metrics::inc_poison_wal_quarantined(reason.metric_label());
                DecodeOutcome::Quarantined(msg)
            }
            Err(quarantine_error) => {
                metrics::inc_poison_wal_quarantine_failure(reason.metric_label());
                // When metadata is missing we cannot read `delivered`, so we
                // ack immediately to avoid an infinite Nak loop with no
                // progress signal.
                let delivered = msg.info().ok().map(|i| i.delivered).unwrap_or(i64::MAX);
                if should_drop_poison_on_quarantine_failure(delivered) {
                    metrics::inc_poison_wal_quarantine_dropped(reason.metric_label());
                    tracing::error!(
                        subject = msg.subject.as_str(),
                        stream_seq,
                        delivered,
                        reason = ?reason,
                        error = %quarantine_error,
                        "quarantine failed after max NAKs; acking to unblock consumer"
                    );
                    DecodeOutcome::Quarantined(msg)
                } else {
                    tracing::warn!(
                        subject = msg.subject.as_str(),
                        stream_seq,
                        delivered,
                        reason = ?reason,
                        error = %quarantine_error,
                        "failed to quarantine poison WAL message; requesting redelivery"
                    );
                    DecodeOutcome::Retry(msg)
                }
            }
        }
    }
}

const MAX_POISON_QUARANTINE_NAKS: i64 = 5;

// After this many redeliveries on the same poison message, ack it even
// when quarantine is still failing — otherwise a persistent KV outage
// would trap the consumer in an unbounded Nak loop on that message.
fn should_drop_poison_on_quarantine_failure(delivered: i64) -> bool {
    delivered >= MAX_POISON_QUARANTINE_NAKS
}

async fn finish_decode_outcome(msg: async_nats::jetstream::Message, action: WalAckAction) {
    let result = match action {
        WalAckAction::Ack => msg.ack().await,
        WalAckAction::Nak => {
            msg.ack_with(async_nats::jetstream::AckKind::Nak(None))
                .await
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, action = ?action, "failed to finish decoded WAL message");
    }
}

async fn quarantine_poison_wal(
    kv_hot: &async_nats::jetstream::kv::Store,
    msg: &async_nats::jetstream::Message,
    stream_seq: Option<u64>,
    reason: PoisonWalReason,
    error: Option<String>,
) -> Result<(), StorageError> {
    let record = PoisonWalRecord::from_message(reason, msg, stream_seq, error);
    let key = poison_wal_key(msg.subject.as_str(), &msg.payload, stream_seq);
    let payload = serde_json::to_vec(&record)
        .map_err(|e| StorageError::Serialization(format!("encode poison WAL record: {e}")))?;
    kv_hot
        .put(&key, payload.into())
        .await
        .map(|_| ())
        .map_err(|e| StorageError::Io(format!("put poison WAL quarantine {key}: {e}")))
}

fn poison_wal_key(subject: &str, payload: &[u8], stream_seq: Option<u64>) -> String {
    match stream_seq {
        Some(stream_seq) => keys::poison_wal_stream_key(stream_seq),
        None => keys::poison_wal_hash_key(hash_poison_wal(subject, payload)),
    }
}

// FNV-1a 64-bit. Defined inline so the output is stable across Rust
// toolchain upgrades — `std::collections::hash_map::DefaultHasher` is
// explicitly documented as not stable, which would cause the same
// malformed payload to land under different `poison.hash.*` keys after
// a compiler upgrade and silently break per-payload quarantine dedup.
fn hash_poison_wal(subject: &str, payload: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    for byte in subject.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Separator so "ab" + "cd" and "a" + "bcd" hash differently.
    hash ^= 0xff;
    hash = hash.wrapping_mul(FNV_PRIME);
    for byte in payload {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn payload_preview_hex(payload: &[u8]) -> String {
    const MAX_PREVIEW_BYTES: usize = 96;

    let mut preview = String::with_capacity(payload.len().min(MAX_PREVIEW_BYTES) * 2);
    for byte in payload.iter().take(MAX_PREVIEW_BYTES) {
        use std::fmt::Write as _;

        let _ = write!(&mut preview, "{byte:02x}");
    }
    preview
}

async fn finish_wal_messages(
    kv_hot: &async_nats::jetstream::kv::Store,
    messages: Vec<WalMessagePlan>,
) {
    for plan in messages {
        let result = match plan.action {
            WalAckAction::Ack => plan.msg.ack().await,
            WalAckAction::Nak => {
                plan.msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
            }
        };
        if let Err(error) = result {
            tracing::warn!(%error, action = ?plan.action, "failed to finish WAL message");
            continue;
        }

        if let Some((thread_id, thread_seq)) = plan.delete_state
            && let Err(error) = wal_state::delete_state(kv_hot, &thread_id, thread_seq).await
        {
            tracing::warn!(
                thread_id,
                thread_seq,
                error = %error,
                "failed to delete settled WAL state after ack"
            );
        }
    }
}

fn merge_entry(by_thread: &mut HashMap<String, ThreadBatch>, decoded: BufferedWalEntry) {
    let thread_id = decoded.checkpoint.thread_id.clone();
    let batch = by_thread.entry(thread_id).or_insert_with(|| ThreadBatch {
        entries: Vec::new(),
    });
    batch.entries.push(decoded);
}

/// Sort the per-run accumulator by ascending `thread_seq` so the final
/// `inner.checkpoint` application matches the highest-seq entry and the
/// thread projection converges to that run.
fn order_runs_for_flush(runs_by_id: HashMap<String, (RunRecord, u64)>) -> Vec<(RunRecord, u64)> {
    let mut ordered: Vec<(RunRecord, u64)> = runs_by_id.into_values().collect();
    ordered.sort_by_key(|(_, seq)| *seq);
    ordered
}

struct FlushClaimContext<'a> {
    kv_hot: &'a async_nats::jetstream::kv::Store,
    test_hooks: &'a FlusherTestHooks,
    thread_id: &'a str,
    claim_token: &'a str,
}

struct FlushExecutionContext<'a, T: ThreadRunStore + Send + Sync> {
    inner: &'a T,
    kv_hot: &'a async_nats::jetstream::kv::Store,
    test_hooks: &'a FlusherTestHooks,
    thread_id: &'a str,
    claim_token: Option<&'a str>,
}

impl<'a, T: ThreadRunStore + Send + Sync> FlushExecutionContext<'a, T> {
    async fn ensure_claim_current(&self, stage: &'static str, thread_seq: u64) -> bool {
        let Some(claim_token) = self.claim_token else {
            return true;
        };

        if let Err(error) =
            ensure_flush_claim_current(self.kv_hot, self.test_hooks, self.thread_id, claim_token)
                .await
        {
            tracing::warn!(
                thread_id = self.thread_id,
                thread_seq,
                stage,
                error = %error,
                "flush claim expired during WAL materialization"
            );
            return false;
        }

        true
    }

    async fn apply_thread_batch_ordered(
        &self,
        messages: &[Message],
        ordered_runs: &[(RunRecord, u64)],
    ) -> bool {
        let claim = self.claim_token.map(|claim_token| FlushClaimContext {
            kv_hot: self.kv_hot,
            test_hooks: self.test_hooks,
            thread_id: self.thread_id,
            claim_token,
        });
        apply_thread_batch_ordered(
            self.inner,
            claim.as_ref(),
            self.thread_id,
            messages,
            ordered_runs,
        )
        .await
    }

    async fn persist_stale_runs(&self, stale_runs: &[(RunRecord, u64)]) -> bool {
        persist_stale_runs_without_projection(self.inner, self.thread_id, stale_runs).await
    }

    async fn materialize_projection(
        &self,
        messages: &[Message],
        thread_seq: u64,
        projected_thread: Option<&Thread>,
    ) -> bool {
        if !self
            .ensure_claim_current("materialize_thread_projection", thread_seq)
            .await
        {
            return false;
        }
        if let Some(projected_thread) = projected_thread
            && let Err(error) = self.inner.save_thread(projected_thread).await
        {
            tracing::warn!(
                thread_id = self.thread_id,
                thread_seq,
                error = %error,
                "inner save_thread for materialized WAL projection failed"
            );
            return false;
        }

        if !self
            .ensure_claim_current("materialize_messages", thread_seq)
            .await
        {
            return false;
        }
        if let Err(error) = self.inner.save_messages(self.thread_id, messages).await {
            tracing::warn!(
                thread_id = self.thread_id,
                thread_seq,
                error = %error,
                "inner save_messages for materialized WAL projection failed"
            );
            return false;
        }

        true
    }
}

/// Apply a per-thread batch to `inner` in seq-ascending order. Returns
/// `true` on full success, `false` if any checkpoint errored (caller is
/// responsible for nacking the WAL messages in that case).
async fn apply_thread_batch_ordered<T: ThreadRunStore + Send + Sync>(
    inner: &T,
    claim: Option<&FlushClaimContext<'_>>,
    thread_id: &str,
    messages: &[Message],
    ordered_runs: &[(RunRecord, u64)],
) -> bool {
    for (run, _) in ordered_runs {
        if let Some(claim) = claim
            && let Err(error) = ensure_flush_claim_current(
                claim.kv_hot,
                claim.test_hooks,
                claim.thread_id,
                claim.claim_token,
            )
            .await
        {
            tracing::warn!(
                thread_id,
                run_id = %run.run_id,
                error = %error,
                "flush claim expired before inner checkpoint"
            );
            return false;
        }
        if let Err(e) = inner.checkpoint(thread_id, messages, run).await {
            tracing::warn!(thread_id, run_id = %run.run_id, error = %e, "inner checkpoint failed");
            return false;
        }
    }
    true
}

/// Persist stale run records without touching the thread/message projection.
async fn persist_stale_runs_without_projection<T: ThreadRunStore + Send + Sync>(
    inner: &T,
    thread_id: &str,
    stale_runs: &[(RunRecord, u64)],
) -> bool {
    for (run, _) in stale_runs {
        match inner.load_run(&run.run_id).await {
            Ok(Some(_)) => {}
            Ok(None) => match inner.create_run(run).await {
                Ok(()) | Err(StorageError::AlreadyExists(_)) => {}
                Err(e) => {
                    tracing::warn!(
                        thread_id,
                        run_id = %run.run_id,
                        error = %e,
                        "inner create_run for stale WAL entry failed"
                    );
                    return false;
                }
            },
            Err(e) => {
                tracing::warn!(
                    thread_id,
                    run_id = %run.run_id,
                    error = %e,
                    "inner load_run for stale WAL entry failed"
                );
                return false;
            }
        }
    }
    true
}

async fn classify_entry(
    kv_hot: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    current_flushed: u64,
    checkpoint: &entry::CheckpointEntry,
    stream_seq: u64,
) -> Result<WalEntryDecision, StorageError> {
    let state = match wal_state::load_state(kv_hot, thread_id, checkpoint.thread_seq).await? {
        Some(state) if state.status == wal_state::WalEntryStatus::Prepared => {
            wal_state::settle_thread_state(kv_hot, thread_id, checkpoint.thread_seq)
                .await?
                .or(Some(state))
        }
        state => state,
    };

    Ok(match state {
        Some(state) => match state.status {
            wal_state::WalEntryStatus::Committed if state.js_seq == Some(stream_seq) => {
                WalEntryDecision::Committed
            }
            wal_state::WalEntryStatus::Committed => WalEntryDecision::AckIgnore,
            wal_state::WalEntryStatus::Aborted => WalEntryDecision::AckIgnore,
            wal_state::WalEntryStatus::Prepared => WalEntryDecision::Retry,
        },
        None if checkpoint.thread_seq <= current_flushed => WalEntryDecision::AckIgnore,
        None => WalEntryDecision::Retry,
    })
}

fn build_flush_projection(committed: &[CommittedFlushEntry]) -> Option<FlushProjection> {
    let latest = committed
        .iter()
        .max_by_key(|entry| entry.checkpoint.thread_seq)?;
    let mut runs_by_id: HashMap<String, (RunRecord, u64)> = HashMap::new();
    for entry in committed {
        let run_id = entry.checkpoint.run.run_id.clone();
        match runs_by_id.get(&run_id) {
            Some((_, existing_seq)) if *existing_seq >= entry.checkpoint.thread_seq => {}
            _ => {
                runs_by_id.insert(
                    run_id,
                    (entry.checkpoint.run.clone(), entry.checkpoint.thread_seq),
                );
            }
        }
    }

    Some(FlushProjection {
        latest_messages: latest.checkpoint.messages.clone(),
        latest_thread_seq: latest.checkpoint.thread_seq,
        latest_thread_js_seq: latest.stream_seq,
        latest_projected_thread: latest.checkpoint.projected_thread.clone(),
        runs_by_id,
    })
}

async fn flush_committed_entries<T: ThreadRunStore + Send + Sync>(
    ctx: &FlushExecutionContext<'_, T>,
    current_flushed: u64,
    current_latest: u64,
    committed: &[CommittedFlushEntry],
) -> bool {
    let Some(projection) = build_flush_projection(committed) else {
        return true;
    };
    let FlushProjection {
        latest_messages,
        latest_thread_seq,
        latest_thread_js_seq,
        latest_projected_thread,
        runs_by_id,
    } = projection;
    let ordered = order_runs_for_flush(runs_by_id);
    let (stale_runs, fresh_runs): (Vec<_>, Vec<_>) = ordered
        .into_iter()
        .partition(|(_, seq)| *seq <= current_flushed);

    let stale_ok = ctx.persist_stale_runs(&stale_runs).await;
    if !stale_ok {
        return false;
    }

    let has_fresh = latest_thread_seq > current_flushed;
    if has_fresh {
        if !ctx
            .ensure_claim_current("materialize_fresh_projection", latest_thread_seq)
            .await
        {
            return false;
        }
        let fresh_ok = if fresh_runs.is_empty() {
            true
        } else {
            ctx.apply_thread_batch_ordered(&latest_messages, &fresh_runs)
                .await
        };
        if !fresh_ok {
            return false;
        }
        if !ctx
            .materialize_projection(
                &latest_messages,
                latest_thread_seq,
                latest_projected_thread.as_ref(),
            )
            .await
        {
            return false;
        }
    }

    if latest_thread_seq > current_latest {
        if !ctx
            .ensure_claim_current("promote_latest_seq", latest_thread_seq)
            .await
        {
            return false;
        }
        if hot_meta::promote_latest_seq(
            ctx.kv_hot,
            ctx.thread_id,
            latest_thread_seq,
            latest_thread_js_seq,
            now_millis(),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                thread_id = ctx.thread_id,
                thread_seq = latest_thread_seq,
                "failed to promote latest_seq from committed WAL batch"
            );
            return false;
        }
    }

    if has_fresh {
        if !ctx
            .ensure_claim_current("write_flushed_seq", latest_thread_seq)
            .await
        {
            return false;
        }
        if let Err(error) =
            hot_meta::write_flushed_seq(ctx.kv_hot, ctx.thread_id, latest_thread_seq).await
        {
            tracing::warn!(
                thread_id = ctx.thread_id,
                thread_seq = latest_thread_seq,
                error = %error,
                "write flushed_seq failed; nacking WAL batch for redelivery"
            );
            return false;
        }
    }

    true
}

async fn with_flush_claim<R, Fut>(
    kv_hot: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    claim_options: &hierarchy_claim::ClaimOptions,
    operation: impl FnOnce(String) -> Fut,
) -> Result<R, StorageError>
where
    Fut: std::future::Future<Output = Result<R, StorageError>>,
{
    let claim = hierarchy_claim::acquire_for_key(
        kv_hot,
        &keys::flush_lock_key(thread_id),
        "flush claim",
        claim_options,
    )
    .await?;
    let result = operation(claim.claim_token().to_string()).await;
    let release_result = hierarchy_claim::release(kv_hot, claim).await;
    match result {
        Ok(value) => {
            release_result?;
            Ok(value)
        }
        Err(error) => {
            if let Err(release_error) = release_result {
                tracing::warn!(
                    thread_id,
                    error = %release_error,
                    "failed to release flush claim after operation error"
                );
            }
            Err(error)
        }
    }
}

async fn ensure_flush_claim_current(
    kv_hot: &async_nats::jetstream::kv::Store,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    claim_token: &str,
) -> Result<(), StorageError> {
    if !hierarchy_claim::claim_token_is_current_for_key(
        kv_hot,
        &keys::flush_lock_key(thread_id),
        "flush claim",
        claim_token,
    )
    .await?
    {
        return Err(StorageError::Io(format!(
            "flush claim lost ownership for thread {thread_id}"
        )));
    }
    test_hooks
        .pause_after_claim_check_if_configured(thread_id)
        .await;
    if hierarchy_claim::claim_token_is_current_for_key(
        kv_hot,
        &keys::flush_lock_key(thread_id),
        "flush claim",
        claim_token,
    )
    .await?
    {
        Ok(())
    } else {
        Err(StorageError::Io(format!(
            "flush claim lost ownership for thread {thread_id}"
        )))
    }
}

async fn flush_thread_batch<T: ThreadRunStore + Send + Sync>(
    inner: &Arc<T>,
    kv_hot: &async_nats::jetstream::kv::Store,
    claim_options: &hierarchy_claim::ClaimOptions,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    batch: ThreadBatch,
) -> Result<(), StorageError> {
    with_flush_claim(kv_hot, thread_id, claim_options, |claim_token| async move {
        let meta = hot_meta::read_meta(kv_hot, thread_id).await?;
        let current_flushed = hot_meta::read_flushed_seq(kv_hot, thread_id).await?;
        test_hooks
            .pause_after_read_flushed_if_configured(thread_id)
            .await;
        let ctx = FlushExecutionContext {
            inner: inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            claim_token: Some(claim_token.as_str()),
        };

        let mut decisions = Vec::with_capacity(batch.entries.len());
        for entry in &batch.entries {
            decisions.push(
                classify_entry(
                    kv_hot,
                    thread_id,
                    current_flushed,
                    &entry.checkpoint,
                    entry.stream_seq,
                )
                .await?,
            );
        }

        let mut plans = Vec::new();
        let mut committed_entries = Vec::new();
        let mut committed_messages = Vec::new();
        for (entry, decision) in batch.entries.into_iter().zip(decisions) {
            match decision {
                WalEntryDecision::Committed => {
                    let committed = CommittedFlushEntry {
                        checkpoint: entry.checkpoint,
                        stream_seq: entry.stream_seq,
                    };
                    committed_entries.push(committed.clone());
                    committed_messages.push(AckableCommittedWalEntry {
                        committed,
                        msg: entry.msg,
                    });
                }
                WalEntryDecision::AckIgnore => plans.push(WalMessagePlan {
                    msg: entry.msg,
                    action: WalAckAction::Ack,
                    delete_state: Some((thread_id.to_string(), entry.checkpoint.thread_seq)),
                }),
                WalEntryDecision::Retry => plans.push(WalMessagePlan {
                    msg: entry.msg,
                    action: WalAckAction::Nak,
                    delete_state: None,
                }),
            }
        }

        committed_entries.sort_by_key(|entry| entry.checkpoint.thread_seq);
        let committed_ok =
            flush_committed_entries(&ctx, current_flushed, meta.latest_seq, &committed_entries)
                .await;
        let committed_action = if committed_ok {
            WalAckAction::Ack
        } else {
            WalAckAction::Nak
        };
        for entry in committed_messages {
            plans.push(WalMessagePlan {
                msg: entry.msg,
                action: committed_action,
                delete_state: (committed_action == WalAckAction::Ack)
                    .then_some((thread_id.to_string(), entry.committed.checkpoint.thread_seq)),
            });
        }
        finish_wal_messages(kv_hot, plans).await;
        Ok(())
    })
    .await
}

pub(crate) async fn flush_test_entries<T: ThreadRunStore + Send + Sync>(
    inner: &Arc<T>,
    kv_hot: &async_nats::jetstream::kv::Store,
    claim_options: &hierarchy_claim::ClaimOptions,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    entries: Vec<(entry::CheckpointEntry, u64)>,
) -> Result<(), StorageError> {
    with_flush_claim(kv_hot, thread_id, claim_options, |claim_token| async move {
        let meta = hot_meta::read_meta(kv_hot, thread_id).await?;
        let current_flushed = hot_meta::read_flushed_seq(kv_hot, thread_id).await?;
        test_hooks
            .pause_after_read_flushed_if_configured(thread_id)
            .await;
        let ctx = FlushExecutionContext {
            inner: inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            claim_token: Some(claim_token.as_str()),
        };

        let mut committed: Vec<CommittedFlushEntry> = entries
            .into_iter()
            .map(|(checkpoint, stream_seq)| CommittedFlushEntry {
                checkpoint,
                stream_seq,
            })
            .collect();
        committed.sort_by_key(|entry| entry.checkpoint.thread_seq);

        if flush_committed_entries(&ctx, current_flushed, meta.latest_seq, &committed).await {
            Ok(())
        } else {
            Err(StorageError::Io(format!(
                "test flusher failed for thread {thread_id}"
            )))
        }
    })
    .await
}

pub(crate) async fn process_test_entries<T: ThreadRunStore + Send + Sync>(
    inner: &Arc<T>,
    kv_hot: &async_nats::jetstream::kv::Store,
    claim_options: &hierarchy_claim::ClaimOptions,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    entries: Vec<(entry::CheckpointEntry, u64)>,
) -> Result<(), StorageError> {
    with_flush_claim(kv_hot, thread_id, claim_options, |claim_token| async move {
        let meta = hot_meta::read_meta(kv_hot, thread_id).await?;
        let current_flushed = hot_meta::read_flushed_seq(kv_hot, thread_id).await?;
        test_hooks
            .pause_after_read_flushed_if_configured(thread_id)
            .await;
        let ctx = FlushExecutionContext {
            inner: inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            claim_token: Some(claim_token.as_str()),
        };

        let mut committed = Vec::new();
        for (checkpoint, stream_seq) in entries {
            if classify_entry(kv_hot, thread_id, current_flushed, &checkpoint, stream_seq).await?
                == WalEntryDecision::Committed
            {
                committed.push(CommittedFlushEntry {
                    checkpoint,
                    stream_seq,
                });
            }
        }
        committed.sort_by_key(|entry| entry.checkpoint.thread_seq);

        if flush_committed_entries(&ctx, current_flushed, meta.latest_seq, &committed).await {
            Ok(())
        } else {
            Err(StorageError::Io(format!(
                "test flusher failed for thread {thread_id}"
            )))
        }
    })
    .await
}

async fn flush_batch<T: ThreadRunStore + Send + Sync + 'static>(
    inner: &Arc<T>,
    kv_hot: &async_nats::jetstream::kv::Store,
    claim_options: &hierarchy_claim::ClaimOptions,
    test_hooks: &FlusherTestHooks,
    by_thread: HashMap<String, ThreadBatch>,
) {
    for (thread_id, batch) in by_thread {
        if let Err(error) =
            flush_thread_batch(inner, kv_hot, claim_options, test_hooks, &thread_id, batch).await
        {
            tracing::warn!(thread_id, error = %error, "flush thread batch failed");
        }
    }
}

use super::recovery::now_millis;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryStore;
    use awaken_server_contract::contract::lifecycle::RunStatus;
    use awaken_server_contract::contract::storage::ThreadStore;

    fn mk_run(run_id: &str, thread_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            thread_id: thread_id.into(),
            agent_id: "a".into(),
            parent_run_id: None,
            registry_manifest: None,
            activation: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Created,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 1,
            started_at: None,
            finished_at: None,
            updated_at: 1,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    #[test]
    fn order_runs_for_flush_sorts_by_thread_seq() {
        let mut map: HashMap<String, (RunRecord, u64)> = HashMap::new();
        map.insert("a".into(), (mk_run("a", "t"), 42));
        map.insert("b".into(), (mk_run("b", "t"), 10));
        map.insert("c".into(), (mk_run("c", "t"), 99));
        let ordered = order_runs_for_flush(map);
        let seqs: Vec<u64> = ordered.iter().map(|(_, s)| *s).collect();
        assert_eq!(seqs, vec![10, 42, 99]);
    }

    /// Regression for issue #4: flushing a batch with multiple runs for the
    /// same thread must leave the thread projection pointing at the
    /// highest-seq run. `InMemoryStore::checkpoint` updates `latest_run_id`
    /// on each call — applying in HashMap order could land the projection
    /// on an older run.
    #[tokio::test]
    async fn flush_ordering_preserves_highest_seq_projection() {
        let inner = InMemoryStore::new();

        let older_run = mk_run("run-old", "t");
        let newer_run = mk_run("run-new", "t");

        let mut map: HashMap<String, (RunRecord, u64)> = HashMap::new();
        map.insert("run-new".into(), (newer_run, 20));
        map.insert("run-old".into(), (older_run, 10));

        let ordered = order_runs_for_flush(map);
        let ok = apply_thread_batch_ordered(&inner, None, "t", &[], &ordered).await;
        assert!(ok);

        let thread = ThreadStore::load_thread(&inner as &InMemoryStore, "t")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            thread.latest_run_id.as_deref(),
            Some("run-new"),
            "thread projection must point at highest-seq run"
        );
        let open = thread.open_run_id.as_deref();
        assert!(
            open == Some("run-new") || open.is_none(),
            "open_run_id must match highest-seq projection; got {open:?}"
        );
    }

    #[test]
    fn poison_wal_hash_key_is_stable_for_metadata_less_messages() {
        let subject = "thread.h7431";
        let payload = br#"broken"#;

        let first = poison_wal_key(subject, payload, None);
        let second = poison_wal_key(subject, payload, None);

        assert_eq!(first, second);
        assert!(first.starts_with("poison.hash."));
    }

    #[test]
    fn poison_wal_key_routes_by_presence_of_stream_seq() {
        let subject = "thread.h7431";
        let payload = br#"broken"#;

        assert_eq!(poison_wal_key(subject, payload, Some(42)), "poison.seq.42");

        let hash_key = poison_wal_key(subject, payload, None);
        assert!(hash_key.starts_with("poison.hash."));
        assert_ne!(poison_wal_key(subject, payload, Some(42)), hash_key);
    }

    #[test]
    fn poison_wal_hash_key_separates_distinct_payloads_and_subjects() {
        let a = poison_wal_key("thread.h7431", b"payload-a", None);
        let b = poison_wal_key("thread.h7431", b"payload-b", None);
        assert_ne!(
            a, b,
            "different payloads under the same subject must not share a quarantine slot"
        );

        let c = poison_wal_key("thread.h0000001", b"payload-a", None);
        let d = poison_wal_key("thread.h0000002", b"payload-a", None);
        assert_ne!(
            c, d,
            "same bytes on different subjects must not share a quarantine slot"
        );
    }

    // Pins the exact hash output for a known input. If this golden value
    // changes, the hash algorithm changed; any deployed KV already holding
    // `poison.hash.*` entries will silently dedupe into new slots and
    // accumulate duplicates. Bump the key prefix (e.g. `poison.hash.v2.`)
    // if you need to replace the algorithm, so old and new entries can
    // coexist instead of masking each other.
    #[test]
    fn poison_wal_hash_key_is_pinned_to_fnv1a_golden_value() {
        assert_eq!(
            poison_wal_key("thread.h7431", b"broken", None),
            "poison.hash.f0b96944d6ae33ac"
        );
    }

    #[test]
    fn payload_preview_hex_truncates_to_fixed_budget() {
        let payload = vec![0xab; 128];

        let preview = payload_preview_hex(&payload);

        assert_eq!(preview.len(), 96 * 2);
        assert!(preview.starts_with("abab"));
    }

    #[test]
    fn payload_preview_hex_does_not_pad_short_payloads() {
        let preview = payload_preview_hex(&[0x01, 0x02, 0x03]);
        assert_eq!(preview, "010203");
    }

    // Guards the invariant that a persistent KV outage cannot trap the
    // consumer in an unbounded Nak loop. First delivery gets a Nak chance;
    // after MAX_POISON_QUARANTINE_NAKS retries the flusher ack-drops.
    #[test]
    fn quarantine_nak_loop_is_bounded_by_delivered_count() {
        for delivered in 1..MAX_POISON_QUARANTINE_NAKS {
            assert!(
                !should_drop_poison_on_quarantine_failure(delivered),
                "delivery #{delivered} should still be retried"
            );
        }
        assert!(should_drop_poison_on_quarantine_failure(
            MAX_POISON_QUARANTINE_NAKS
        ));
        assert!(should_drop_poison_on_quarantine_failure(
            MAX_POISON_QUARANTINE_NAKS + 1
        ));
        // Missing JetStream metadata path: caller substitutes i64::MAX so
        // the first failure is acked immediately.
        assert!(should_drop_poison_on_quarantine_failure(i64::MAX));
    }
}
