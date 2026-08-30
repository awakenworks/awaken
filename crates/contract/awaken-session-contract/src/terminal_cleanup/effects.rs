use super::*;


impl SessionCleanupCommand {
    #[must_use]
    pub fn new(session_id: &str, thread_id: &str, root_effect_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            effect_id: crate::stable_fingerprint(&(
                "session-terminal-cleanup-thread-v1",
                session_id,
                thread_id,
                root_effect_id,
            )),
            restore_target: None,
        }
    }

    pub fn with_restore_target(
        mut self,
        request: crate::SandboxRestoreRequest,
    ) -> Result<Self, SessionCleanupError> {
        if self.thread_id != self.session_id || request.session_id != self.session_id {
            return Err(SessionCleanupError::OperationMismatch);
        }
        self.restore_target = Some(request);
        Ok(self)
    }
}
