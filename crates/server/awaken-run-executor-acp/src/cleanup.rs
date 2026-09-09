//! Retain the current ACP process inside its original execution Future.
//! Dispatch owns durable occupancy; this scope owns only the existing handle
//! and duplex channel, including after a bounded cleanup attempt failed.

use super::{AgentSession, Supervisor};
use awaken_runtime_contract::runtime_context::AttemptQuiescence;
use std::time::Duration;

pub(super) struct CleanupScope {
    pub session: Option<AgentSession>,
    pub complete: bool,
    quiescence: AttemptQuiescence,
}

impl CleanupScope {
    pub fn new(quiescence: AttemptQuiescence) -> Self {
        Self {
            session: None,
            complete: true,
            quiescence,
        }
    }

    pub async fn finish(&mut self, grace: Duration) {
        while !self.complete {
            let session = self.session.as_mut().expect("pending ACP process");
            // Reuse the sole bounded TERM/KILL/drain policy. Only this same
            // handle can report exit. Retry observation/cleanup, never a prompt,
            // tool effect, new process, or a historical Dispatch transition.
            if Supervisor::reap_after_terminal(
                session.channel.as_mut(),
                session.process.as_ref(),
                grace,
            )
            .await
            .is_ok()
            {
                self.complete = true;
            } else {
                tokio::time::sleep(grace.clamp(Duration::from_millis(25), Duration::from_secs(1)))
                    .await;
            }
        }
    }
}

impl Drop for CleanupScope {
    fn drop(&mut self) {
        if !self.complete {
            // Owner abortion still loses the observation. This only adds a
            // veto; successful cleanup never clears another child's uncertainty.
            self.quiescence.mark_uncertain();
        }
    }
}
