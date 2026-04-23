//! Checkpoint write path: KV latest_seq CAS + hot run cache + JetStream publish.

use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    RunRecord, StorageError, ThreadRunStore, ThreadStore, checkpoint_parent_thread_id,
};

use super::{NatsBufferedThreadStore, entry, hot_meta, keys};

pub async fn checkpoint<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
    messages: &[Message],
    run: &RunRecord,
) -> Result<(), StorageError> {
    let existing_thread = store.load_thread(thread_id).await?;
    store
        .validate_thread_hierarchy(
            thread_id,
            checkpoint_parent_thread_id(existing_thread.as_ref(), run),
        )
        .await?;

    let now = now_millis();
    // Reserve a unique seq but don't let readers observe it yet — that
    // only happens after the WAL publish lands. If publish fails we
    // abandon the reservation (gap in `reserved_seq`, harmless to
    // readers which only consult `latest_seq`).
    let seq = hot_meta::reserve_seq(&store.kv_hot, thread_id, now).await?;

    let wal_entry = entry::CheckpointEntry {
        thread_id: thread_id.to_string(),
        run: run.clone(),
        messages: messages.to_vec(),
        thread_seq: seq,
        written_at: now,
    };
    let payload = entry::encode(&wal_entry)?;
    let publish_ack = store
        .jetstream
        .publish(keys::thread_subject(thread_id), payload)
        .await
        .map_err(|e| StorageError::Io(format!("publish: {e}")))?;
    let ack = publish_ack
        .await
        .map_err(|e| StorageError::Io(format!("publish ack: {e}")))?;
    let js_seq = ack.sequence;

    // WAL is durable. Promote hot state in commit order:
    // 1. Cache the new run so `load_run` hits the fresh copy.
    // 2. Raise `latest_seq` AND bind it to the JS stream seq of THIS
    //    WAL entry, so concurrent writers whose JS publish arrivals
    //    invert reservation order don't confuse readers.
    hot_meta::cache_run_if_newer(&store.kv_hot, run, seq).await?;
    hot_meta::promote_latest_seq(&store.kv_hot, thread_id, seq, js_seq, now).await?;

    Ok(())
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
