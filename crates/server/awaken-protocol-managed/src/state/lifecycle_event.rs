//! Lifecycle-fact catalog committed through the Session repository outbox.
//!
//! These past-tense facts are distinct from the in-session SSE transition names.
//! The Managed adapter owns the catalog while the contract owns only the narrow
//! sink port, so webhook delivery machinery never becomes a protocol dependency.

/// Session terminated (archived). Anthropic `session.status_terminated`.
pub const SESSION_TERMINATED: &str = "session.status_terminated";
