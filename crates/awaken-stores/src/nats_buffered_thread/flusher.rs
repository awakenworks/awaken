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
use futures::StreamExt;

use super::{config::NatsBufferedThreadConfig, entry, hot_meta};

/// Accumulator for one thread within a single flush batch.
struct ThreadBatch {
    latest_messages: Vec<Message>,
    latest_thread_seq: u64,
    /// One entry per unique run_id — latest version by thread_seq.
    runs_by_id: HashMap<String, (RunRecord, u64)>,
    msgs_to_ack: Vec<async_nats::jetstream::Message>,
}

pub fn spawn_flusher<T: ThreadRunStore + Send + Sync + 'static>(
    inner: Arc<T>,
    consumer: async_nats::jetstream::consumer::PullConsumer,
    kv_hot: async_nats::jetstream::kv::Store,
    config: NatsBufferedThreadConfig,
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

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            return;
                        }
                    }
                    _ = &mut window => break,
                    next = messages.next() => match next {
                        None => return,
                        Some(Err(e)) => {
                            tracing::warn!(error = %e, "flusher message stream error");
                        }
                        Some(Ok(msg)) => {
                            if let Some(decoded) = decode_or_ack(&msg).await {
                                merge_entry(&mut by_thread, decoded, msg);
                                if by_thread.len() >= config.flush_batch_size {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if !by_thread.is_empty() {
                flush_batch(&inner, &kv_hot, by_thread).await;
            }
        }
    })
}

async fn decode_or_ack(msg: &async_nats::jetstream::Message) -> Option<entry::CheckpointEntry> {
    match entry::decode(&msg.payload) {
        Ok(e) => Some(e),
        Err(err) => {
            tracing::warn!(error = %err, "poison entry; acking to skip");
            let _ = msg.ack().await;
            None
        }
    }
}

fn merge_entry(
    by_thread: &mut HashMap<String, ThreadBatch>,
    decoded: entry::CheckpointEntry,
    msg: async_nats::jetstream::Message,
) {
    let thread_id = decoded.thread_id.clone();
    let batch = by_thread.entry(thread_id).or_insert_with(|| ThreadBatch {
        latest_messages: Vec::new(),
        latest_thread_seq: 0,
        runs_by_id: HashMap::new(),
        msgs_to_ack: Vec::new(),
    });

    if decoded.thread_seq >= batch.latest_thread_seq {
        batch.latest_messages = decoded.messages;
        batch.latest_thread_seq = decoded.thread_seq;
    }

    let run_id = decoded.run.run_id.clone();
    match batch.runs_by_id.get(&run_id) {
        Some((_, existing_seq)) if *existing_seq >= decoded.thread_seq => {}
        _ => {
            batch
                .runs_by_id
                .insert(run_id, (decoded.run, decoded.thread_seq));
        }
    }

    batch.msgs_to_ack.push(msg);
}

/// Sort the per-run accumulator by ascending `thread_seq` so the final
/// `inner.checkpoint` application matches the highest-seq entry and the
/// thread projection converges to that run.
fn order_runs_for_flush(runs_by_id: HashMap<String, (RunRecord, u64)>) -> Vec<(RunRecord, u64)> {
    let mut ordered: Vec<(RunRecord, u64)> = runs_by_id.into_values().collect();
    ordered.sort_by_key(|(_, seq)| *seq);
    ordered
}

/// Apply a per-thread batch to `inner` in seq-ascending order. Returns
/// `true` on full success, `false` if any checkpoint errored (caller is
/// responsible for nacking the WAL messages in that case).
async fn apply_thread_batch_ordered<T: ThreadRunStore + Send + Sync>(
    inner: &T,
    thread_id: &str,
    messages: &[Message],
    ordered_runs: &[(RunRecord, u64)],
) -> bool {
    for (run, _) in ordered_runs {
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

async fn flush_batch<T: ThreadRunStore + Send + Sync + 'static>(
    inner: &Arc<T>,
    kv_hot: &async_nats::jetstream::kv::Store,
    by_thread: HashMap<String, ThreadBatch>,
) {
    for (thread_id, batch) in by_thread {
        let current_flushed = match hot_meta::read_flushed_seq(kv_hot, &thread_id).await {
            Ok(seq) => seq,
            Err(e) => {
                tracing::warn!(thread_id, error = %e, "read flushed_seq failed");
                for msg in batch.msgs_to_ack {
                    let _ = msg
                        .ack_with(async_nats::jetstream::AckKind::Nak(None))
                        .await;
                }
                continue;
            }
        };
        let ordered = order_runs_for_flush(batch.runs_by_id);
        let (stale_runs, fresh_runs): (Vec<_>, Vec<_>) = ordered
            .into_iter()
            .partition(|(_, seq)| *seq <= current_flushed);

        if batch.latest_thread_seq <= current_flushed {
            let stale_ok =
                persist_stale_runs_without_projection(inner.as_ref(), &thread_id, &stale_runs)
                    .await;
            if stale_ok {
                for msg in batch.msgs_to_ack {
                    let _ = msg.ack().await;
                }
            } else {
                for msg in batch.msgs_to_ack {
                    let _ = msg
                        .ack_with(async_nats::jetstream::AckKind::Nak(None))
                        .await;
                }
            }
            continue;
        }

        let stale_ok =
            persist_stale_runs_without_projection(inner.as_ref(), &thread_id, &stale_runs).await;
        let fresh_ok = if fresh_runs.is_empty() {
            true
        } else {
            apply_thread_batch_ordered(
                inner.as_ref(),
                &thread_id,
                &batch.latest_messages,
                &fresh_runs,
            )
            .await
        };
        let all_ok = stale_ok && fresh_ok;

        if all_ok {
            if let Err(e) =
                hot_meta::write_flushed_seq(kv_hot, &thread_id, batch.latest_thread_seq).await
            {
                tracing::warn!(thread_id, error = %e, "write flushed_seq failed");
            }
            for msg in batch.msgs_to_ack {
                let _ = msg.ack().await;
            }
        } else {
            for msg in batch.msgs_to_ack {
                let _ = msg
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await;
            }
        }
    }
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

        // Deliberately insert older into the HashMap last so HashMap
        // iteration could order it after the newer run.
        let mut map: HashMap<String, (RunRecord, u64)> = HashMap::new();
        map.insert("run-new".into(), (newer_run, 20));
        map.insert("run-old".into(), (older_run, 10));

        let ordered = order_runs_for_flush(map);
        let ok = apply_thread_batch_ordered(&inner, "t", &[], &ordered).await;
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
