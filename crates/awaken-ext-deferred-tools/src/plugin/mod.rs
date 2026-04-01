pub mod hooks;
#[allow(clippy::module_inception)]
pub mod plugin;

pub use plugin::{DEFERRED_TOOLS_PLUGIN_ID, DeferredToolsPlugin};
