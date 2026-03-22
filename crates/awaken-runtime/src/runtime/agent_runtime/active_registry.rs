//! ActiveRunRegistry: tracks active runs with dual indexing (run_id + thread_id).

use std::collections::HashMap;

use parking_lot::RwLock;

use super::RunHandle;

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
    /// Returns `false` if the thread already has an active run.
    pub(super) fn register(&self, run_id: &str, thread_id: &str, handle: RunHandle) -> bool {
        let mut by_thread = self.by_thread_id.write();
        if by_thread.contains_key(thread_id) {
            return false;
        }
        by_thread.insert(thread_id.to_string(), run_id.to_string());
        drop(by_thread);

        self.by_run_id.write().insert(run_id.to_string(), handle);
        true
    }

    /// Unregister a run by run_id. Removes both run_id and thread_id mappings.
    pub(super) fn unregister(&self, run_id: &str) {
        self.by_run_id.write().remove(run_id);
        self.by_thread_id.write().retain(|_, v| v != run_id);
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

    /// Look up a handle by trying run_id first, then thread_id.
    pub(super) fn get_handle(&self, id: &str) -> Option<RunHandle> {
        self.get_by_run_id(id).or_else(|| self.get_by_thread_id(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::cancellation::CancellationToken;
    use awaken_contract::contract::suspension::ToolCallResume;
    use futures::channel::mpsc;

    fn make_handle(run_id: &str, thread_id: &str) -> RunHandle {
        let token = CancellationToken::new();
        let (tx, _rx) = mpsc::unbounded::<(String, ToolCallResume)>();
        RunHandle {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "agent".to_string(),
            cancellation_token: token,
            decision_tx: tx,
        }
    }

    #[test]
    fn register_and_lookup_by_run_id() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1", "t1");
        assert!(reg.register("r1", "t1", handle));
        assert!(reg.get_by_run_id("r1").is_some());
        assert!(reg.get_by_run_id("unknown").is_none());
    }

    #[test]
    fn register_and_lookup_by_thread_id() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1", "t1");
        assert!(reg.register("r1", "t1", handle));
        assert!(reg.get_by_thread_id("t1").is_some());
        assert!(reg.get_by_thread_id("unknown").is_none());
    }

    #[test]
    fn get_handle_dual_lookup() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1", "t1");
        assert!(reg.register("r1", "t1", handle));
        // By run_id
        assert!(reg.get_handle("r1").is_some());
        // By thread_id
        assert!(reg.get_handle("t1").is_some());
        // Unknown
        assert!(reg.get_handle("unknown").is_none());
    }

    #[test]
    fn duplicate_thread_rejected() {
        let reg = ActiveRunRegistry::new();
        let h1 = make_handle("r1", "t1");
        let h2 = make_handle("r2", "t1");
        assert!(reg.register("r1", "t1", h1));
        assert!(!reg.register("r2", "t1", h2));
    }

    #[test]
    fn unregister_removes_both_indices() {
        let reg = ActiveRunRegistry::new();
        let handle = make_handle("r1", "t1");
        assert!(reg.register("r1", "t1", handle));
        reg.unregister("r1");
        assert!(reg.get_by_run_id("r1").is_none());
        assert!(reg.get_by_thread_id("t1").is_none());
    }
}
