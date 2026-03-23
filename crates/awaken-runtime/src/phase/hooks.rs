use std::sync::Arc;

use async_trait::async_trait;

use crate::state::StateCommand;
use awaken_contract::StateError;

use super::PhaseContext;

#[async_trait]
pub trait PhaseHook: Send + Sync + 'static {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError>;
}

pub(crate) type PhaseHookArc = Arc<dyn PhaseHook>;
