use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub sequence: u64,
    pub run_id: crate::agent::run::Id,
    pub kind: crate::event::kind::Kind,
    pub payload: serde_json::Value,
}
