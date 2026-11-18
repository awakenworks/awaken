//! Persistence and lifecycle-outbox ports for the durable Session aggregate.

mod creation_intents;
mod lifecycle_outbox;
mod repository_port;

pub(crate) use crate::state::*;
pub(crate) use awaken_session_types::SessionId;
pub use creation_intents::{
    PreparedParticipantIntent, PreparedParticipantIntentClaim, PreparedParticipantIntentPhase,
    PreparedParticipantIntentRecord, PreparedParticipantIntents,
};
pub use lifecycle_outbox::{LifecycleOutbox, LifecycleOutboxBatch, LifecycleOutboxCursor};
pub use repository_port::{
    SessionConditionalPublication, SessionReconciliation, SessionRepository, SessionRepositoryReads,
};

#[cfg(test)]
mod tests;
