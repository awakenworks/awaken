use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{RunRecord, StorageError};
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEntry {
    pub thread_id: String,
    pub run: RunRecord,
    pub messages: Vec<Message>,
    pub thread_seq: u64,
    pub written_at: u64,
}

pub fn encode(entry: &CheckpointEntry) -> Result<Bytes, StorageError> {
    serde_json::to_vec(entry)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn decode(bytes: &[u8]) -> Result<CheckpointEntry, StorageError> {
    serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;

    fn sample() -> CheckpointEntry {
        CheckpointEntry {
            thread_id: "t1".to_string(),
            run: RunRecord {
                run_id: "r1".to_string(),
                thread_id: "t1".to_string(),
                agent_id: "agent".to_string(),
                parent_run_id: None,
                request: None,
                input: None,
                output: None,
                status: RunStatus::Done,
                termination_reason: None,
                final_output: None,
                error_payload: None,
                dispatch_id: None,
                session_id: None,
                transport_request_id: None,
                waiting: None,
                outcome: None,
                created_at: 1,
                started_at: None,
                finished_at: None,
                updated_at: 1,
                steps: 0,
                input_tokens: 0,
                output_tokens: 0,
                state: None,
            },
            messages: vec![Message::user("hi")],
            thread_seq: 1,
            written_at: 1000,
        }
    }

    #[test]
    fn roundtrip_preserves_fields() {
        let entry = sample();
        let bytes = encode(&entry).unwrap();
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.thread_id, "t1");
        assert_eq!(decoded.thread_seq, 1);
        assert_eq!(decoded.messages.len(), 1);
    }
}
