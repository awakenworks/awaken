use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub kind: crate::event::kind::Kind,
    pub payload: serde_json::Value,
}
