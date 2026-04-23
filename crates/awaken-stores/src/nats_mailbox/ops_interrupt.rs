//! Interrupt + dispatch epoch management.

use awaken_contract::contract::mailbox::{MailboxInterruptDetails, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, claim_guard, codec, keys, kv_helpers, ops_query, ops_write};

enum SupersedeOutcome {
    Superseded(Box<awaken_contract::contract::mailbox::RunDispatch>),
    Claimed(Box<awaken_contract::contract::mailbox::RunDispatch>),
    NotQueued,
}

pub async fn interrupt(
    store: &NatsMailboxStore,
    thread_id: &str,
    now: u64,
) -> Result<MailboxInterruptDetails, StorageError> {
    let new_epoch = bump_epoch(store, thread_id).await?;

    let mut dispatches = ops_query::load_thread_dispatches(store, thread_id).await?;

    let mut superseded_count = 0usize;
    let mut superseded_dispatches = Vec::new();
    let mut active_dispatch = None;

    let guard_active_dispatch_id = claim_guard::active_dispatch_id(store, thread_id, now).await?;
    if let Some(active_dispatch_id) = guard_active_dispatch_id.as_deref()
        && let Some(authoritative) = ops_query::load_dispatch(store, active_dispatch_id).await?
        && authoritative.thread_id == thread_id
        && authoritative.status == RunDispatchStatus::Claimed
    {
        store.index.write().await.upsert(authoritative.clone());
        if !dispatches
            .iter()
            .any(|dispatch| dispatch.dispatch_id == authoritative.dispatch_id)
        {
            dispatches.push(authoritative.clone());
        }
        active_dispatch = Some(authoritative);
    }

    for dispatch in dispatches {
        match dispatch.status {
            RunDispatchStatus::Queued => {
                match supersede(store, &dispatch.dispatch_id, new_epoch, now).await {
                    Ok(SupersedeOutcome::Superseded(superseded)) => {
                        superseded_count += 1;
                        // Terminal → release any dedupe lock so a subsequent
                        // enqueue with the same key can take over.
                        if let Some(ref dedupe_key) = superseded.dedupe_key {
                            ops_write::release_dedupe_lock(
                                store,
                                &superseded.thread_id,
                                dedupe_key,
                                &superseded.dispatch_id,
                            )
                            .await;
                        }
                        superseded_dispatches.push(*superseded);
                    }
                    Ok(SupersedeOutcome::Claimed(authoritative)) => {
                        active_dispatch = Some(*authoritative);
                    }
                    Ok(SupersedeOutcome::NotQueued) => {}
                    Err(error) => {
                        tracing::warn!(
                            thread_id,
                            dispatch_id = %dispatch.dispatch_id,
                            error = %error,
                            "failed to supersede queued dispatch during interrupt"
                        );
                    }
                }
            }
            RunDispatchStatus::Claimed => {
                if guard_active_dispatch_id
                    .as_deref()
                    .is_none_or(|active_id| active_id == dispatch.dispatch_id)
                {
                    active_dispatch = Some(dispatch);
                }
            }
            _ => {}
        }
    }

    Ok(MailboxInterruptDetails {
        new_dispatch_epoch: new_epoch,
        active_dispatch,
        superseded_count,
        superseded_dispatches,
    })
}

async fn bump_epoch(store: &NatsMailboxStore, thread_id: &str) -> Result<u64, StorageError> {
    let key = keys::epoch_key(thread_id);
    for _ in 0..5 {
        let entry = store
            .kv_epoch
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let (current, revision) = match entry {
            Some(e) if kv_helpers::is_tombstone(&e) => (0u64, 0u64),
            Some(e) => (codec::decode_epoch(&e.value)?, e.revision),
            None => (0u64, 0u64),
        };
        let new_epoch = current + 1;
        let bytes = codec::encode_epoch(new_epoch);
        let ok = if revision == 0 {
            store.kv_epoch.create(&key, bytes).await.is_ok()
        } else {
            store.kv_epoch.update(&key, bytes, revision).await.is_ok()
        };
        if ok {
            return Ok(new_epoch);
        }
    }
    Err(StorageError::Io("epoch CAS exhausted retries".to_string()))
}

async fn supersede(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    new_epoch: u64,
    now: u64,
) -> Result<SupersedeOutcome, StorageError> {
    for _ in 0..5 {
        let entry = store
            .kv_dispatch
            .entry(&keys::dispatch_key(dispatch_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let Some(entry) = entry else {
            return Err(StorageError::NotFound(dispatch_id.to_string()));
        };
        if kv_helpers::is_tombstone(&entry) {
            return Err(StorageError::NotFound(dispatch_id.to_string()));
        }
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status != RunDispatchStatus::Queued {
            if dispatch.status == RunDispatchStatus::Claimed {
                return Ok(SupersedeOutcome::Claimed(Box::new(dispatch)));
            }
            return Ok(SupersedeOutcome::NotQueued);
        }
        if dispatch.dispatch_epoch >= new_epoch {
            return Ok(SupersedeOutcome::NotQueued);
        }
        dispatch.status = RunDispatchStatus::Superseded;
        dispatch.dispatch_epoch = new_epoch;
        dispatch.completed_at = Some(now);
        dispatch.updated_at = now;
        ops_write::clear_claim_fields(&mut dispatch);
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
            ops_write::cleanup_thread_index(store, &dispatch).await;
            return Ok(SupersedeOutcome::Superseded(Box::new(dispatch)));
        }
    }
    Err(StorageError::Io(
        "supersede CAS exhausted retries".to_string(),
    ))
}
