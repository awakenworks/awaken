//! Reminder plugin implementation.

mod hook;
mod plugin;

pub use plugin::{REMINDER_PLUGIN_NAME, ReminderPlugin};

#[cfg(test)]
mod tests;
