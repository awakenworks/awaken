//! Construction and clock ownership for the in-memory dispatch adapter.

use std::sync::{Arc, Mutex};

use super::State;

/// In-memory durable-ingress store. Cloneable handles share one state.
pub struct MemoryDispatchStore {
    pub(super) state: Mutex<State>,
    pub(super) authority: Arc<tokio::sync::Mutex<()>>,
    pub(super) clock: Arc<dyn crate::Clock>,
}

impl std::fmt::Debug for MemoryDispatchStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryDispatchStore")
            .finish_non_exhaustive()
    }
}

impl Default for MemoryDispatchStore {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            authority: Arc::new(tokio::sync::Mutex::new(())),
            clock: Arc::new(crate::SystemClock),
        }
    }
}

impl MemoryDispatchStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the store-owned clock. Production uses SystemClock; deterministic
    /// adapter conformance injects one shared ManualClock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn crate::Clock>) -> Self {
        self.clock = clock;
        self
    }
}
