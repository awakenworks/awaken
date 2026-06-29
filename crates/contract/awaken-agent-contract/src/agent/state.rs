use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Value {
    pub key: Key,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Set(Value),
    Remove(Key),
}
