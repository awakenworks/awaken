//! Run ingress: the delivery seam between a caller and the runtime.
//!
//! `RunIngress` has exactly two delivery semantics (G5): direct in-process
//! execution and durable dispatch. This MVP ships `DirectRunIngress` only;
//! durable-only operations fail closed on it. Protocol adapters build the
//! `RunActivation` (no DTO leakage) and `RuntimeRunContext` (no durable data)
//! before calling submit (G19).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::control::{Error as ControlError, LiveCommand, LiveRunControl};
use awaken_runtime_contract::execution::{Error, Result, RunExecutor, RunOutcome};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::runtime::Runtime;

/// Submit and steer runs. Direct and durable implementations differ only by
/// delivery guarantees, never by execution model.
#[async_trait]
pub trait RunIngress: Send + Sync {
    /// Execute a run with caller-provided live wiring.
    async fn submit(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunOutcome>;

    /// Durable, fire-and-forget submission. Direct ingress fails closed (G5).
    async fn submit_background(&self, activation: RunActivation) -> Result<RunOutcome>;

    /// Cancel an in-flight run by id.
    fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError>;
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
impl RunIngress for DirectRunIngress {
    async fn submit(
        &self,
        activation: RunActivation,
        context: RuntimeRunContext,
    ) -> Result<RunOutcome> {
        self.runtime.execute(activation, context).await
    }

    async fn submit_background(&self, _activation: RunActivation) -> Result<RunOutcome> {
        Err(Error::Execution(
            "direct ingress does not support durable background submission".to_string(),
        ))
    }

    fn cancel(&self, run_id: &RunId) -> std::result::Result<(), ControlError> {
        self.runtime.deliver(LiveCommand::Cancel {
            run_id: run_id.clone(),
        })
    }
}
