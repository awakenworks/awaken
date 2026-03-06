//! Extension bundles: skills, policy, reminders, observability.

#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "observability")]
pub mod observability;
#[cfg(feature = "permission")]
pub mod permission;
#[cfg(feature = "reminder")]
pub mod reminder;
pub mod skills;
