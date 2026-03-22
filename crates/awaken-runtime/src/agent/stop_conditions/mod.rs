//! Stop condition policy system and built-in policies.

mod hook;
mod plugin;
mod policy;
mod state;

pub use plugin::{MaxRoundsPlugin, StopConditionPlugin};
pub use policy::{
    ConsecutiveErrorsPolicy, MaxRoundsPolicy, StopDecision, StopPolicy, StopPolicyStats,
    TimeoutPolicy, TokenBudgetPolicy, policies_from_specs,
};
pub use state::{StopConditionStatsKey, StopConditionStatsState};

#[cfg(test)]
mod tests;
