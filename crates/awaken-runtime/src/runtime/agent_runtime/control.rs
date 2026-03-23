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
