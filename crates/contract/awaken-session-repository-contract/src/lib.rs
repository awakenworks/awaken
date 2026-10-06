//! Persistence and lifecycle-outbox ports for the durable Session aggregate.

mod lifecycle_outbox;
mod repository_port;

pub(crate) use awaken_session_state_contract::*;
pub(crate) use awaken_session_types::SessionId;
pub use lifecycle_outbox::{LifecycleOutbox, SessionStore};
pub use repository_port::SessionRepository;
