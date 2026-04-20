//! Per-thread distributed claim guard.

use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, codec, keys};

pub(crate) struct AcquiredThreadClaim {
    pub claim_token: String,
    pub lease_until: u64,
}

pub(crate) async fn active_dispatch_id(
    store: &NatsMailboxStore,
    thread_id: &str,
    now: u64,
) -> Result<Option<String>, StorageError> {
    let key = keys::thread_claim_key(thread_id);
    let Some(entry) = store
        .kv_thread_index
        .entry(&key)
        .await
        .map_err(|e| StorageError::Io(format!("thread claim entry: {e}")))?
    else {
        return Ok(None);
    };
    let claim = codec::decode_thread_claim(&entry.value)?;
    if claim.lease_until >= now {
        Ok(Some(claim.dispatch_id))
    } else {
        Ok(None)
    }
}

pub(crate) async fn acquire(
    store: &NatsMailboxStore,
    thread_id: &str,
    dispatch_id: &str,
    lease_ms: u64,
    now: u64,
) -> Result<Option<AcquiredThreadClaim>, StorageError> {
    let key = keys::thread_claim_key(thread_id);
    for _ in 0..5 {
        let entry = store
            .kv_thread_index
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("thread claim entry: {e}")))?;

        if let Some(ref entry) = entry {
            let existing = codec::decode_thread_claim(&entry.value)?;
            if existing.lease_until >= now {
                return Ok(None);
            }
        }

        let claim_token = uuid::Uuid::now_v7().to_string();
        let lease_until = now.saturating_add(lease_ms);
        let claim = codec::ThreadClaim {
            dispatch_id: dispatch_id.to_string(),
            claim_token: claim_token.clone(),
            lease_until,
        };
        let bytes = codec::encode_thread_claim(&claim)?;

        let result = match entry {
            Some(entry) => store
                .kv_thread_index
                .update(&key, bytes, entry.revision)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            None => store
                .kv_thread_index
                .create(&key, bytes)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
        };

        if result.is_ok() {
            return Ok(Some(AcquiredThreadClaim {
                claim_token,
                lease_until,
            }));
        }
    }

    Err(StorageError::Io(
        "thread claim CAS exhausted retries".to_string(),
    ))
}

pub(crate) async fn extend(
    store: &NatsMailboxStore,
    thread_id: &str,
    dispatch_id: &str,
    claim_token: &str,
    lease_until: u64,
) -> Result<bool, StorageError> {
    let key = keys::thread_claim_key(thread_id);
    for _ in 0..5 {
        let entry = store
            .kv_thread_index
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("thread claim entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(false);
        };
        let mut claim = codec::decode_thread_claim(&entry.value)?;
        if claim.dispatch_id != dispatch_id || claim.claim_token != claim_token {
            return Ok(false);
        }
        claim.lease_until = lease_until;
        let bytes = codec::encode_thread_claim(&claim)?;
        if store
            .kv_thread_index
            .update(&key, bytes, entry.revision)
            .await
            .is_ok()
        {
            return Ok(true);
        }
    }

    Ok(false)
}

pub(crate) async fn release(
    store: &NatsMailboxStore,
    thread_id: &str,
    dispatch_id: &str,
    claim_token: &str,
) -> Result<(), StorageError> {
    let key = keys::thread_claim_key(thread_id);
    for _ in 0..5 {
        let entry = store
            .kv_thread_index
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("thread claim entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(());
        };
        let mut claim = codec::decode_thread_claim(&entry.value)?;
        if claim.dispatch_id != dispatch_id || claim.claim_token != claim_token {
            return Ok(());
        }
        claim.lease_until = 0;
        let bytes = codec::encode_thread_claim(&claim)?;
        if store
            .kv_thread_index
            .update(&key, bytes, entry.revision)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }

    Err(StorageError::Io(
        "thread claim release CAS exhausted retries".to_string(),
    ))
}
