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

use crate::runtime::Runtime;

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
        let _thread_execution = self
            .runtime
            .acquire_thread_execution(&activation.thread_id)
            .await;
        let attempt = self.runtime.begin_active_attempt(
            &activation.run_id,
            &activation.thread_id,
            context,
            self.executor.capabilities().live_input,
        );
        self.executor
            .execute(activation, attempt.context().clone())
            .await
    }

    pub async fn resume(
        &self,
        activation: RunActivation,
        command: ResumeCommand,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        let _thread_execution = self
            .runtime
            .acquire_thread_execution(&activation.thread_id)
            .await;
        let attempt = self.runtime.begin_active_attempt(
            &activation.run_id,
            &activation.thread_id,
            context,
            self.executor.capabilities().live_input,
        );
        self.executor
            .resume(activation, command, attempt.context().clone())
            .await
    }

    pub async fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError> {
        self.runtime.deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}
