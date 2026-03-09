use serde::{Deserialize, Serialize};

/// Declarative stop-condition configuration consumed by runtime stop policies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopConditionSpec {
    MaxRounds { rounds: usize },
    Timeout { seconds: u64 },
    TokenBudget { max_total: usize },
    ConsecutiveErrors { max: usize },
    StopOnTool { tool_name: String },
    ContentMatch { pattern: String },
    LoopDetection { window: usize },
}
