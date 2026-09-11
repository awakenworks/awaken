//! Retain the current ACP process inside its original execution Future.
//! Dispatch owns durable occupancy; this scope owns only the existing handle
//! and duplex channel, including after a bounded cleanup attempt failed.

use super::{AcpError, AgentChannelSource, AgentSession, OpenError, Supervisor};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::runtime_context::{AttemptQuiescence, RuntimeRunContext};
use std::time::Duration;

pub(super) async fn open_session(
    source: &dyn AgentChannelSource,
    activation: &RunActivation,
    context: &RuntimeRunContext,
) -> std::result::Result<AgentSession, OpenError> {
    // `AgentChannelSource::open` crosses the provider launch boundary. A
    // cancelled caller can drop that Future after the provider started a
    // process but before its handle reached `CleanupScope`. Reuse the attempt's
    // canonical return observer so such a lost launch observer permanently
    // vetoes an ACK. Normal return transfers the typed owner to the scope below.
    context
        .physical_quiescence
        .observe_return(source.open(activation, context))
        .await
}

pub(super) fn preserve_terminal_outcome<T>(
    outcome: std::result::Result<T, AcpError>,
    cleanup: std::result::Result<bool, AcpError>,
    process_id: &str,
) -> std::result::Result<T, AcpError> {
    if let Err(cleanup_error) = cleanup {
        tracing::warn!(
            process_id,
            error = %cleanup_error,
            "ACP terminal process cleanup failed after the Step outcome was fixed"
        );
    }
    outcome
}

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
