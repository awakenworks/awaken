//! Permission plugin: registers state keys and a tool permission checker.

mod checker;
mod plugin;

pub use plugin::{PERMISSION_PLUGIN_NAME, PermissionPlugin};

#[cfg(test)]
mod tests;
