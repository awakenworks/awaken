//! Cooperative pause signal — the input-side mirror of cancellation (ADR-0054).
//!
//! An operator asks an in-flight run to pause via `LiveCommand::Pause`; the
//! runtime sets this shared flag on the active attempt's [`RuntimeRunContext`].
//! The engine observes it **only at safe loop boundaries** (never mid-step), so a
//! pause is always a clean commit-then-park, never a torn state. Modelled on
//! `tokio_util::CancellationToken`: a cheap clonable handle over a shared flag.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A shared flag requesting a park at the next safe boundary. Cloning shares the
/// same underlying flag, so the runtime's command side and the engine's boundary
/// side observe one signal.
#[derive(Clone, Default)]
pub struct PauseSignal(Arc<AtomicBool>);

impl PauseSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a pause; the next safe boundary parks the run.
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// True once a pause has been requested.
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_observes_the_request() {
        let a = PauseSignal::new();
        let b = a.clone();
        assert!(!b.requested());
        a.request();
        assert!(b.requested(), "clones share the flag");
    }
}
