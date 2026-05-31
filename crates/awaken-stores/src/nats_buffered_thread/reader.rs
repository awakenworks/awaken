//! Read path: DB + WAL overlay for read-your-writes consistency.

use std::collections::{HashMap, HashSet};

use awaken_server_contract::contract::message::{Message, strip_unpaired_tool_calls_from_view};
use awaken_server_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, StorageError, ThreadPage, ThreadQuery, ThreadRunStore,
    paginate_threads,
};
use awaken_server_contract::thread::{Thread, normalize_lineage_id};

use super::{NatsBufferedThreadStore, config::ReadConsistency, entry, hot_meta, recovery};

pub async fn load_thread<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<Thread>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.load_thread(thread_id).await,
        ReadConsistency::Strong => {
            store.force_flush(thread_id).await?;
            return store.inner.load_thread(thread_id).await;
        }
        ReadConsistency::ReadYourWrites => load_thread_overlay(store, thread_id).await,
    }
}

pub async fn list_threads<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    offset: usize,
    limit: usize,
) -> Result<Vec<String>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.list_threads(offset, limit).await,
        ReadConsistency::Strong => {
            store.force_flush_all_pending().await?;
            return store.inner.list_threads(offset, limit).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    recovery::reconcile_all_thread_tails(store).await?;
    let all_inner_ids = list_all_inner_thread_ids(store).await?;
    let mut ids: HashSet<String> = all_inner_ids.into_iter().collect();
    ids.extend(hot_meta::pending_thread_ids(&store.kv_hot).await?);

    let mut threads = Vec::new();
    for id in ids {
        if let Some(thread) = load_thread_overlay(store, &id).await? {
            threads.push(thread);
        }
    }
    threads.sort_by(|a, b| {
        let a_updated = a.metadata.updated_at.or(a.metadata.created_at).unwrap_or(0);
        let b_updated = b.metadata.updated_at.or(b.metadata.created_at).unwrap_or(0);
        b_updated.cmp(&a_updated).then_with(|| a.id.cmp(&b.id))
    });
    Ok(threads
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|thread| thread.id)
        .collect())
}

pub async fn list_threads_query<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    query: &ThreadQuery,
) -> Result<ThreadPage, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.list_threads_query(query).await,
        ReadConsistency::Strong => {
            store.force_flush_all_pending().await?;
            return store.inner.list_threads_query(query).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    recovery::reconcile_all_thread_tails(store).await?;
    let all_inner_ids = list_all_inner_thread_ids(store).await?;
    let mut ids: HashSet<String> = all_inner_ids.into_iter().collect();
    ids.extend(hot_meta::pending_thread_ids(&store.kv_hot).await?);

    let mut threads = Vec::new();
    for id in ids {
        if let Some(thread) = load_thread_overlay(store, &id).await? {
            threads.push(thread);
        }
    }
    Ok(paginate_threads(threads, query))
}

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

    recovery::reconcile_thread_tail(store, thread_id).await?;
    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq <= flushed_seq {
        return store.inner.load_messages(thread_id).await;
    }

    if let Some(latest_entry) = read_committed_wal_entry(store, &meta).await? {
        let mut messages = latest_entry.messages;
        strip_unpaired_tool_calls_from_view(&mut messages);
        return Ok(Some(messages));
    }

    store.inner.load_messages(thread_id).await
}

/// Raw committed log (no read-time view filter), mirroring [`load_messages`]
/// but delegating to the inner store's `load_committed_messages` and skipping
/// `strip_unpaired_tool_calls_*` so the append version guard / seq see the
/// durable log.
pub async fn load_committed_messages<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<Vec<Message>>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.load_committed_messages(thread_id).await,
        ReadConsistency::Strong => {
            store.force_flush(thread_id).await?;
            return store.inner.load_committed_messages(thread_id).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    recovery::reconcile_thread_tail(store, thread_id).await?;
    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq <= flushed_seq {
        return store.inner.load_committed_messages(thread_id).await;
    }

    if let Some(latest_entry) = read_committed_wal_entry(store, &meta).await? {
        return Ok(Some(latest_entry.messages));
    }

    store.inner.load_committed_messages(thread_id).await
}

async fn list_all_inner_thread_ids<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
) -> Result<Vec<String>, StorageError> {
    const SCAN_LIMIT: usize = 200;
    let mut offset = 0;
    let mut all = Vec::new();
    loop {
        let page = store.inner.list_threads(offset, SCAN_LIMIT).await?;
        if page.is_empty() {
            break;
        }
        let count = page.len();
        all.extend(page);
        if count < SCAN_LIMIT {
            break;
        }
        offset += count;
    }
    Ok(all)
}

pub async fn load_run<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    run_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.load_run(run_id).await,
        ReadConsistency::Strong => {
            if let Some((run, cached_seq)) =
                hot_meta::load_cached_run_with_seq(&store.kv_hot, run_id).await?
            {
                store.force_flush(&run.thread_id).await?;
                if let Some(inner_run) = store.inner.load_run(run_id).await? {
                    return Ok(Some(inner_run));
                }
                let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, &run.thread_id).await?;
                if flushed_seq >= cached_seq {
                    return Ok(Some(run));
                }
                return Ok(None);
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

    recovery::reconcile_thread_tail(store, thread_id).await?;
    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq > flushed_seq
        && let Some(latest_entry) = read_committed_wal_entry(store, &meta).await?
    {
        return Ok(Some(latest_entry.run));
    }
    latest_inner_run(store, thread_id).await
}

pub async fn list_runs<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    query: &RunQuery,
) -> Result<RunPage, StorageError> {
    match store.config.read_consistency {
        ReadConsistency::Eventual => return store.inner.list_runs(query).await,
        ReadConsistency::Strong => {
            if let Some(thread_id) = query.thread_id.as_deref() {
                store.force_flush(thread_id).await?;
            } else {
                store.force_flush_all_pending().await?;
            }
            return store.inner.list_runs(query).await;
        }
        ReadConsistency::ReadYourWrites => {}
    }

    recovery::reconcile_all_thread_tails(store).await?;
    let mut by_id: HashMap<String, RunRecord> = list_all_inner_runs(store, query)
        .await?
        .into_iter()
        .map(|run| (run.run_id.clone(), run))
        .collect();

    for (run, _) in hot_meta::pending_cached_runs(&store.kv_hot).await? {
        if query
            .thread_id
            .as_deref()
            .is_some_and(|thread_id| run.thread_id != thread_id)
        {
            continue;
        }
        if query.status.is_some_and(|status| run.status != status) {
            continue;
        }
        if !query.matches_id_prefix(&run.thread_id) {
            continue;
        }
        by_id.insert(run.run_id.clone(), run);
    }

    let mut filtered: Vec<RunRecord> = by_id.into_values().collect();
    filtered.sort_by_key(|run| run.created_at);
    let total = filtered.len();
    let offset = query.offset.min(total);
    let limit = query.limit.clamp(1, 200);
    let items: Vec<RunRecord> = filtered.into_iter().skip(offset).take(limit).collect();
    let has_more = offset + items.len() < total;
    Ok(RunPage {
        items,
        total,
        has_more,
    })
}

async fn list_all_inner_runs<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    query: &RunQuery,
) -> Result<Vec<RunRecord>, StorageError> {
    let mut all = Vec::new();
    let mut offset = 0usize;
    loop {
        let mut page_query = query.clone();
        page_query.offset = offset;
        page_query.limit = 200;
        let page = store.inner.list_runs(&page_query).await?;
        let count = page.items.len();
        all.extend(page.items);
        if !page.has_more || count == 0 {
            break;
        }
        offset += count;
    }
    Ok(all)
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

pub(crate) async fn load_thread_overlay<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Option<Thread>, StorageError> {
    recovery::reconcile_thread_tail(store, thread_id).await?;
    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let flushed_seq = hot_meta::read_flushed_seq(&store.kv_hot, thread_id).await?;

    if meta.latest_seq <= flushed_seq {
        return store.inner.load_thread(thread_id).await;
    }

    let mut thread = store
        .inner
        .load_thread(thread_id)
        .await?
        .unwrap_or_else(|| Thread::with_id(thread_id));
    if let Some(latest_entry) = read_committed_wal_entry(store, &meta).await? {
        if let Some(projected_thread) = latest_entry.projected_thread.clone() {
            return Ok(Some(projected_thread));
        }
        thread
            .metadata
            .created_at
            .get_or_insert(latest_entry.written_at);
        thread.metadata.updated_at = Some(latest_entry.written_at);
        thread.apply_run_projection(&latest_entry.run);
        return Ok(Some(thread));
    }

    store.inner.load_thread(thread_id).await
}

pub(crate) async fn validate_thread_hierarchy_overlay<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
    parent_thread_id: Option<&str>,
) -> Result<(), StorageError> {
    let Some(parent_thread_id) = normalize_lineage_id(parent_thread_id) else {
        return Ok(());
    };
    if parent_thread_id == thread_id {
        return Err(StorageError::Validation(format!(
            "thread '{thread_id}' cannot parent itself"
        )));
    }

    let root_parent_thread_id = parent_thread_id.clone();
    let mut current_thread_id = parent_thread_id;
    let mut visited = std::collections::HashSet::from([thread_id.to_owned()]);

    loop {
        if !visited.insert(current_thread_id.clone()) {
            return Err(StorageError::Validation(format!(
                "thread hierarchy cycle detected at '{current_thread_id}'"
            )));
        }

        let Some(thread) = load_thread_overlay(store, &current_thread_id).await? else {
            let message = if current_thread_id == root_parent_thread_id {
                format!("parent thread not found: {root_parent_thread_id}")
            } else {
                format!("thread hierarchy references missing ancestor '{current_thread_id}'")
            };
            return Err(StorageError::Validation(message));
        };

        let Some(next_parent_thread_id) = normalize_lineage_id(thread.parent_thread_id.as_deref())
        else {
            return Ok(());
        };
        current_thread_id = next_parent_thread_id;
    }
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
