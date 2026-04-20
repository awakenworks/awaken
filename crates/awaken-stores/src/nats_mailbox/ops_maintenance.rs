//! Maintenance operations: lease reclaim, terminal GC.

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, claim_guard, codec, keys, ops_query, ops_write};

pub async fn reclaim_expired_leases(
    store: &NatsMailboxStore,
    now: u64,
    limit: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    let candidates = ops_query::load_all_dispatches(store)
        .await?
        .into_iter()
        .filter(|dispatch| {
            dispatch.status == RunDispatchStatus::Claimed
                && dispatch.lease_until.is_some_and(|until| until < now)
        })
        .take(limit)
        .collect::<Vec<_>>();
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
        let old_claim_token = dispatch.claim_token.clone();
        let thread_epoch = ops_write::current_thread_epoch(store, &dispatch.thread_id).await?;
        if dispatch.dispatch_epoch < thread_epoch {
            dispatch.status = RunDispatchStatus::Superseded;
            dispatch.dispatch_epoch = thread_epoch;
            dispatch.last_error =
                Some("claimed dispatch lease expired after interrupt".to_string());
            dispatch.completed_at = Some(now);
            dispatch.updated_at = now;
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
            let bytes = codec::encode(&dispatch)?;
            if let Ok(revision) = store
                .kv_dispatch
                .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
                .await
            {
                store
                    .index
                    .write()
                    .await
                    .upsert_with_revision(dispatch.clone(), revision);
                if let Some(ref claim_token) = old_claim_token {
                    claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token)
                        .await?;
                }
                if let Some(ref dedupe_key) = dispatch.dedupe_key {
                    ops_write::release_dedupe_lock(
                        store,
                        &dispatch.thread_id,
                        dedupe_key,
                        &dispatch.dispatch_id,
                    )
                    .await;
                }
                return Ok(None);
            }
            continue;
        }
        dispatch.attempt_count = dispatch.attempt_count.saturating_add(1);
        dispatch.available_at = now;
        dispatch.updated_at = now;
        if dispatch.attempt_count >= dispatch.max_attempts {
            dispatch.status = RunDispatchStatus::DeadLetter;
            dispatch.last_error = Some("lease expired; max attempts reached".to_string());
            dispatch.completed_at = Some(now);
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
        } else {
            dispatch.status = RunDispatchStatus::Queued;
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
        }
        let bytes = codec::encode(&dispatch)?;
        if let Ok(revision) = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
        {
            store
                .index
                .write()
                .await
                .upsert_with_revision(dispatch.clone(), revision);
            if let Some(ref claim_token) = old_claim_token {
                claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
            }
            if dispatch.status == RunDispatchStatus::DeadLetter
                && let Some(ref dedupe_key) = dispatch.dedupe_key
            {
                ops_write::release_dedupe_lock(
                    store,
                    &dispatch.thread_id,
                    dedupe_key,
                    &dispatch.dispatch_id,
                )
                .await;
            }
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
