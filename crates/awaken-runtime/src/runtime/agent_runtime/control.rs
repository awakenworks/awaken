//! Control methods: cancel, send_decisions — with dual-index lookup (run_id + thread_id).

use awaken_contract::contract::suspension::ToolCallResume;

use super::AgentRuntime;

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

    /// Cancel an active run, trying run_id first, then thread_id.
    pub fn cancel(&self, id: &str) -> bool {
        if let Some(handle) = self.active_runs.get_handle(id) {
            handle.cancel();
            true
        } else {
            false
        }
    }

    /// Send decisions to an active run by thread ID.
    pub fn send_decisions(
        &self,
        thread_id: &str,
        decisions: Vec<(String, ToolCallResume)>,
    ) -> bool {
        if let Some(handle) = self.active_runs.get_by_thread_id(thread_id) {
            let total = decisions.len();
            let mut sent = 0;
            for (call_id, resume) in decisions {
                if handle.send_decision(call_id, resume).is_err() {
                    tracing::warn!(
                        thread_id = %thread_id,
                        sent,
                        total,
                        "send_decisions partial failure — channel closed"
                    );
                    return false;
                }
                sent += 1;
            }
            true
        } else {
            false
        }
    }

    /// Send a decision to an active run, trying run_id first, then thread_id.
    pub fn send_decision(&self, id: &str, tool_call_id: String, resume: ToolCallResume) -> bool {
        if let Some(handle) = self.active_runs.get_handle(id) {
            handle.send_decision(tool_call_id, resume).is_ok()
        } else {
            false
        }
    }
}
