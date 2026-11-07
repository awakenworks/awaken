// @formal entrypoint live_inbox.attempt_scope_is_local_current_capability_gated_and_never_carried_over
// @formal entrypoint session_execution.one_physical_attempt_per_thread
//! Direct physical-attempt delivery.
//!
//! Durable admission is owned by `RunDispatch` and the dispatch worker. This
//! concrete driver owns only queue-less, in-process delivery into an exact
//! [`RunAttemptExecutor`] and therefore does not pretend to share a service
//! interface with durable enqueue/recovery.

use std::sync::Arc;

use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Result, RunAttemptExecutor};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::runtime::{Runtime, drive_direct_attempt};

/// In-process attempt driver: runs execute inline on the calling task.
#[derive(Clone)]
pub struct DirectAttemptDriver {
    runtime: Arc<Runtime>,
    executor: Arc<dyn RunAttemptExecutor>,
}

impl DirectAttemptDriver {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        let executor: Arc<dyn RunAttemptExecutor> = runtime.clone();
        Self { runtime, executor }
    }

    /// Run inline through a session-selected attempt executor while retaining the
    /// native runtime as the live cancellation authority. This is the direct
    /// counterpart of durable dispatch's replaceable attempt executor.
    #[must_use]
    pub fn with_attempt_executor(
        runtime: Arc<Runtime>,
        executor: Arc<dyn RunAttemptExecutor>,
    ) -> Self {
        Self { runtime, executor }
    }

    pub async fn start(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        self.drive(activation, None, context).await
    }

    pub async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        self.drive(activation, Some(command), context).await
    }

    async fn drive(
        &self,
        activation: RunActivation,
        command: Option<ResumeCommand>,
        mut context: RuntimeRunContext,
    ) -> Result<RunState> {
        // Activated admission owns a new physical attempt and its evidence.
        context.physical_quiescence = Default::default();
        let run_id = activation.run_id.clone();
        let thread_id = activation.thread_id.clone();
        drive_direct_attempt(
            &self.runtime,
            &run_id,
            &thread_id,
            context,
            self.executor.capabilities().live_input,
            |context| async move {
                match command {
                    Some(command) => self.executor.resume(activation, command, context).await,
                    None => self.executor.execute(activation, context).await,
                }
            },
        )
        .await
    }

    pub async fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError> {
        self.runtime.deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}
