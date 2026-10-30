//! Persistence and lifecycle-outbox ports for the durable Session aggregate.

mod lifecycle_outbox;
mod repository_port;

pub(crate) use crate::state::*;
pub(crate) use awaken_session_types::SessionId;
pub use lifecycle_outbox::LifecycleOutbox;
pub use repository_port::{SessionReconciliation, SessionRepository, SessionRepositoryReads};

#[cfg(test)]
mod tests;
