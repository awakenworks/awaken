//! Durable commit state for NATS buffered thread WAL entries.

use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use awaken_server_contract::contract::storage::StorageError;

use super::{hierarchy_claim, keys};

const CAS_RETRIES: usize = 5;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WalEntryStatus {
    Prepared,
    Committed,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WalEntryState {
    pub claim_token: String,
    pub written_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub js_seq: Option<u64>,
    pub status: WalEntryStatus,
}

#[derive(Debug, Clone)]
pub(crate) struct ThreadWalState {
    pub thread_id: String,
    pub thread_seq: u64,
    pub state: WalEntryState,
}

pub(crate) async fn mark_prepared(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
    claim_token: &str,
    written_at: u64,
) -> Result<(), StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    let state = WalEntryState {
        claim_token: claim_token.to_string(),
        written_at,
        js_seq: None,
        status: WalEntryStatus::Prepared,
    };
    kv.create(&key, encode_state(&state)?)
        .await
        .map(|_| ())
        .map_err(|error| StorageError::Io(format!("create WAL state {key}: {error}")))
}

pub(crate) async fn mark_committed(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
    claim_token: &str,
    js_seq: u64,
) -> Result<(), StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    for _ in 0..CAS_RETRIES {
        let Some(entry) = read_live_entry(kv, &key).await? else {
            return Err(StorageError::Io(format!(
                "commit WAL state missing (thread={thread_id}, seq={thread_seq})"
            )));
        };
        let mut state = decode_state(&entry.value)?;
        if state.claim_token != claim_token {
            return Err(StorageError::Io(format!(
                "commit WAL state ownership mismatch (thread={thread_id}, seq={thread_seq})"
            )));
        }
        match state.status {
            WalEntryStatus::Prepared => {
                if !hierarchy_claim::claim_token_is_current(kv, claim_token).await? {
                    return Err(StorageError::Io(format!(
                        "cannot commit WAL state after hierarchy claim expiry (thread={thread_id}, seq={thread_seq})"
                    )));
                }
            }
            WalEntryStatus::Committed if state.js_seq == Some(js_seq) => return Ok(()),
            WalEntryStatus::Committed => {
                return Err(StorageError::CommitUnknown(format!(
                    "WAL state already committed with different js_seq (thread={thread_id}, seq={thread_seq}, existing_js_seq={:?}, new_js_seq={js_seq})",
                    state.js_seq
                )));
            }
            WalEntryStatus::Aborted => {
                return Err(StorageError::Io(format!(
                    "cannot commit aborted WAL state (thread={thread_id}, seq={thread_seq})"
                )));
            }
        }
        state.js_seq = Some(js_seq);
        state.status = WalEntryStatus::Committed;
        if kv
            .update(&key, encode_state(&state)?, entry.revision)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }

    Err(StorageError::Io(format!(
        "commit WAL state CAS exhausted retries (thread={thread_id}, seq={thread_seq})"
    )))
}

pub(crate) async fn put_committed_state(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
    js_seq: u64,
    written_at: u64,
) -> Result<(), StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    let state = WalEntryState {
        claim_token: "test-committed".to_string(),
        written_at,
        js_seq: Some(js_seq),
        status: WalEntryStatus::Committed,
    };
    kv.put(&key, encode_state(&state)?)
        .await
        .map(|_| ())
        .map_err(|error| StorageError::Io(format!("put WAL state {key}: {error}")))
}

pub(crate) async fn mark_aborted(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
    claim_token: &str,
) -> Result<(), StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    for _ in 0..CAS_RETRIES {
        let Some(entry) = read_live_entry(kv, &key).await? else {
            return Ok(());
        };
        let mut state = decode_state(&entry.value)?;
        if state.claim_token != claim_token {
            return Ok(());
        }
        if state.status == WalEntryStatus::Committed {
            return Ok(());
        }
        state.status = WalEntryStatus::Aborted;
        if kv
            .update(&key, encode_state(&state)?, entry.revision)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }

    Err(StorageError::Io(format!(
        "abort WAL state CAS exhausted retries (thread={thread_id}, seq={thread_seq})"
    )))
}

pub(crate) async fn load_state(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
) -> Result<Option<WalEntryState>, StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    Ok(match read_live_entry(kv, &key).await? {
        Some(entry) => Some(decode_state(&entry.value)?),
        None => None,
    })
}

pub(crate) async fn settle_thread_state(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
) -> Result<Option<WalEntryState>, StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    for _ in 0..CAS_RETRIES {
        let Some(entry) = read_live_entry(kv, &key).await? else {
            return Ok(None);
        };
        let mut state = decode_state(&entry.value)?;
        if state.status != WalEntryStatus::Prepared {
            return Ok(Some(state));
        }
        if hierarchy_claim::claim_token_is_current(kv, &state.claim_token).await? {
            return Ok(Some(state));
        }
        state.status = WalEntryStatus::Aborted;
        let encoded = encode_state(&state)?;
        if kv.update(&key, encoded, entry.revision).await.is_ok() {
            return Ok(Some(state));
        }
    }

    Err(StorageError::Io(format!(
        "settle WAL state CAS exhausted retries (thread={thread_id}, seq={thread_seq})"
    )))
}

pub(crate) async fn list_thread_states(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
) -> Result<Vec<ThreadWalState>, StorageError> {
    let prefix = keys::wal_state_prefix(thread_id);
    let mut key_stream = kv
        .keys()
        .await
        .map_err(|error| StorageError::Io(format!("list WAL state keys: {error}")))?;
    let mut states = Vec::new();
    while let Some(key_result) = key_stream.next().await {
        let key =
            key_result.map_err(|error| StorageError::Io(format!("WAL state key: {error}")))?;
        if !key.starts_with(&prefix) {
            continue;
        }
        let Some(entry) = read_live_entry(kv, &key).await? else {
            continue;
        };
        let Some(parsed_thread_id) = keys::thread_id_from_wal_state_key(&key) else {
            continue;
        };
        let Some(thread_seq) = keys::thread_seq_from_wal_state_key(&key) else {
            continue;
        };
        states.push(ThreadWalState {
            thread_id: parsed_thread_id,
            thread_seq,
            state: decode_state(&entry.value)?,
        });
    }
    states.sort_by_key(|state| state.thread_seq);
    Ok(states)
}

pub(crate) async fn known_thread_ids(
    kv: &async_nats::jetstream::kv::Store,
) -> Result<Vec<String>, StorageError> {
    let mut key_stream = kv
        .keys()
        .await
        .map_err(|error| StorageError::Io(format!("list WAL state keys: {error}")))?;
    let mut thread_ids = Vec::new();
    while let Some(key_result) = key_stream.next().await {
        let key =
            key_result.map_err(|error| StorageError::Io(format!("WAL state key: {error}")))?;
        let Some(thread_id) = keys::thread_id_from_wal_state_key(&key) else {
            continue;
        };
        if read_live_entry(kv, &key).await?.is_some() {
            thread_ids.push(thread_id);
        }
    }
    thread_ids.sort();
    thread_ids.dedup();
    Ok(thread_ids)
}

pub(crate) async fn delete_state(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    thread_seq: u64,
) -> Result<(), StorageError> {
    let key = keys::wal_state_key(thread_id, thread_seq);
    if read_live_entry(kv, &key).await?.is_none() {
        return Ok(());
    }
    kv.delete(&key)
        .await
        .map_err(|error| StorageError::Io(format!("delete WAL state {key}: {error}")))
}

fn encode_state(state: &WalEntryState) -> Result<Bytes, StorageError> {
    serde_json::to_vec(state)
        .map(Bytes::from)
        .map_err(|error| StorageError::Serialization(error.to_string()))
}

fn decode_state(bytes: &[u8]) -> Result<WalEntryState, StorageError> {
    serde_json::from_slice(bytes).map_err(|error| StorageError::Serialization(error.to_string()))
}

async fn read_live_entry(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<Option<async_nats::jetstream::kv::Entry>, StorageError> {
    Ok(
        match kv
            .entry(key)
            .await
            .map_err(|error| StorageError::Io(format!("read WAL state {key}: {error}")))?
        {
            Some(entry) if !is_tombstone(&entry) => Some(entry),
            _ => None,
        },
    )
}

fn is_tombstone(entry: &async_nats::jetstream::kv::Entry) -> bool {
    matches!(
        entry.operation,
        async_nats::jetstream::kv::Operation::Delete | async_nats::jetstream::kv::Operation::Purge
    )
}
