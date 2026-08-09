//! Webhook lifecycle-fact catalog projected through `SessionLifecycleFactSink`.
//!
//! These past-tense facts are distinct from the in-session SSE transition names.
//! The Managed adapter owns the catalog while the contract owns only the narrow
//! sink port, so webhook delivery machinery never becomes a protocol dependency.

/// Session created, or a turn settled — now idle. Anthropic `session.status_idled`.
pub const SESSION_IDLED: &str = "session.status_idled";
/// Session terminated (archived). Anthropic `session.status_terminated`.
pub const SESSION_TERMINATED: &str = "session.status_terminated";
/// Session deleted (record dropped, not tombstoned). Matches the SSE terminal
/// transition name because the delete edge carries no status.
pub const SESSION_DELETED: &str = "session.deleted";
