use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fact {
    pub scope: Scope,
    pub value: crate::agent::state::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    Run(crate::agent::run::Id),
    Thread(crate::agent::thread::Id),
}
