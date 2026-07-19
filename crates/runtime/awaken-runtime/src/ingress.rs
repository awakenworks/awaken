//! Run ingress: the delivery seam between a caller and the runtime.
//!
//! `RunIngress` has exactly two delivery semantics (G5): direct in-process
//! execution and durable dispatch. This MVP ships `DirectRunIngress` only;
//! durable-only operations fail closed on it. Protocol adapters build the
//! `RunActivation` (no DTO leakage) and `RuntimeRunContext` (no durable data)
//! before calling submit (G19).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error, Result, RunExecutor};
use awaken_runtime_contract::resume::ResumeCommand;
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::runtime::Runtime;

/// The unified application interface for one Run.
///
/// Protocol adapters and parent Runs use these same domain operations. Creation
/// may produce a root activation or a child activation with a
/// `DelegationOrigin`, but after admission both follow this interface and the
/// same Runtime state machine.
#[async_trait]
pub trait RunService: Send + Sync {
    /// Start a Run from immutable activation data.
    async fn start(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState>;

    /// Resume a Run from its committed ticket with a typed response.
    async fn resume(&self, command: ResumeCommand, context: RuntimeRunContext) -> Result<RunState>;

    /// Cancel an in-flight Run by id.
    fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError>;
}

/// Delivery capabilities owned by the ingress bounded context. This extends the
/// Run API only with fire-and-forget durable admission; ordinary callers depend
/// on [`RunService`], not this infrastructure-specific operation.
#[async_trait]
pub trait RunIngress: RunService {
    /// Durable, fire-and-forget submission. Direct ingress fails closed (G5).
    async fn submit_background(&self, activation: RunActivation) -> Result<RunState>;
}

/// In-process ingress: runs execute inline on the calling task.
#[derive(Clone)]
pub struct DirectRunIngress {
    runtime: Arc<Runtime>,
}

impl DirectRunIngress {
    pub fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl RunService for DirectRunIngress {
    async fn start(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunState> {
        self.runtime.execute(activation, context).await
    }

    async fn resume(&self, command: ResumeCommand, context: RuntimeRunContext) -> Result<RunState> {
        let reader = context.reader.clone().ok_or_else(|| {
            Error::Execution("RunService::resume requires committed-history wiring".to_string())
        })?;
        self.runtime.resume(command, reader.as_ref(), context).await
    }

    fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError> {
        self.runtime.deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}

#[async_trait]
impl RunIngress for DirectRunIngress {
    async fn submit_background(&self, _activation: RunActivation) -> Result<RunState> {
        Err(Error::Execution(
            "direct ingress does not support durable background submission".to_string(),
        ))
    }
}
