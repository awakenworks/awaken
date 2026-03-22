use serde::{Deserialize, Serialize};

/// Persistent stats used by stop-condition policies across phase boundaries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopConditionStatsState {
    pub(super) step_count: u32,
    pub(super) total_input_tokens: u64,
    pub(super) total_output_tokens: u64,
    pub(super) start_time_ms: u64,
    pub(super) consecutive_errors: u32,
}

pub struct StopConditionStatsKey;

impl crate::state::StateKey for StopConditionStatsKey {
    const KEY: &'static str = "__runtime.stop_condition_stats";
    type Value = StopConditionStatsState;
    type Update = StopConditionStatsState;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        *value = update;
    }
}
