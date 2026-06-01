//! Claim operations: claim (by thread), claim_dispatch (by id).

use awaken_server_contract::contract::mailbox::{RunDispatch, RunDispatchStatus};
use awaken_server_contract::contract::storage::StorageError;

use super::{
    NatsMailboxStore, claim_guard, codec, keys, kv_helpers, metrics, ops_query, ops_write,
};
use crate::mailbox_state;

enum AvailableAtPolicy {
    Ignore,
    Respect,
}

// Small queues stay on the authoritative KV path so stale watcher indexes cannot
// reorder low-volume claims; large hot queues use the local index as a read cache.
const LOCAL_CLAIM_FAST_PATH_MIN_CANDIDATES: usize = 128;

async fn active_local_claim_blocks(
    store: &NatsMailboxStore,
    thread_id: &str,
    except_dispatch_id: Option<&str>,
    now: u64,
) -> Result<bool, StorageError> {
    let Some(local_active) = store
        .index
        .read()
        .await
        .active_claimed_for_thread(thread_id, now)
    else {
        return Ok(false);
    };
    if except_dispatch_id == Some(local_active.dispatch_id()) {
        return Ok(false);
    }

    let Some(authoritative) = ops_query::load_dispatch(store, local_active.dispatch_id()).await?
    else {
        store.index.write().await.remove(local_active.dispatch_id());
        return Ok(false);
    };
    store.index.write().await.upsert(authoritative.clone());
    Ok(authoritative.status() == RunDispatchStatus::Claimed
        && authoritative
            .lease_until()
            .is_some_and(|lease_until| lease_until >= now))
}

pub async fn claim_dispatch(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    consumer_id: &str,
    lease_ms: u64,
    now: u64,
) -> Result<Option<RunDispatch>, StorageError> {
    claim_dispatch_inner(
        store,
        dispatch_id,
        consumer_id,
        lease_ms,
        now,
        AvailableAtPolicy::Ignore,
    )
    .await
}

async fn claim_available_dispatch(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    consumer_id: &str,
    lease_ms: u64,
    now: u64,
) -> Result<Option<RunDispatch>, StorageError> {
    claim_dispatch_inner(
        store,
        dispatch_id,
        consumer_id,
        lease_ms,
        now,
        AvailableAtPolicy::Respect,
    )
    .await
}

async fn claim_dispatch_inner(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    consumer_id: &str,
    lease_ms: u64,
    now: u64,
    available_at_policy: AvailableAtPolicy,
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

        if dispatch.status() != RunDispatchStatus::Queued {
            return Ok(None);
        }
        if matches!(available_at_policy, AvailableAtPolicy::Respect)
            && dispatch.available_at() > now
        {
            return Ok(None);
        }

        // Epoch check: reject dispatches whose recorded epoch is older
        // than the thread's authoritative epoch in KV. This is the
        // cross-node guarantee for `interrupt()` — a node whose local
        // index hasn't caught up may see a stale Queued dispatch; the
        // KV read here is strongly consistent and rejects it.
        let thread_epoch = ops_write::current_thread_epoch(store, dispatch.thread_id()).await?;
        if dispatch.dispatch_epoch() < thread_epoch {
            mailbox_state::mark_superseded_at_epoch(&mut dispatch, now, thread_epoch, None);
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
                return Ok(None);
            }
            continue;
        }

        if active_local_claim_blocks(store, dispatch.thread_id(), Some(dispatch_id), now).await? {
            return Ok(None);
        }

        let Some(thread_claim) =
            claim_guard::acquire(store, dispatch.thread_id(), dispatch_id, lease_ms, now).await?
        else {
            return Ok(None);
        };

        dispatch.claim(
            consumer_id,
            thread_claim.claim_token.clone(),
            thread_claim.lease_until,
            now,
        )?;

        let bytes = codec::encode(&dispatch)?;
        let result = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await;
        match result {
            Ok(revision) => {
                store
                    .index
                    .write()
                    .await
                    .upsert_with_revision(dispatch.clone(), revision);
                return Ok(Some(dispatch));
            }
            Err(_e) => {
                claim_guard::release(
                    store,
                    dispatch.thread_id(),
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

    if claim_guard::active_dispatch_id(store, thread_id, now)
        .await?
        .is_some()
    {
        metrics::inc_claim_attempt("blocked");
        return Ok(Vec::new());
    }
    if active_local_claim_blocks(store, thread_id, None, now).await? {
        metrics::inc_claim_attempt("blocked");
        return Ok(Vec::new());
    }

    let (local_candidate_count, local_candidate) =
        ops_query::best_local_claim_candidate(store, thread_id, now).await;
    if local_candidate_count >= LOCAL_CLAIM_FAST_PATH_MIN_CANDIDATES
        && let Some(candidate) = local_candidate
    {
        match claim_available_dispatch(store, candidate.dispatch_id(), consumer_id, lease_ms, now)
            .await
        {
            Ok(Some(d)) => {
                metrics::inc_claim_attempt("claimed");
                return Ok(vec![d]);
            }
            Ok(None) => {}
            Err(error) => {
                metrics::inc_claim_attempt("error");
                return Err(error);
            }
        }
    }

    let mut candidates = match ops_query::load_claim_candidates(store, thread_id, now).await {
        Ok(dispatches) => dispatches,
        Err(error) => {
            metrics::inc_claim_attempt("error");
            return Err(error);
        }
    }
    .into_iter()
    .filter(|dispatch| dispatch.status() == RunDispatchStatus::Queued)
    .collect::<Vec<_>>();
    candidates.retain(|d| d.available_at() <= now);
    let available_candidates = candidates.len();
    candidates.sort_by(|a, b| {
        a.priority()
            .cmp(&b.priority())
            .then(a.created_at().cmp(&b.created_at()))
    });

    let mut claimed = Vec::new();
    for candidate in candidates {
        match claim_available_dispatch(store, candidate.dispatch_id(), consumer_id, lease_ms, now)
            .await
        {
            Ok(Some(d)) => {
                claimed.push(d);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                metrics::inc_claim_attempt("error");
                return Err(error);
            }
        }
    }
    let result = if !claimed.is_empty() {
        "claimed"
    } else if available_candidates > 0 {
        "blocked"
    } else {
        "no_eligible"
    };
    metrics::inc_claim_attempt(result);
    Ok(claimed)
}
