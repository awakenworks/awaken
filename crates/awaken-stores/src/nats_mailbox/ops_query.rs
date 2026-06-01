//! Read-path operations.

use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, codec, keys, kv_helpers, metrics};

pub async fn load_dispatch(
    store: &NatsMailboxStore,
    dispatch_id: &str,
) -> Result<Option<RunDispatch>, StorageError> {
    let entry = store
        .kv_dispatch
        .entry(&keys::dispatch_key(dispatch_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv dispatch entry: {e}")))?;
    match entry {
        Some(entry) if kv_helpers::is_tombstone(&entry) => Ok(None),
        Some(entry) => Ok(Some(codec::decode(&entry.value)?)),
        None => Ok(None),
    }
}

pub(crate) async fn load_thread_dispatches(
    store: &NatsMailboxStore,
    thread_id: &str,
) -> Result<Vec<RunDispatch>, StorageError> {
    let started = std::time::Instant::now();
    let Some(ids) = load_thread_index_ids(store, thread_id).await? else {
        metrics::record_claim_scan(0, started.elapsed());
        return Ok(Vec::new());
    };

    let id_count = ids.len();
    let mut dispatches = Vec::new();
    for dispatch_id in ids {
        let Some(dispatch) = load_dispatch(store, &dispatch_id).await? else {
            continue;
        };
        if dispatch.thread_id() == thread_id {
            store.index.write().await.upsert(dispatch.clone());
            dispatches.push(dispatch);
        }
    }
    metrics::record_claim_scan(id_count, started.elapsed());
    Ok(dispatches)
}

pub(crate) async fn load_claim_candidates(
    store: &NatsMailboxStore,
    thread_id: &str,
    now: u64,
) -> Result<Vec<RunDispatch>, StorageError> {
    let started = std::time::Instant::now();
    let Some(ids) = load_thread_index_ids(store, thread_id).await? else {
        metrics::record_claim_scan(0, started.elapsed());
        return Ok(Vec::new());
    };

    let id_count = ids.len();
    let mut dispatches = Vec::new();
    for dispatch_id in ids {
        let cached = store.index.read().await.get_cloned(&dispatch_id);
        if let Some(dispatch) = cached
            && dispatch.thread_id() == thread_id
            && dispatch.status() == RunDispatchStatus::Queued
            && dispatch.available_at() <= now
        {
            dispatches.push(dispatch);
            continue;
        }

        let Some(dispatch) = load_dispatch(store, &dispatch_id).await? else {
            continue;
        };
        if dispatch.thread_id() == thread_id {
            store.index.write().await.upsert(dispatch.clone());
            dispatches.push(dispatch);
        }
    }
    metrics::record_claim_scan(id_count, started.elapsed());
    Ok(dispatches)
}

pub(crate) async fn best_local_claim_candidate(
    store: &NatsMailboxStore,
    thread_id: &str,
    now: u64,
) -> (usize, Option<RunDispatch>) {
    store
        .index
        .read()
        .await
        .best_queued_for_thread(thread_id, now)
}

async fn load_thread_index_ids(
    store: &NatsMailboxStore,
    thread_id: &str,
) -> Result<Option<Vec<String>>, StorageError> {
    let entry = store
        .kv_thread_index
        .entry(&keys::thread_index_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("thread index entry: {e}")))?;
    match entry {
        Some(entry) if kv_helpers::is_tombstone(&entry) => Ok(None),
        Some(entry) => codec::decode_thread_index(&entry.value).map(Some),
        None => Ok(None),
    }
}

pub(crate) async fn load_all_dispatches(
    store: &NatsMailboxStore,
) -> Result<Vec<RunDispatch>, StorageError> {
    match tokio::time::timeout(
        store.authoritative_scan_timeout,
        load_all_dispatches_inner(store),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(StorageError::Io(format!(
            "authoritative dispatch scan timed out after {:?}",
            store.authoritative_scan_timeout
        ))),
    }
}

async fn load_all_dispatches_inner(
    store: &NatsMailboxStore,
) -> Result<Vec<RunDispatch>, StorageError> {
    use futures::StreamExt;

    let started = std::time::Instant::now();
    let mut keys = store
        .kv_dispatch
        .keys()
        .await
        .map_err(|e| StorageError::Io(format!("kv dispatch keys: {e}")))?;
    let mut dispatches = Vec::new();
    let mut key_count = 0usize;
    while let Some(key_result) = keys.next().await {
        let key = key_result.map_err(|e| StorageError::Io(format!("kv dispatch key: {e}")))?;
        key_count += 1;
        let Some(dispatch_id) = keys::dispatch_id_from_key(&key) else {
            continue;
        };
        let Some(dispatch) = load_dispatch(store, &dispatch_id).await? else {
            continue;
        };
        store.index.write().await.upsert(dispatch.clone());
        dispatches.push(dispatch);
    }
    metrics::record_authoritative_scan(key_count, started.elapsed());
    Ok(dispatches)
}

pub async fn list_dispatches(
    store: &NatsMailboxStore,
    thread_id: &str,
    status_filter: Option<&[RunDispatchStatus]>,
    limit: usize,
    offset: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    let mut items = store
        .index
        .read()
        .await
        .list_by_thread(thread_id, status_filter);
    items.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then(a.created_at().cmp(&b.created_at()))
    });
    Ok(items.into_iter().skip(offset).take(limit).collect())
}

pub async fn queued_thread_ids(store: &NatsMailboxStore) -> Result<Vec<String>, StorageError> {
    Ok(store.index.read().await.queued_thread_ids())
}

pub async fn count_dispatches_by_status(
    store: &NatsMailboxStore,
    status: RunDispatchStatus,
) -> Result<usize, StorageError> {
    Ok(store.index.read().await.count_by_status(status))
}

pub async fn list_terminal_dispatches(
    store: &NatsMailboxStore,
    limit: usize,
    offset: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    let mut dispatches = load_all_dispatches(store)
        .await?
        .into_iter()
        .filter(|dispatch| dispatch.status().is_terminal())
        .collect::<Vec<_>>();
    dispatches.sort_by(|a, b| {
        a.updated_at()
            .cmp(&b.updated_at())
            .then(a.created_at().cmp(&b.created_at()))
            .then(a.dispatch_id().cmp(b.dispatch_id()))
    });
    Ok(dispatches.into_iter().skip(offset).take(limit).collect())
}
