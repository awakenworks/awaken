//! Recover hot metadata from committed WAL entries after partial writer failure.

use awaken_contract::contract::storage::{StorageError, ThreadRunStore};

use super::{NatsBufferedThreadStore, entry, hot_meta, wal_state};

pub(crate) async fn reconcile_thread_tail<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<(), StorageError> {
    let meta = hot_meta::read_meta(&store.kv_hot, thread_id).await?;
    let settled_states = settle_thread_states(store, thread_id).await?;

    let mut latest_seq = meta.latest_seq;
    let mut latest_js_seq = meta.latest_js_seq;
    for state in settled_states {
        if state.thread_seq <= meta.latest_seq {
            continue;
        }
        let Some(js_seq) = state.state.js_seq else {
            continue;
        };
        let raw = store.stream.get_raw_message(js_seq).await.map_err(|error| {
            StorageError::Io(format!(
                "load committed WAL entry (thread={thread_id}, seq={}, js_seq={js_seq}): {error}",
                state.thread_seq
            ))
        })?;
        let decoded = entry::decode(&raw.payload)?;
        hot_meta::cache_run_if_newer(&store.kv_hot, &decoded.run, decoded.thread_seq).await?;
        if decoded.thread_seq > latest_seq {
            latest_seq = decoded.thread_seq;
            latest_js_seq = js_seq;
        }
    }

    if latest_seq > meta.latest_seq {
        hot_meta::promote_latest_seq(
            &store.kv_hot,
            thread_id,
            latest_seq,
            latest_js_seq,
            now_millis(),
        )
        .await?;
    }

    Ok(())
}

pub(crate) async fn reconcile_all_thread_tails<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
) -> Result<(), StorageError> {
    for thread_id in wal_state::known_thread_ids(&store.kv_hot).await? {
        reconcile_thread_tail(store, &thread_id).await?;
    }
    Ok(())
}

pub(crate) async fn settle_thread_states<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    thread_id: &str,
) -> Result<Vec<wal_state::ThreadWalState>, StorageError> {
    let mut states = wal_state::list_thread_states(&store.kv_hot, thread_id).await?;
    for state in &mut states {
        if state.state.status == wal_state::WalEntryStatus::Prepared {
            if let Some(updated) =
                wal_state::settle_thread_state(&store.kv_hot, thread_id, state.thread_seq).await?
            {
                state.state = updated;
            }
        }
    }
    Ok(states)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
