//! Claim operations: claim (by thread), claim_dispatch (by id).

use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, claim_guard, codec, keys, ops_query, ops_write};

pub async fn claim_dispatch(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    consumer_id: &str,
    lease_ms: u64,
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

        if dispatch.status != RunDispatchStatus::Queued {
            return Ok(None);
        }
        // Note: `claim_dispatch` is the by-ID inline-claim path and
        // must NOT honor `available_at` — that field only guards
        // queue-scan claims (`claim()` by thread) so a foreground
        // submit can set a future `available_at` to keep the sweeper
        // away, then immediately claim the dispatch it just wrote.
        // Memory and SQLite backends already have this semantic.

        // Epoch check: reject dispatches whose recorded epoch is older
        // than the thread's authoritative epoch in KV. This is the
        // cross-node guarantee for `interrupt()` — a node whose local
        // index hasn't caught up may see a stale Queued dispatch; the
        // KV read here is strongly consistent and rejects it.
        let thread_epoch = read_thread_epoch(store, &dispatch.thread_id).await?;
        if dispatch.dispatch_epoch < thread_epoch {
            dispatch.status = RunDispatchStatus::Superseded;
            dispatch.dispatch_epoch = thread_epoch;
            dispatch.completed_at = Some(now);
            dispatch.updated_at = now;
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
            let bytes = codec::encode(&dispatch)?;
            if store
                .kv_dispatch
                .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
                .await
                .is_ok()
            {
                store.index.write().await.upsert(dispatch.clone());
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

        let Some(thread_claim) =
            claim_guard::acquire(store, &dispatch.thread_id, dispatch_id, lease_ms, now).await?
        else {
            return Ok(None);
        };

        dispatch.status = RunDispatchStatus::Claimed;
        dispatch.claim_token = Some(thread_claim.claim_token.clone());
        dispatch.claimed_by = Some(consumer_id.to_string());
        dispatch.lease_until = Some(thread_claim.lease_until);
        dispatch.updated_at = now;

        let bytes = codec::encode(&dispatch)?;
        let result = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await;
        match result {
            Ok(_) => {
                store.index.write().await.upsert(dispatch.clone());
                return Ok(Some(dispatch));
            }
            Err(_e) => {
                claim_guard::release(
                    store,
                    &dispatch.thread_id,
                    dispatch_id,
                    &thread_claim.claim_token,
                )
                .await?;
                // CAS conflict; retry.
                continue;
            }
        }
    }
    Ok(None)
}

pub async fn claim(
    store: &NatsMailboxStore,
    thread_id: &str,
    consumer_id: &str,
    lease_ms: u64,
    now: u64,
    limit: usize,
) -> Result<Vec<RunDispatch>, StorageError> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let mut candidates = ops_query::load_thread_dispatches(store, thread_id)
        .await?
        .into_iter()
        .filter(|dispatch| dispatch.status == RunDispatchStatus::Queued)
        .collect::<Vec<_>>();
    candidates.retain(|d| d.available_at <= now);
    candidates.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(a.created_at.cmp(&b.created_at))
    });

    let mut claimed = Vec::new();
    for candidate in candidates {
        if let Some(d) =
            claim_dispatch(store, &candidate.dispatch_id, consumer_id, lease_ms, now).await?
        {
            claimed.push(d);
            if claimed.len() >= limit {
                break;
            }
        }
    }
    Ok(claimed)
}

/// Read the thread's current dispatch epoch from KV. `None` → epoch 0
/// (no epoch record yet).
async fn read_thread_epoch(store: &NatsMailboxStore, thread_id: &str) -> Result<u64, StorageError> {
    let entry = store
        .kv_epoch
        .entry(&keys::epoch_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry epoch: {e}")))?;
    match entry {
        Some(e) => codec::decode_epoch(&e.value),
        None => Ok(0),
    }
}
