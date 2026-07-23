//! Versioned canonical hashing for retryable Thread commit operations.

use awaken_agent_contract::thread::commit::operation::CommitPayloadHash;
use awaken_agent_contract::thread::commit::staged::ThreadCommit;
use sha2::{Digest, Sha256};

const CANONICAL_COMMIT_VERSION: &str = "awaken.thread-commit.v1";

#[derive(Debug, thiserror::Error)]
pub enum CommitHashError {
    #[error("serialize ThreadCommit for canonical hashing: {0}")]
    Serialize(String),
}

/// Hash a `ThreadCommit` independently of HTTP JSON object field order.
pub fn commit_payload_hash(commit: &ThreadCommit) -> Result<CommitPayloadHash, CommitHashError> {
    let value = serde_json::to_value(commit)
        .map_err(|error| CommitHashError::Serialize(error.to_string()))?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical);
    let mut hasher = Sha256::new();
    hasher.update((CANONICAL_COMMIT_VERSION.len() as u64).to_le_bytes());
    hasher.update(CANONICAL_COMMIT_VERSION.as_bytes());
    hasher.update((canonical.len() as u64).to_le_bytes());
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(CommitPayloadHash(format!("sha256:{hex}")))
}

fn write_canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output
                .push_str(&serde_json::to_string(value).expect("a JSON string always serializes"));
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("a JSON object key always serializes"),
                );
                output.push(':');
                write_canonical_json(&values[key], output);
            }
            output.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_agent_contract::audit::draft::Draft;
    use awaken_agent_contract::audit::kind::Kind;
    use awaken_agent_contract::thread::commit::RunDisposition;

    use super::*;

    fn commit(payload: serde_json::Value) -> ThreadCommit {
        ThreadCommit {
            thread_id: ThreadId("thread".into()),
            run: RunDisposition::running(RunId("run".into())),
            messages: Vec::new(),
            state: Vec::new(),
            events: vec![Draft {
                kind: Kind::RunStateChanged,
                payload,
            }],
        }
    }

    #[test]
    fn object_field_order_does_not_change_commit_hash() {
        let left = commit(serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap());
        let right = commit(serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap());
        assert_eq!(
            commit_payload_hash(&left).unwrap(),
            commit_payload_hash(&right).unwrap()
        );
    }

    #[test]
    fn payload_change_changes_commit_hash() {
        assert_ne!(
            commit_payload_hash(&commit(serde_json::json!({"a": 1}))).unwrap(),
            commit_payload_hash(&commit(serde_json::json!({"a": 2}))).unwrap()
        );
    }
}
