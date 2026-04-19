//! Read-path operations served from in-memory index.

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::NatsMailboxStore;

pub async fn load_dispatch(
    store: &NatsMailboxStore,
    dispatch_id: &str,
) -> Result<Option<RunDispatch>, StorageError> {
    Ok(store.index.read().await.get(dispatch_id).cloned())
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
