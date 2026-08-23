//! Low-latency wake receiver for the disposable Managed event projection.
//!
//! Durable lifecycle facts remain owned and replayed by Coordinator's one
//! Session outbox consumer. This receiver is only another delivery target on
//! that existing fan-out: it never reads or acknowledges the outbox itself and
//! always enters the sole committed-event projector.

use std::sync::{Arc, Weak};

use awaken_session_contract::{LifecycleFactDelivery, ManagedLifecycleFact};

use crate::{ManagedState, StateError};

/// Immutable delivery target that refreshes the already-served Managed state.
///
/// A weak reference avoids making the outbox supervisor own the HTTP adapter.
/// Composition constructs it from the exact projection before traffic is served.
pub struct ManagedLifecycleFactDelivery {
    state: Weak<ManagedState>,
}

impl ManagedLifecycleFactDelivery {
    #[must_use]
    pub fn new(state: &Arc<ManagedState>) -> Self {
        Self {
            state: Arc::downgrade(state),
        }
    }
}

#[async_trait::async_trait]
impl LifecycleFactDelivery for ManagedLifecycleFactDelivery {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        let state = self
            .state
            .upgrade()
            .ok_or_else(|| "Managed lifecycle projection is unavailable".to_string())?;
        match state.refresh_committed_events(&fact.object_id).await {
            Ok(()) | Err(StateError::NotFound) => Ok(()),
            Err(error) => Err(format!(
                "refresh Managed lifecycle projection for `{}`: {error}",
                fact.object_id
            )),
        }
    }
}
