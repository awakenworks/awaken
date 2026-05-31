use std::sync::Arc;

use awaken_runtime_contract::contract::lifecycle::StopConditionSpec;
use regex::Regex;

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
    pub recent_response_texts: Vec<String>,
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
        if stats.step_count as usize >= self.max {
            StopDecision::Stop {
                code: "max_rounds".into(),
                detail: format!("reached {} rounds", self.max),
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

/// Stop after N consecutive inference errors.
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
                    "{} consecutive inference errors (limit {})",
                    stats.consecutive_errors, self.max
                ),
            }
        } else {
            StopDecision::Continue
        }
    }
}

pub struct StopOnToolPolicy {
    tool_name: String,
}

impl StopOnToolPolicy {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
        }
    }
}

impl StopPolicy for StopOnToolPolicy {
    fn id(&self) -> &str {
        "stop_on_tool"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.tool_name.is_empty() {
            return StopDecision::Continue;
        }
        if stats
            .last_tool_names
            .iter()
            .any(|name| name == &self.tool_name)
        {
            StopDecision::Stop {
                code: "stop_on_tool".into(),
                detail: format!("tool {} was requested before execution", self.tool_name),
            }
        } else {
            StopDecision::Continue
        }
    }
}

pub struct ContentMatchPolicy {
    pattern: String,
    regex: Result<Regex, String>,
}

impl ContentMatchPolicy {
    pub fn new(pattern: impl Into<String>) -> Self {
        let pattern = pattern.into();
        let regex = Regex::new(&pattern).map_err(|error| error.to_string());
        Self { pattern, regex }
    }
}

impl StopPolicy for ContentMatchPolicy {
    fn id(&self) -> &str {
        "content_match"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        if self.pattern.is_empty() {
            return StopDecision::Continue;
        }
        let regex = match &self.regex {
            Ok(regex) => regex,
            Err(error) => {
                return StopDecision::Stop {
                    code: "content_match_invalid_regex".into(),
                    detail: format!(
                        "content_match stop condition has invalid regex {}: {error}",
                        self.pattern
                    ),
                };
            }
        };
        if regex.is_match(&stats.last_response_text) {
            StopDecision::Stop {
                code: "content_match".into(),
                detail: format!("response matched stop regex {}", self.pattern),
            }
        } else {
            StopDecision::Continue
        }
    }
}

pub struct LoopDetectionPolicy {
    window: usize,
}

impl LoopDetectionPolicy {
    pub fn new(window: usize) -> Self {
        Self { window }
    }
}

impl StopPolicy for LoopDetectionPolicy {
    fn id(&self) -> &str {
        "loop_detection"
    }

    fn evaluate(&self, stats: &StopPolicyStats) -> StopDecision {
        // This policy intentionally detects exact repeated response text only.
        // Whitespace, timestamp, or formatting differences are treated as
        // distinct responses.
        if self.window < 2 || stats.recent_response_texts.len() < self.window {
            return StopDecision::Continue;
        }
        let start = stats.recent_response_texts.len() - self.window;
        let window = &stats.recent_response_texts[start..];
        let Some(first) = window.first() else {
            return StopDecision::Continue;
        };
        if first.is_empty() {
            return StopDecision::Continue;
        }
        if window.iter().all(|item| item == first) {
            StopDecision::Stop {
                code: "loop_detection".into(),
                detail: format!("same response repeated for {} steps", self.window),
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
        .map(|spec| -> Arc<dyn StopPolicy> {
            match spec {
                StopConditionSpec::MaxRounds { rounds } => Arc::new(MaxRoundsPolicy::new(*rounds)),
                StopConditionSpec::Timeout { seconds } => {
                    Arc::new(TimeoutPolicy::new(seconds.saturating_mul(1000)))
                }
                StopConditionSpec::TokenBudget { max_total } => {
                    Arc::new(TokenBudgetPolicy::new(*max_total as u64))
                }
                StopConditionSpec::ConsecutiveErrors { max } => {
                    Arc::new(ConsecutiveErrorsPolicy::new(*max as u32))
                }
                StopConditionSpec::StopOnTool { tool_name } => {
                    Arc::new(StopOnToolPolicy::new(tool_name.clone()))
                }
                StopConditionSpec::ContentMatch { pattern } => {
                    Arc::new(ContentMatchPolicy::new(pattern.clone()))
                }
                StopConditionSpec::LoopDetection { window } => {
                    Arc::new(LoopDetectionPolicy::new(*window))
                }
            }
        })
        .collect()
}
