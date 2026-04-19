//! Interrupt + dispatch epoch management.

use awaken_contract::contract::mailbox::{MailboxInterrupt, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, codec, keys, ops_write};

enum SupersedeOutcome {
    Superseded,
    Claimed(Box<awaken_contract::contract::mailbox::RunDispatch>),
    NotQueued,
}

pub async fn interrupt(
    store: &NatsMailboxStore,
    thread_id: &str,
    now: u64,
) -> Result<MailboxInterrupt, StorageError> {
    let new_epoch = bump_epoch(store, thread_id).await?;

    let dispatches = store.index.read().await.list_by_thread(thread_id, None);

    let mut superseded_count = 0usize;
    let mut active_dispatch = None;

    for dispatch in dispatches {
        match dispatch.status {
            RunDispatchStatus::Queued => {
                match supersede(store, &dispatch.dispatch_id, new_epoch, now).await {
                    Ok(SupersedeOutcome::Superseded) => {
                        superseded_count += 1;
                        // Terminal → release any dedupe lock so a subsequent
                        // enqueue with the same key can take over.
                        if let Some(ref dedupe_key) = dispatch.dedupe_key {
                            ops_write::release_dedupe_lock(
                                store,
                                &dispatch.thread_id,
                                dedupe_key,
                                &dispatch.dispatch_id,
                            )
                            .await;
                        }
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
                active_dispatch = Some(dispatch);
            }
            _ => {}
        }
    }

    Ok(MailboxInterrupt {
        new_dispatch_epoch: new_epoch,
        active_dispatch,
        superseded_count,
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
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status != RunDispatchStatus::Queued {
            if dispatch.status == RunDispatchStatus::Claimed {
                return Ok(SupersedeOutcome::Claimed(Box::new(dispatch)));
            }
            return Ok(SupersedeOutcome::NotQueued);
        }
        dispatch.status = RunDispatchStatus::Superseded;
        dispatch.dispatch_epoch = new_epoch;
        dispatch.completed_at = Some(now);
        dispatch.updated_at = now;
        let bytes = codec::encode(&dispatch)?;
        if store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
            .is_ok()
        {
            store.index.write().await.upsert(dispatch);
            return Ok(SupersedeOutcome::Superseded);
        }
    }
    Err(StorageError::Io(
        "supersede CAS exhausted retries".to_string(),
    ))
}
