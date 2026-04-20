//! Read-path operations.

use async_nats::jetstream::kv::Operation;
use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, codec, keys, ops_write};

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
        Some(entry) if matches!(entry.operation, Operation::Delete | Operation::Purge) => Ok(None),
        Some(entry) => Ok(Some(codec::decode(&entry.value)?)),
        None => Ok(None),
    }
}

pub(crate) async fn load_thread_dispatches(
    store: &NatsMailboxStore,
    thread_id: &str,
) -> Result<Vec<RunDispatch>, StorageError> {
    let Some(ids) = load_thread_index_ids(store, thread_id).await? else {
        let dispatches = load_all_dispatches(store)
            .await?
            .into_iter()
            .filter(|dispatch| dispatch.thread_id == thread_id)
            .collect::<Vec<_>>();
        for dispatch in &dispatches {
            ops_write::append_thread_index(store, thread_id, &dispatch.dispatch_id).await?;
        }
        return Ok(dispatches);
    };

    let mut dispatches = Vec::new();
    for dispatch_id in ids {
        let Some(dispatch) = load_dispatch(store, &dispatch_id).await? else {
            continue;
        };
        if dispatch.thread_id == thread_id {
            store.index.write().await.upsert(dispatch.clone());
            dispatches.push(dispatch);
        }
    }
    Ok(dispatches)
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
    entry
        .map(|entry| codec::decode_thread_index(&entry.value))
        .transpose()
}

pub(crate) async fn load_all_dispatches(
    store: &NatsMailboxStore,
) -> Result<Vec<RunDispatch>, StorageError> {
    use futures::StreamExt;

    let mut keys = store
        .kv_dispatch
        .keys()
        .await
        .map_err(|e| StorageError::Io(format!("kv dispatch keys: {e}")))?;
    let mut dispatches = Vec::new();
    while let Some(key_result) = keys.next().await {
        let key = key_result.map_err(|e| StorageError::Io(format!("kv dispatch key: {e}")))?;
        let Some(dispatch_id) = keys::dispatch_id_from_key(&key) else {
            continue;
        };
        let Some(dispatch) = load_dispatch(store, &dispatch_id).await? else {
            continue;
        };
        store.index.write().await.upsert(dispatch.clone());
        dispatches.push(dispatch);
    }
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
        a.priority
            .cmp(&b.priority)
            .then(a.created_at.cmp(&b.created_at))
    });
    Ok(items.into_iter().skip(offset).take(limit).collect())
}

pub async fn queued_thread_ids(store: &NatsMailboxStore) -> Result<Vec<String>, StorageError> {
    Ok(store.index.read().await.queued_thread_ids())
}
