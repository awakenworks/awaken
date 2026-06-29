use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub run_id: crate::agent::run::Id,
    pub lifecycle: crate::agent::run::Lifecycle,
}
