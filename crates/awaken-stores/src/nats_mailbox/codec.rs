//! RunDispatch serialization for NATS KV values.

use awaken_server_contract::contract::mailbox::RunDispatch;
use awaken_server_contract::contract::storage::StorageError;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub fn encode(dispatch: &RunDispatch) -> Result<Bytes, StorageError> {
    serde_json::to_vec(dispatch)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn decode(bytes: &[u8]) -> Result<RunDispatch, StorageError> {
    let dispatch: RunDispatch =
        serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))?;
    dispatch.validate_for_persist()?;
    Ok(dispatch)
}

pub fn encode_thread_index(ids: &[String]) -> Result<Bytes, StorageError> {
    serde_json::to_vec(ids)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn decode_thread_index(bytes: &[u8]) -> Result<Vec<String>, StorageError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DedupeLockRecord {
    pub dispatch_id: String,
    pub created_at: u64,
}

pub(crate) fn encode_dedupe_lock(record: &DedupeLockRecord) -> Result<Bytes, StorageError> {
    serde_json::to_vec(record)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub(crate) fn decode_dedupe_lock(bytes: &[u8]) -> Result<DedupeLockRecord, StorageError> {
    if let Ok(record) = serde_json::from_slice::<DedupeLockRecord>(bytes) {
        return Ok(record);
    }
    let dispatch_id = String::from_utf8(bytes.to_vec())
        .map_err(|e| StorageError::Serialization(format!("dedupe lock utf8: {e}")))?;
    Ok(DedupeLockRecord {
        dispatch_id,
        created_at: 0,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThreadClaim {
    pub dispatch_id: String,
    pub claim_token: String,
    pub lease_until: u64,
}

pub(crate) fn encode_thread_claim(claim: &ThreadClaim) -> Result<Bytes, StorageError> {
    serde_json::to_vec(claim)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub(crate) fn decode_thread_claim(bytes: &[u8]) -> Result<ThreadClaim, StorageError> {
    serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn encode_epoch(epoch: u64) -> Bytes {
    Bytes::copy_from_slice(&epoch.to_le_bytes())
}

pub fn decode_epoch(bytes: &[u8]) -> Result<u64, StorageError> {
    if bytes.len() != 8 {
        return Err(StorageError::Serialization(format!(
            "epoch expects 8 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::mailbox::RunDispatch;

    fn sample_dispatch() -> RunDispatch {
        RunDispatch::queued("d1", "t1", "r1", 0).with_max_attempts(3)
    }

    #[test]
    fn encode_decode_roundtrip() {
        let dispatch = sample_dispatch();
        let encoded = encode(&dispatch).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded.dispatch_id(), "d1");
    }

    #[test]
    fn epoch_roundtrip() {
        let bytes = encode_epoch(42);
        assert_eq!(decode_epoch(&bytes).unwrap(), 42);
    }

    #[test]
    fn epoch_wrong_length_errors() {
        assert!(decode_epoch(&[1, 2, 3]).is_err());
    }

    #[test]
    fn thread_claim_roundtrips() {
        let claim = ThreadClaim {
            dispatch_id: "d1".to_string(),
            claim_token: "token".to_string(),
            lease_until: 42,
        };

        let bytes = encode_thread_claim(&claim).unwrap();
        let decoded = decode_thread_claim(&bytes).unwrap();

        assert_eq!(decoded.dispatch_id, "d1");
        assert_eq!(decoded.claim_token, "token");
        assert_eq!(decoded.lease_until, 42);
    }

    #[test]
    fn dedupe_lock_record_roundtrips() {
        let record = DedupeLockRecord {
            dispatch_id: "d1".to_string(),
            created_at: 123,
        };
        let bytes = encode_dedupe_lock(&record).unwrap();
        let decoded = decode_dedupe_lock(&bytes).unwrap();
        assert_eq!(decoded.dispatch_id, "d1");
        assert_eq!(decoded.created_at, 123);
    }

    #[test]
    fn dedupe_lock_decodes_legacy_dispatch_id() {
        let decoded = decode_dedupe_lock(b"legacy-dispatch").unwrap();
        assert_eq!(decoded.dispatch_id, "legacy-dispatch");
        assert_eq!(decoded.created_at, 0);
    }
}
