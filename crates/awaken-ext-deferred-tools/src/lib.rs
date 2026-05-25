pub mod config;
pub mod plugin;
// Internal: initial tool classification is declarative (`config.resolve_mode`);
// this module holds the live re-deferral mechanism (`DiscBetaEvaluator`) and is
// not a user-pluggable policy surface.
mod policy;
pub mod state;
pub mod tool_search;

pub use config::{DeferredToolsConfig, DeferredToolsConfigKey, ToolLoadMode};
pub use plugin::{DEFERRED_TOOLS_PLUGIN_ID, DeferredToolsPlugin};
pub use state::{AgentToolPriors, AgentToolPriorsKey};
pub use tool_search::TOOL_SEARCH_ID;

#[cfg(test)]
mod tests;
