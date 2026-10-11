// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: SDK approval orchestration consumes committed tickets through
// @formal reason: the existing resume and cancellation authority; this is not a new proof claim.
//! Shared synchronous/asynchronous approval orchestration over committed tickets.

use crate::{ConfigurationError, Runtime};
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::diagnostic::{RuntimeDiagnostic, RuntimeErrorKind};
use awaken_runtime_contract::execution::{Error, RunExecutor};
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::terminal::{
    CommittedTerminalProjection, committed_terminal_projection, deliver_committed_terminal,
};
use awaken_time::{WallClock, WallClockExt};
use std::{future::Future, sync::Arc, time::Duration};

/// A failed approval wait leaves the committed ticket available for recovery.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApprovalError {
    #[error("approval timed out; the Run remains Awaiting and can be resumed")]
    TimedOut,
    #[error("approval callback failed: {0}")]
    Failed(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("awaiting run has no current committed ticket")]
    MissingTicket,
    #[error("stable Run identity belongs to another Thread")]
    IdentityConflict,
    #[error("committed Run changed while reading its terminal identity")]
    InconsistentRecord,
}

impl ApprovalError {
    pub fn failed(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Failed(Box::new(source))
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::TimedOut => "approval_timeout",
            Self::Failed(_) => "approval_failed",
            Self::MissingTicket => "resume_ticket_missing",
            Self::IdentityConflict => "run_identity_conflict",
            Self::InconsistentRecord => "inconsistent_run_record",
        }
    }
}

impl From<ApprovalError> for Error {
    fn from(error: ApprovalError) -> Self {
        let kind = match &error {
            ApprovalError::TimedOut | ApprovalError::Failed(_) => RuntimeErrorKind::Unavailable,
            ApprovalError::MissingTicket | ApprovalError::IdentityConflict => {
                RuntimeErrorKind::Conflict
            }
            ApprovalError::InconsistentRecord => RuntimeErrorKind::Internal,
        };
        RuntimeDiagnostic::new(kind, error.code(), error).into()
    }
}

/// Timeout applies to each callback wait, not the Run or ticket lifetime.
#[derive(Clone)]
pub struct ApprovalOptions {
    timeout: Option<Duration>,
    clock: Arc<dyn WallClock>,
}

impl Default for ApprovalOptions {
    fn default() -> Self {
        Self {
            timeout: None,
            clock: awaken_time::system_clock(),
        }
    }
}

impl ApprovalOptions {
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn WallClock>) -> Self {
        self.clock = clock;
        self
    }
}

pub(crate) async fn drive_to_completion<F, Fut>(
    runtime: &Runtime,
    run_id: RunId,
    activation: RunActivation,
    context: RuntimeRunContext,
    decide: &mut F,
    options: &ApprovalOptions,
) -> Result<RunState, Error>
where
    F: FnMut(ResumeTicket) -> Fut,
    Fut: Future<Output = Result<ResumeResult, ApprovalError>>,
{
    crate::configuration::validate_configuration(runtime)?;
    let reader = context
        .reader
        .as_deref()
        .ok_or(ConfigurationError::MissingHistoryReader)?;
    let thread_id = activation.thread_id.clone();
    let mut state = match committed_terminal_projection(reader, &run_id, &thread_id) {
        CommittedTerminalProjection::Exact(terminal) => {
            let _ = deliver_committed_terminal(&context.terminal_observers, &terminal).await;
            return Ok(RunState::Ended(terminal.cause));
        }
        CommittedTerminalProjection::IdentityConflict => {
            return Err(ApprovalError::IdentityConflict.into());
        }
        CommittedTerminalProjection::Nonterminal => match reader.run_state(&run_id) {
            Some(RunState::Awaiting) => {
                let ticket = reader
                    .resume_ticket(&run_id)
                    .ok_or(ApprovalError::MissingTicket)?;
                if ticket.snapshot_id != activation.snapshot.id.0 {
                    return Err(
                        awaken_runtime_contract::resume::ResumeError::SnapshotMismatch.into(),
                    );
                }
                if ticket.catalog_fingerprint != activation.snapshot.fingerprint.0 {
                    return Err(
                        awaken_runtime_contract::resume::ResumeError::FingerprintMismatch.into(),
                    );
                }
                crate::configuration::validate_for(runtime, &activation.snapshot, &context)?;
                runtime.register_snapshot(activation.snapshot.clone());
                RunState::Awaiting
            }
            Some(RunState::Running) | None => {
                crate::configuration::validate_for(runtime, &activation.snapshot, &context)?;
                runtime.execute(activation, context.clone()).await?
            }
            Some(RunState::Ended(_)) => return Err(ApprovalError::InconsistentRecord.into()),
        },
    };
    while state == RunState::Awaiting {
        let ticket = reader
            .resume_ticket(&run_id)
            .ok_or(ApprovalError::MissingTicket)?;
        if context
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return crate::engine::run_commands::cancel_run(
                runtime,
                run_id,
                thread_id,
                context.clone(),
            )
            .await;
        }
        let callback = decide(ticket.clone());
        let wait = async {
            match options.timeout {
                Some(timeout) => tokio::time::timeout(timeout, callback)
                    .await
                    .map_err(|_| ApprovalError::TimedOut)?,
                None => callback.await,
            }
        };
        let cancel = async {
            match context.cancellation.as_ref() {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        };
        let answer = tokio::select! {
            biased;
            _ = cancel => return crate::engine::run_commands::cancel_run(runtime, run_id, thread_id, context.clone()).await,
            answer = wait => answer?,
        };
        let command = ResumeCommand::from_ticket(&ticket, answer, options.clock.now_ms());
        state = runtime.resume(command, reader, context.clone()).await?;
    }
    Ok(state)
}
