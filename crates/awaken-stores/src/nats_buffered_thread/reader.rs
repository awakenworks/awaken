//! Read path: DB + WAL overlay for read-your-writes consistency.

use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError, ThreadRunStore};

use super::{NatsBufferedThreadStore, config::ReadConsistency, entry, hot_meta};

pub async fn load_messages<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<Vec<Message>>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.load_messages(thread_id).await,
        ReadConsistency::Strong => {
            store.force_flush(thread_id).await?;
            return store.inner.load_messages(thread_id).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq <= flushed_seq {
        return store.inner.load_messages(thread_id).await;
    }

    if let Some(latest_entry) = read_committed_wal_entry(store, &meta).await? {
        return Ok(Some(latest_entry.messages));
    }

    store.inner.load_messages(thread_id).await
}

pub async fn load_run<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    run_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.load_run(run_id).await,
        ReadConsistency::Strong => {
            if let Some(run) = hot_meta::load_cached_run(&store.kv_hot, run_id).await? {
                store.force_flush(&run.thread_id).await?;
            }
            return store.inner.load_run(run_id).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }
    if let Some(run) = hot_meta::load_cached_run(&store.kv_hot, run_id).await? {
        return Ok(Some(run));
    }
    store.inner.load_run(run_id).await
}

pub async fn latest_run<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return latest_inner_run(store, thread_id).await,
        ReadConsistency::Strong => {
            store.force_flush(thread_id).await?;
            return latest_inner_run(store, thread_id).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq > flushed_seq
        && let Some(latest_entry) = read_committed_wal_entry(store, &meta).await?
    {
        return Ok(Some(latest_entry.run));
    }
    latest_inner_run(store, thread_id).await
}

async fn latest_inner_run<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    if let Some(thread) = store.inner.load_thread(thread_id).await?
        && let Some(run_id) = thread.latest_run_id
        && let Some(run) = store.inner.load_run(&run_id).await?
    {
        return Ok(Some(run));
    }
    store.inner.latest_run(thread_id).await
}

/// Fetch the WAL entry bound to `meta.latest_seq` via `meta.latest_js_seq`.
///
/// Directly addressing the JetStream stream sequence avoids the
/// concurrent-writer race where "last message by subject" reflects
/// publish-arrival order (which can invert reservation order). We also
/// validate that the decoded entry's `thread_seq` matches `latest_seq`;
/// a mismatch indicates lost bookkeeping and we conservatively fall
/// back to the inner store rather than return unverified data.
async fn read_committed_wal_entry<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    meta: &hot_meta::ThreadHotMetadata,
) -> Result<Option<entry::CheckpointEntry>, StorageError> {
    if meta.latest_js_seq == 0 {
        // Legacy hot-meta row written before this field existed.
        return Ok(None);
    }
    match store.stream.get_raw_message(meta.latest_js_seq).await {
        Ok(raw) => {
            let decoded = entry::decode(&raw.payload)?;
            if decoded.thread_seq != meta.latest_seq {
                tracing::warn!(
                    thread_seq = decoded.thread_seq,
                    latest_seq = meta.latest_seq,
                    latest_js_seq = meta.latest_js_seq,
                    "WAL entry at committed JS seq has mismatched thread_seq; falling back"
                );
                return Ok(None);
            }
            Ok(Some(decoded))
        }
        Err(_) => Ok(None),
    }
}
