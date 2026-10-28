// @formal entrypoint acp.process_cleanup_requires_observed_exit_and_retained_owner
//! Retain the current ACP process inside its original execution Future.
//! Dispatch owns durable occupancy; this scope owns only the existing handle
//! and duplex channel, including after a bounded cleanup attempt failed.

use super::{AcpError, AgentChannelSource, AgentSession, OpenError, Supervisor};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::runtime_context::{AttemptQuiescence, RuntimeRunContext};
use std::time::Duration;

/// Proof that a channel open was reached through [`open_session`], the one door
/// that wraps the launch in the attempt's canonical return observer.
///
/// The tuple field is private, so the only expression anywhere that can build a
/// door is the one inside [`open_session`] below. That is what turns "call
/// `open` only through the cleanup door" from a comment into a rule: an
/// implementor of [`AgentChannelSource`] can receive a door and forward it to an
/// inner source, and nobody else can conjure one.
///
/// ```compile_fail,E0603
/// use awaken_run_executor_acp::ChannelOpenDoor;
///
/// let _ = ChannelOpenDoor(());
/// ```
#[derive(Debug)]
pub struct ChannelOpenDoor(());

/// Open one agent channel. **The only caller of [`AgentChannelSource::open`].**
///
/// `open` crosses the provider launch boundary. A cancelled caller can drop that
/// Future after the provider started a process but before its handle reached
/// `CleanupScope`, so the launch must be wrapped in the attempt's canonical
/// return observer for such a lost launch observer to permanently veto an ACK.
/// A second caller that skipped the observer would strand exactly that process,
/// which is why the door above makes a second caller unwritable rather than
/// merely discouraged.
pub async fn open_session(
    source: &dyn AgentChannelSource,
    activation: &RunActivation,
    context: &RuntimeRunContext,
) -> std::result::Result<AgentSession, OpenError> {
    // Normal return transfers the typed owner to the scope below.
    context
        .physical_quiescence
        .observe_return(source.open(activation, context, ChannelOpenDoor(())))
        .await
}

/// Reap the per-Step SDK process after any business outcome. A terminal frame
/// is not physical exit; a cleanup failure retains this same typed owner.
pub(super) async fn finish_step(
    session: &mut AgentSession,
    grace: Duration,
    outcome: std::result::Result<super::TerminationReason, AcpError>,
) -> (
    std::result::Result<super::TerminationReason, AcpError>,
    bool,
) {
    let started = awaken_time::mono::Stopwatch::start();
    let reaped =
        Supervisor::reap_after_terminal(session.channel.as_mut(), session.process.as_ref(), grace)
            .await;
    let complete = reaped.is_ok();
    tracing::info!(
        process_id = session.process.id(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        complete,
        phase = "sdk_reap",
        "ACP Step cleanup observed"
    );
    (
        preserve_terminal_outcome(outcome, reaped, session.process.id()),
        complete,
    )
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
