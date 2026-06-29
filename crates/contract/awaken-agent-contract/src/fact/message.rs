use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub thread_id: crate::agent::thread::Id,
    pub message_id: crate::agent::message::Id,
}
