//! Reminder extension for the awaken agent framework.
//!
//! Provides declarative reminder rules that trigger after tool execution.
//! When a tool call matches both input pattern and output conditions,
//! a context message is injected into the prompt.

pub mod config;
pub mod output_matcher;
pub mod plugin;
pub mod rule;

pub use config::ReminderConfig;
pub use output_matcher::{ContentMatcher, OutputMatcher, ToolStatusMatcher, output_matches};
pub use plugin::ReminderPlugin;
pub use rule::ReminderRule;
