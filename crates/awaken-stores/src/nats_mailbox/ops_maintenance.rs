//! Maintenance operations: lease reclaim, terminal GC.

use std::collections::HashSet;

use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::storage::StorageError;

use super::{
    NatsMailboxStore, claim_guard, codec, keys, kv_helpers, metrics, ops_query, ops_write,
};
use crate::mailbox_state;

pub async fn reclaim_expired_leases(
    store: &NatsMailboxStore,
    now: u64,
    limit: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    let mut candidates = match tokio::time::timeout(
        store.authoritative_scan_timeout,
        claim_guard::expired_dispatch_ids(store, now, limit),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(StorageError::Io(format!(
                "authoritative thread-claim scan timed out after {:?}",
                store.authoritative_scan_timeout
            )));
        }
    };
    if candidates.len() < limit {
        let mut seen = candidates.iter().cloned().collect::<HashSet<_>>();
        let remaining = limit - candidates.len();
        for dispatch_id in store
            .index
            .read()
            .await
            .expired_claimed_dispatch_ids(now, remaining)
        {
            if seen.insert(dispatch_id.clone()) {
                candidates.push(dispatch_id);
            }
        }
    }
    let mut reclaimed = Vec::new();
    for dispatch_id in candidates {
        if let Some(d) = reclaim_one(store, &dispatch_id, now).await? {
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
        if kv_helpers::is_tombstone(&entry) {
            return Ok(None);
        }
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status() != RunDispatchStatus::Claimed {
            return Ok(None);
        }
        if dispatch.lease_until().map(|u| u >= now).unwrap_or(true) {
            return Ok(None);
        }
        if let Some(lease_until) = dispatch.lease_until() {
            metrics::record_claimed_dispatch_lease_age(now, lease_until);
        }
        let old_claim_token = dispatch.claim_token().map(str::to_string);
        let thread_epoch = ops_write::current_thread_epoch(store, dispatch.thread_id()).await?;
        if dispatch.dispatch_epoch() < thread_epoch {
            mailbox_state::mark_superseded_at_epoch(
                &mut dispatch,
                now,
                thread_epoch,
                Some(mailbox_state::REASON_CLAIMED_LEASE_EXPIRED_AFTER_INTERRUPT),
            );
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
                    claim_guard::release(store, dispatch.thread_id(), dispatch_id, claim_token)
                        .await?;
                }
                if let Some(dedupe_key) = dispatch.dedupe_key() {
                    ops_write::release_dedupe_lock(
                        store,
                        dispatch.thread_id(),
                        dedupe_key,
                        dispatch.dispatch_id(),
                    )
                    .await;
                }
                ops_write::cleanup_thread_index(store, &dispatch).await;
                metrics::inc_expired_claim_reclaimed();
                return Ok(None);
            }
            continue;
        }
        mailbox_state::mark_expired_lease(&mut dispatch, now);
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
                claim_guard::release(store, dispatch.thread_id(), dispatch_id, claim_token).await?;
            }
            if dispatch.status() == RunDispatchStatus::DeadLetter
                && let Some(dedupe_key) = dispatch.dedupe_key()
            {
                ops_write::release_dedupe_lock(
                    store,
                    dispatch.thread_id(),
                    dedupe_key,
                    dispatch.dispatch_id(),
                )
                .await;
            }
            if dispatch.status() == RunDispatchStatus::DeadLetter {
                ops_write::cleanup_thread_index(store, &dispatch).await;
            }
            metrics::inc_expired_claim_reclaimed();
            return Ok(Some(dispatch));
        }
    }
    Ok(None)
}

pub async fn purge_terminal(
    store: &NatsMailboxStore,
    older_than: u64,
) -> Result<usize, StorageError> {
    let dispatches = ops_query::load_all_dispatches(store)
        .await?
        .into_iter()
        .filter(|dispatch| {
            dispatch.status().is_terminal()
                && dispatch
                    .completed_at()
                    .is_some_and(|completed_at| completed_at < older_than)
        })
        .collect::<Vec<_>>();
    let mut purged = 0;
    for dispatch in dispatches {
        if store
            .kv_dispatch
            .delete(&keys::dispatch_key(dispatch.dispatch_id()))
            .await
            .is_ok()
        {
            store.index.write().await.remove(dispatch.dispatch_id());
            ops_write::cleanup_thread_index(store, &dispatch).await;
            purged += 1;
        }
    }
    Ok(purged)
}
