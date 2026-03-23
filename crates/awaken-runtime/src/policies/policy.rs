use std::sync::Arc;

use awaken_contract::contract::lifecycle::StopConditionSpec;

/// Decision returned by a stop policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopDecision {
    /// No stop condition triggered.
    Continue,
    /// Stop the run with a code and detail message.
    Stop { code: String, detail: String },
}

/// Statistics available to stop policies for evaluation.
#[derive(Debug, Clone)]
pub struct StopPolicyStats {
    pub step_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub elapsed_ms: u64,
    pub consecutive_errors: u32,
    pub last_tool_names: Vec<String>,
    pub last_response_text: String,
}

/// A stateless stop condition evaluator.
///
/// Reads stats from the context and returns a decision.
/// Implementations must NOT be async — evaluation is pure computation on stats.
pub trait StopPolicy: Send + Sync + 'static {
    /// Unique identifier for this policy.
    fn id(&self) -> &str;

    /// Evaluate whether the run should stop based on current stats.
    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision;
}

/// Stop when step count reaches or exceeds `max`.
pub struct MaxRoundsPolicy {
    pub max: usize,
}

impl MaxRoundsPolicy {
    pub fn new(max: usize) -> Self {
        Self { max }
    }
}

impl StopPolicy for MaxRoundsPolicy {
    fn id(&self) -> &str {
        "max_rounds"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max == 0 {
            return StopDecision::Continue;
        }
        if stats.step_count as usize > self.max {
            StopDecision::Stop {
                code: "max_rounds".into(),
                detail: format!("exceeded {} rounds", self.max),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop when total tokens (input + output) exceed a budget.
pub struct TokenBudgetPolicy {
    pub max_total: u64,
}

impl TokenBudgetPolicy {
    pub fn new(max_total: u64) -> Self {
        Self { max_total }
    }
}

impl StopPolicy for TokenBudgetPolicy {
    fn id(&self) -> &str {
        "token_budget"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max_total == 0 {
            return StopDecision::Continue;
        }
        let total = stats.total_input_tokens + stats.total_output_tokens;
        if total > self.max_total {
            StopDecision::Stop {
                code: "token_budget".into(),
                detail: format!("token usage {} exceeds budget {}", total, self.max_total),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop when elapsed time exceeds a limit in milliseconds.
pub struct TimeoutPolicy {
    pub max_ms: u64,
}

impl TimeoutPolicy {
    pub fn new(max_ms: u64) -> Self {
        Self { max_ms }
    }
}

impl StopPolicy for TimeoutPolicy {
    fn id(&self) -> &str {
        "timeout"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max_ms == 0 {
            return StopDecision::Continue;
        }
        if stats.elapsed_ms > self.max_ms {
            StopDecision::Stop {
                code: "timeout".into(),
                detail: format!(
                    "elapsed {}ms exceeds limit {}ms",
                    stats.elapsed_ms, self.max_ms
                ),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Stop after N consecutive tool errors.
pub struct ConsecutiveErrorsPolicy {
    pub max: u32,
}

impl ConsecutiveErrorsPolicy {
    pub fn new(max: u32) -> Self {
        Self { max }
    }
}

impl StopPolicy for ConsecutiveErrorsPolicy {
    fn id(&self) -> &str {
        "consecutive_errors"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.max == 0 {
            return StopDecision::Continue;
        }
        if stats.consecutive_errors >= self.max {
            StopDecision::Stop {
                code: "consecutive_errors".into(),
                detail: format!(
                    "{} consecutive errors (limit {})",
                    stats.consecutive_errors, self.max
                ),
            }
        } else {
            StopDecision::Continue
        }
    }
}

/// Convert declarative stop condition specs into policy instances.
pub fn policies_from_specs(specs: &[StopConditionSpec]) -> Vec<Arc<dyn StopPolicy>> {
    specs
        .iter()
        .filter_map(|spec| -> Option<Arc<dyn StopPolicy>> {
            match spec {
                StopConditionSpec::MaxRounds { rounds } => {
                    Some(Arc::new(MaxRoundsPolicy::new(*rounds)))
                }
                StopConditionSpec::Timeout { seconds } => {
                    Some(Arc::new(TimeoutPolicy::new(*seconds * 1000)))
                }
                StopConditionSpec::TokenBudget { max_total } => {
                    Some(Arc::new(TokenBudgetPolicy::new(*max_total as u64)))
                }
                StopConditionSpec::ConsecutiveErrors { max } => {
                    Some(Arc::new(ConsecutiveErrorsPolicy::new(*max as u32)))
                }
                // StopOnTool, ContentMatch, LoopDetection are not yet implemented
                StopConditionSpec::StopOnTool { .. }
                | StopConditionSpec::ContentMatch { .. }
                | StopConditionSpec::LoopDetection { .. } => None,
            }
        })
        .collect()
}
