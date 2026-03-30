//! Control methods: cancel, send_decisions — with dual-index lookup (run_id + thread_id).

use awaken_contract::contract::suspension::ToolCallResume;

use super::AgentRuntime;
use super::active_registry::HandleLookup;

impl AgentRuntime {
    /// Cancel an active run by thread ID.
    pub fn cancel_by_thread(&self, thread_id: &str) -> bool {
        if let Some(handle) = self.active_runs.get_by_thread_id(thread_id) {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel an active run by run ID.
    pub fn cancel_by_run_id(&self, run_id: &str) -> bool {
        if let Some(handle) = self.active_runs.get_by_run_id(run_id) {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel an active run by dual-index ID (run_id or thread_id).
    /// Ambiguous IDs are rejected.
    pub fn cancel(&self, id: &str) -> bool {
        match self.active_runs.lookup_strict(id) {
            HandleLookup::Found(handle) => {
                handle.cancel();
                true
            }
            HandleLookup::NotFound => false,
            HandleLookup::Ambiguous => {
                tracing::warn!(id = %id, "cancel rejected: ambiguous control id");
                false
            }
        }
    }

    /// Send decisions to an active run by thread ID.
    pub fn send_decisions(
        &self,
        thread_id: &str,
        decisions: Vec<(String, ToolCallResume)>,
    ) -> bool {
        if let Some(handle) = self.active_runs.get_by_thread_id(thread_id) {
            if handle.send_decisions(decisions).is_err() {
                tracing::warn!(
                    thread_id = %thread_id,
                    "send_decisions failed: channel closed"
                );
                return false;
            }
            true
        } else {
            false
        }
    }

    /// Send a decision by dual-index ID (run_id or thread_id).
    /// Ambiguous IDs are rejected.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        match self.active_runs.lookup_strict(id) {
            HandleLookup::Found(handle) => handle.send_decision(tool_call_id, resume).is_ok(),
            HandleLookup::NotFound => false,
            HandleLookup::Ambiguous => {
                tracing::warn!(id = %id, "send_decision rejected: ambiguous control id");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};
    use serde_json::json;
    use std::sync::Arc;

    use crate::error::RuntimeError;
    use crate::registry::{AgentResolver, ResolvedAgent};

    struct StubResolver;
    impl AgentResolver for StubResolver {
        fn resolve(&self, _agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::ResolveFailed {
                message: "stub".into(),
            })
        }
    }

    fn make_runtime() -> AgentRuntime {
        AgentRuntime::new(Arc::new(StubResolver))
    }

    fn make_resume() -> ToolCallResume {
        ToolCallResume {
            decision_id: "d1".into(),
            action: ResumeDecisionAction::Resume,
            result: json!(null),
            reason: None,
            updated_at: 0,
        }
    }

    // -- cancel_by_run_id --

    #[test]
    fn cancel_by_run_id_returns_true_when_registered() {
        let rt = make_runtime();
        let (handle, _token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.cancel_by_run_id("r1"));
    }

    #[test]
    fn cancel_by_run_id_returns_false_when_not_found() {
        let rt = make_runtime();
        assert!(!rt.cancel_by_run_id("nonexistent"));
    }

    #[test]
    fn cancel_by_run_id_signals_cancellation_token() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(!token.is_cancelled());
        rt.cancel_by_run_id("r1");
        assert!(token.is_cancelled());
    }

    // -- cancel_by_thread --

    #[test]
    fn cancel_by_thread_returns_true_when_registered() {
        let rt = make_runtime();
        let (handle, _token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.cancel_by_thread("t1"));
    }

    #[test]
    fn cancel_by_thread_returns_false_when_not_found() {
        let rt = make_runtime();
        assert!(!rt.cancel_by_thread("nonexistent"));
    }

    #[test]
    fn cancel_by_thread_signals_cancellation_token() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(!token.is_cancelled());
        rt.cancel_by_thread("t1");
        assert!(token.is_cancelled());
    }

    // -- cancel (dual-index) --

    #[test]
    fn cancel_by_run_id_via_dual_index() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.cancel("r1"));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_by_thread_id_via_dual_index() {
        let rt = make_runtime();
        let (handle, token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.cancel("t1"));
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_returns_false_for_unknown_id() {
        let rt = make_runtime();
        assert!(!rt.cancel("unknown"));
    }

    #[test]
    fn cancel_returns_false_for_ambiguous_id() {
        let rt = make_runtime();
        // Register two runs where thread_id of first == run_id of second
        let (h1, _t1, _rx1) = rt.create_run_channels("r1".into());
        rt.register_run("shared", h1).unwrap();
        let (h2, _t2, _rx2) = rt.create_run_channels("shared".into());
        rt.register_run("t2", h2).unwrap();

        // "shared" matches both as thread_id (-> r1) and run_id (-> shared), different runs
        assert!(!rt.cancel("shared"));
    }

    // -- send_decisions --

    #[test]
    fn send_decisions_returns_true_and_delivers() {
        let rt = make_runtime();
        let (handle, _token, mut rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        let resume = make_resume();
        assert!(rt.send_decisions("t1", vec![("tc1".into(), resume)]));

        // Verify delivery
        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "tc1");
    }

    #[test]
    fn send_decisions_returns_false_for_unknown_thread() {
        let rt = make_runtime();
        assert!(!rt.send_decisions("unknown", vec![("tc1".into(), make_resume())]));
    }

    #[test]
    fn send_decisions_returns_false_when_channel_closed() {
        let rt = make_runtime();
        let (handle, _token, rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        // Drop receiver to close the channel
        drop(rx);

        assert!(!rt.send_decisions("t1", vec![("tc1".into(), make_resume())]));
    }

    // -- send_decision (dual-index) --

    #[test]
    fn send_decision_by_run_id() {
        let rt = make_runtime();
        let (handle, _token, mut rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.send_decision("r1", "tc1".into(), make_resume()));

        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].0, "tc1");
    }

    #[test]
    fn send_decision_by_thread_id() {
        let rt = make_runtime();
        let (handle, _token, mut rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();

        assert!(rt.send_decision("t1", "tc1".into(), make_resume()));

        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn send_decision_returns_false_for_unknown_id() {
        let rt = make_runtime();
        assert!(!rt.send_decision("unknown", "tc1".into(), make_resume()));
    }

    #[test]
    fn send_decision_returns_false_for_ambiguous_id() {
        let rt = make_runtime();
        let (h1, _t1, _rx1) = rt.create_run_channels("r1".into());
        rt.register_run("shared", h1).unwrap();
        let (h2, _t2, _rx2) = rt.create_run_channels("shared".into());
        rt.register_run("t2", h2).unwrap();

        assert!(!rt.send_decision("shared", "tc1".into(), make_resume()));
    }

    #[test]
    fn send_decision_returns_false_when_channel_closed() {
        let rt = make_runtime();
        let (handle, _token, rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();
        drop(rx);

        assert!(!rt.send_decision("r1", "tc1".into(), make_resume()));
    }

    // -- cancel after unregister --

    #[test]
    fn cancel_after_unregister_returns_false() {
        let rt = make_runtime();
        let (handle, _token, _rx) = rt.create_run_channels("r1".into());
        rt.register_run("t1", handle).unwrap();
        rt.unregister_run("r1");

        assert!(!rt.cancel("r1"));
        assert!(!rt.cancel("t1"));
    }
}
