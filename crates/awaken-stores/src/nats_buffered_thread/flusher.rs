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

use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};
use awaken_contract::thread::Thread;
use futures::StreamExt;

use super::{config::NatsBufferedThreadConfig, entry, hierarchy_claim, hot_meta, keys, wal_state};

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

pub fn spawn_flusher<T: ThreadRunStore + Send + Sync + 'static>(
    inner: Arc<T>,
    consumer: async_nats::jetstream::consumer::PullConsumer,
    kv_hot: async_nats::jetstream::kv::Store,
    config: NatsBufferedThreadConfig,
    claim_options: hierarchy_claim::ClaimOptions,
    test_hooks: FlusherTestHooks,
    flush_notify: Arc<tokio::sync::Notify>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "flusher failed to open message stream");
                return;
            }
        };

        loop {
            let mut by_thread: HashMap<String, ThreadBatch> = HashMap::new();
            let window = tokio::time::sleep(config.flush_interval);
            tokio::pin!(window);
            let mut shutdown_requested = false;

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            shutdown_requested = true;
                            break;
                        }
                    }
                    _ = flush_notify.notified() => break,
                    _ = &mut window => break,
                    next = messages.next() => match next {
                        None => return,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "flusher message stream error");
                        }
                        Some(Ok(msg)) => {
                            if let Some(decoded) = decode_or_ack(msg).await {
                                merge_entry(&mut by_thread, decoded);
                                if by_thread.len() >= config.flush_batch_size {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if !by_thread.is_empty() {
                flush_batch(&inner, &kv_hot, &claim_options, &test_hooks, by_thread).await;
            }
            if shutdown_requested {
                return;
            }
        }
    })
}

async fn decode_or_ack(msg: async_nats::jetstream::Message) -> Option<BufferedWalEntry> {
    let stream_seq = match msg.info() {
        Ok(info) => info.stream_sequence,
        Err(error) => {
            tracing::warn!(%error, "WAL message missing JetStream metadata; acking to skip");
            let _ = msg.ack().await;
            return None;
        }
    };

    match entry::decode(&msg.payload) {
        Ok(checkpoint) => Some(BufferedWalEntry {
            checkpoint,
            stream_seq,
            msg,
        }),
        Err(err) => {
            tracing::warn!(error = %err, "poison entry; acking to skip");
            let _ = msg.ack().await;
            None
        }
    }
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

async fn materialize_projection<T: ThreadRunStore + Send + Sync>(
    inner: &T,
    kv_hot: &async_nats::jetstream::kv::Store,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    claim_token: Option<&str>,
    latest_thread_seq: u64,
    latest_messages: &[Message],
    latest_projected_thread: Option<&Thread>,
) -> bool {
    if let Some(claim_token) = claim_token
        && let Err(error) =
            ensure_flush_claim_current(kv_hot, test_hooks, thread_id, claim_token).await
    {
        tracing::warn!(
            thread_id,
            thread_seq = latest_thread_seq,
            error = %error,
            "flush claim expired before materializing thread projection"
        );
        return false;
    }
    if let Some(projected_thread) = latest_projected_thread
        && let Err(error) = inner.save_thread(projected_thread).await
    {
        tracing::warn!(
            thread_id,
            thread_seq = latest_thread_seq,
            error = %error,
            "inner save_thread for materialized WAL projection failed"
        );
        return false;
    }

    if let Some(claim_token) = claim_token
        && let Err(error) =
            ensure_flush_claim_current(kv_hot, test_hooks, thread_id, claim_token).await
    {
        tracing::warn!(
            thread_id,
            thread_seq = latest_thread_seq,
            error = %error,
            "flush claim expired before materializing messages"
        );
        return false;
    }
    if let Err(error) = inner.save_messages(thread_id, latest_messages).await {
        tracing::warn!(
            thread_id,
            thread_seq = latest_thread_seq,
            error = %error,
            "inner save_messages for materialized WAL projection failed"
        );
        return false;
    }

    true
}

async fn flush_committed_entries<T: ThreadRunStore + Send + Sync>(
    inner: &T,
    kv_hot: &async_nats::jetstream::kv::Store,
    test_hooks: &FlusherTestHooks,
    thread_id: &str,
    claim_token: Option<&str>,
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

    let stale_ok = persist_stale_runs_without_projection(inner, thread_id, &stale_runs).await;
    if !stale_ok {
        return false;
    }

    let has_fresh = latest_thread_seq > current_flushed;
    if has_fresh {
        if let Some(claim_token) = claim_token
            && let Err(error) =
                ensure_flush_claim_current(kv_hot, test_hooks, thread_id, claim_token).await
        {
            tracing::warn!(
                thread_id,
                thread_seq = latest_thread_seq,
                error = %error,
                "flush claim expired before materializing fresh WAL projection"
            );
            return false;
        }
        let fresh_ok = if fresh_runs.is_empty() {
            true
        } else {
            let claim = claim_token.map(|claim_token| FlushClaimContext {
                kv_hot,
                test_hooks,
                thread_id,
                claim_token,
            });
            apply_thread_batch_ordered(
                inner,
                claim.as_ref(),
                thread_id,
                &latest_messages,
                &fresh_runs,
            )
            .await
        };
        if !fresh_ok
            || !materialize_projection(
                inner,
                kv_hot,
                test_hooks,
                thread_id,
                claim_token,
                latest_thread_seq,
                &latest_messages,
                latest_projected_thread.as_ref(),
            )
            .await
        {
            return false;
        }
    }

    if latest_thread_seq > current_latest {
        if let Some(claim_token) = claim_token
            && let Err(error) =
                ensure_flush_claim_current(kv_hot, test_hooks, thread_id, claim_token).await
        {
            tracing::warn!(
                thread_id,
                thread_seq = latest_thread_seq,
                error = %error,
                "flush claim expired before promoting latest_seq"
            );
            return false;
        }
        if hot_meta::promote_latest_seq(
            kv_hot,
            thread_id,
            latest_thread_seq,
            latest_thread_js_seq,
            now_millis(),
        )
        .await
        .is_err()
        {
            tracing::warn!(
                thread_id,
                thread_seq = latest_thread_seq,
                "failed to promote latest_seq from committed WAL batch"
            );
            return false;
        }
    }

    if has_fresh {
        if let Some(claim_token) = claim_token
            && let Err(error) =
                ensure_flush_claim_current(kv_hot, test_hooks, thread_id, claim_token).await
        {
            tracing::warn!(
                thread_id,
                thread_seq = latest_thread_seq,
                error = %error,
                "flush claim expired before writing flushed_seq"
            );
            return false;
        }
        if let Err(error) = hot_meta::write_flushed_seq(kv_hot, thread_id, latest_thread_seq).await
        {
            tracing::warn!(
                thread_id,
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
        let committed_ok = flush_committed_entries(
            inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            Some(claim_token.as_str()),
            current_flushed,
            meta.latest_seq,
            &committed_entries,
        )
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

        let mut committed: Vec<CommittedFlushEntry> = entries
            .into_iter()
            .map(|(checkpoint, stream_seq)| CommittedFlushEntry {
                checkpoint,
                stream_seq,
            })
            .collect();
        committed.sort_by_key(|entry| entry.checkpoint.thread_seq);

        if flush_committed_entries(
            inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            Some(claim_token.as_str()),
            current_flushed,
            meta.latest_seq,
            &committed,
        )
        .await
        {
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

        if flush_committed_entries(
            inner.as_ref(),
            kv_hot,
            test_hooks,
            thread_id,
            Some(claim_token.as_str()),
            current_flushed,
            meta.latest_seq,
            &committed,
        )
        .await
        {
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

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryStore;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::ThreadStore;

    fn mk_run(run_id: &str, thread_id: &str) -> RunRecord {
        RunRecord {
            run_id: run_id.into(),
            thread_id: thread_id.into(),
            agent_id: "a".into(),
            parent_run_id: None,
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
}
