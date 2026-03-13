mod conditions;
mod plugin;
mod state;

#[cfg(test)]
mod tests;

pub const STOP_POLICY_PLUGIN_ID: &str = "stop_policy";

pub use conditions::{
    ConsecutiveErrors, ContentMatch, LoopDetection, MaxRounds, StopOnTool, StopPolicy,
    StopPolicyInput, StopPolicyStats, Timeout, TokenBudget,
};
pub use plugin::StopPolicyPlugin;
