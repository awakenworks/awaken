//! Maintenance operations: lease reclaim, terminal GC.

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, codec, keys};

pub async fn reclaim_expired_leases(
    store: &NatsMailboxStore,
    now: u64,
    limit: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    let candidates = store
        .index
        .read()
        .await
        .claimed_with_expired_lease(now, limit);
    let mut reclaimed = Vec::new();
    for candidate in candidates {
        if let Some(d) = reclaim_one(store, &candidate.dispatch_id, now).await? {
            reclaimed.push(d);
        }
    }
    Ok(reclaimed)
}

async fn reclaim_one(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    now: u64,
) -> Result<Option<RunDispatch>, StorageError> {
    for _ in 0..5 {
        let entry = store
            .kv_dispatch
            .entry(&keys::dispatch_key(dispatch_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status != RunDispatchStatus::Claimed {
            return Ok(None);
        }
        if dispatch.lease_until.map(|u| u >= now).unwrap_or(true) {
            return Ok(None);
        }
        dispatch.status = RunDispatchStatus::Queued;
        dispatch.claim_token = None;
        dispatch.claimed_by = None;
        dispatch.lease_until = None;
        dispatch.attempt_count += 1;
        dispatch.available_at = now;
        dispatch.updated_at = now;
        let bytes = codec::encode(&dispatch)?;
        if store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
            .is_ok()
        {
            store.index.write().await.upsert(dispatch.clone());
            return Ok(Some(dispatch));
        }
    }
    Ok(None)
}

pub async fn purge_terminal(
    store: &NatsMailboxStore,
    older_than: u64,
) -> Result<usize, StorageError> {
    let ids = store.index.read().await.terminal_older_than(older_than);
    let mut purged = 0;
    for id in ids {
        if store
            .kv_dispatch
            .delete(&keys::dispatch_key(&id))
            .await
            .is_ok()
        {
            store.index.write().await.remove(&id);
            purged += 1;
        }
    }
    Ok(purged)
}
