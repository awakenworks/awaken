// @formal entrypoint session_run.reservation_ttl_uses_store_clock
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

impl MemoryDispatchStore {
    /// The clock is required, and there is deliberately no default.
    ///
    /// This store judges its own rows: whether a lease has expired, whether a
    /// retry is due, when a reservation lapses. A hidden `SystemClock` default
    /// made it possible to hand a test a `ManualClock`, move it, and watch the
    /// assertions pass while the store answered from wall time -- the test had
    /// stopped exercising the boundary it was named for, and said nothing.
    /// Naming the clock at construction cannot make that mistake impossible,
    /// but it does make it impossible to make *silently*: a caller that wants
    /// wall time now says so.
    #[must_use]
    pub fn new(clock: Arc<dyn crate::Clock>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            authority: Arc::new(tokio::sync::Mutex::new(())),
            clock,
        }
    }
}
