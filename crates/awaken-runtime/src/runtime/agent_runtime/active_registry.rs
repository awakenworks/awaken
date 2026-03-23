//! ActiveRunRegistry: tracks active runs with dual indexing (run_id + thread_id).

use std::collections::HashMap;

use parking_lot::RwLock;

use super::RunHandle;

/// Result of dual-index lookup for external control IDs.
pub(super) enum HandleLookup {
    Found(RunHandle),
    NotFound,
    Ambiguous,
}

/// Tracks active runs with dual indexing by run_id and thread_id.
/// At most one active run per thread.
pub(crate) struct ActiveRunRegistry {
    by_run_id: RwLock<HashMap<String, RunHandle>>,
    by_thread_id: RwLock<HashMap<String, String>>, // thread_id → run_id
}

impl ActiveRunRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_run_id: RwLock::new(HashMap::new()),
            by_thread_id: RwLock::new(HashMap::new()),
        }
    }

    /// Register a run with both run_id and thread_id indexing.
    /// Returns `false` if either the thread or run_id is already active.
    pub(super) fn register(&self, run_id: &str, thread_id: &str, handle: RunHandle) -> bool {
        let mut by_thread = self.by_thread_id.write();
        let mut by_run = self.by_run_id.write();

        if by_thread.contains_key(thread_id) || by_run.contains_key(run_id) {
            return false;
        }

        by_thread.insert(thread_id.to_string(), run_id.to_string());
        by_run.insert(run_id.to_string(), handle);
        true
    }

    /// Unregister a run by run_id. Removes both run_id and thread_id mappings.
    pub(super) fn unregister(&self, run_id: &str) {
        let mut by_thread = self.by_thread_id.write();
        let mut by_run = self.by_run_id.write();
        by_run.remove(run_id);
        by_thread.retain(|_, v| v != run_id);
    }

    /// Look up a handle by run_id.
    pub(super) fn get_by_run_id(&self, run_id: &str) -> Option<RunHandle> {
        self.by_run_id.read().get(run_id).cloned()
    }

    /// Look up a handle by thread_id (resolves thread_id -> run_id -> handle).
    pub(super) fn get_by_thread_id(&self, thread_id: &str) -> Option<RunHandle> {
        let run_id = self.by_thread_id.read().get(thread_id).cloned()?;
        self.by_run_id.read().get(&run_id).cloned()
    }

    /// Look up a handle by control id with ambiguity detection.
    ///
    /// If `id` matches both a `run_id` and a `thread_id` that point to
    /// different runs, returns `Ambiguous`.
    pub(super) fn lookup_strict(&self, id: &str) -> HandleLookup {
        let by_run = self.by_run_id.read();
        let by_thread = self.by_thread_id.read();

        let by_run_hit = by_run.get(id).cloned();
        let by_thread_hit = by_thread
            .get(id)
            .and_then(|run_id| by_run.get(run_id))
            .cloned();

        match (by_run_hit, by_thread_hit) {
            (None, None) => HandleLookup::NotFound,
            (Some(handle), None) | (None, Some(handle)) => HandleLookup::Found(handle),
            (Some(run_handle), Some(thread_handle)) => {
                if run_handle.run_id == thread_handle.run_id {
                    HandleLookup::Found(run_handle)
                } else {
                    HandleLookup::Ambiguous
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::CancellationToken;
    use awaken_contract::contract::suspension::ToolCallResume;
    use futures::channel::mpsc;

    fn make_handle(run_id: &str) -> RunHandle {
        let token = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded::<Vec<(String, ToolCallResume)>>();
        RunHandle {
            run_id: run_id.to_string(),
            cancellation_token: token,
            decision_tx: tx,
        }
    }

    #[test]
    fn register_and_lookup_by_run_id() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1");
        assert!(reg.register("r1", "t1", handle));
        assert!(reg.get_by_run_id("r1").is_some());
        assert!(reg.get_by_run_id("unknown").is_none());
    }

    #[test]
    fn register_and_lookup_by_thread_id() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1");
        assert!(reg.register("r1", "t1", handle));
        assert!(reg.get_by_thread_id("t1").is_some());
        assert!(reg.get_by_thread_id("unknown").is_none());
    }

    #[test]
    fn strict_lookup_dual_index_hit() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1");
        assert!(reg.register("r1", "t1", handle));
        // By run_id
        assert!(matches!(reg.lookup_strict("r1"), HandleLookup::Found(_)));
        // By thread_id
        assert!(matches!(reg.lookup_strict("t1"), HandleLookup::Found(_)));
        // Unknown
        assert!(matches!(
            reg.lookup_strict("unknown"),
            HandleLookup::NotFound
        ));
    }

    #[test]
    fn strict_lookup_detects_id_ambiguity() {
        let reg = ActiveRunRegistry::new();
        assert!(reg.register("r1", "t1", make_handle("r1")));
        assert!(reg.register("t1", "t2", make_handle("t1")));

        match reg.lookup_strict("t1") {
            HandleLookup::Ambiguous => {}
            HandleLookup::Found(_) => panic!("lookup should be ambiguous"),
            HandleLookup::NotFound => panic!("lookup should not be missing"),
        }
    }

    #[test]
    fn duplicate_thread_rejected() {
        let reg = ActiveRunRegistry::new();
        let h1 = make_handle("r1");
        let h2 = make_handle("r2");
        assert!(reg.register("r1", "t1", h1));
        assert!(!reg.register("r2", "t1", h2));
    }

    #[test]
    fn duplicate_run_id_rejected() {
        let reg = ActiveRunRegistry::new();
        let h1 = make_handle("r1");
        let h2 = make_handle("r1");
        assert!(reg.register("r1", "t1", h1));
        assert!(!reg.register("r1", "t2", h2));
        assert!(reg.get_by_thread_id("t2").is_none());
    }

    #[test]
    fn unregister_removes_both_indices() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1");
        assert!(reg.register("r1", "t1", handle));
        reg.unregister("r1");
        assert!(reg.get_by_run_id("r1").is_none());
        assert!(reg.get_by_thread_id("t1").is_none());
    }
}
