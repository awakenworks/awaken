#![allow(clippy::module_inception)]

//! Permission plugin: registers state keys and a tool permission checker.

mod checker;
mod filter;
mod plugin;

pub use plugin::{PERMISSION_PLUGIN_NAME, PermissionPlugin};

#[cfg(test)]
mod checker_tests;
#[cfg(test)]
mod filter_tests;
#[cfg(test)]
mod tests;
