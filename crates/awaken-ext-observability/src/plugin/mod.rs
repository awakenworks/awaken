#![allow(clippy::module_inception)]

mod hooks;
mod plugin;
mod shared;

#[cfg(test)]
pub(crate) use hooks::{
    AfterInferenceHook, AfterToolExecuteHook, BeforeInferenceHook, BeforeToolExecuteHook,
    RunEndHook, RunStartHook,
};
pub use plugin::{OBSERVABILITY_PLUGIN_ID, ObservabilityPlugin};
#[cfg(test)]
pub(crate) use shared::{extract_cache_tokens, extract_token_counts};

#[cfg(test)]
mod tests;
