// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: The Host's resume assertion: an answer names the Run's exact pending tool and its kind
// @formal reason: before a resume is delivered. Classification, not a formal-proof claim.
//! Resume assertions at the Host boundary.

use super::*;

impl crate::SessionRuns {
    /// Fail closed before resuming: the asserted `tool_use_id` must name the
    /// Run's pending tool and its binding must match the inbound resume kind.
    pub(in crate::host) fn check_pending(
        &self,
        ticket: &ResumeTicket,
        tool_use_id: &str,
        want_client: bool,
    ) -> Result<(), SessionExecutionError> {
        let pending = Pending::from_resume_ticket(ticket)
            .ok_or_else(|| SessionExecutionError::internal("awaiting run has no pending tool"))?;
        if pending.tool_use_id != tool_use_id {
            return Err(SessionExecutionError::bad_request(format!(
                "tool_use_id {tool_use_id:?} does not match the pending tool"
            )));
        }
        if pending.client_executed != want_client {
            let (got, expected) = if want_client {
                ("built-in", "a confirmation")
            } else {
                ("client-executed", "a client tool result")
            };
            return Err(SessionExecutionError::bad_request(format!(
                "pending tool is {got}; answer it with {expected}"
            )));
        }
        Ok(())
    }
}
