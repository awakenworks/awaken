use std::sync::Arc;

use async_trait::async_trait;

use crate::PhaseContext;
use awaken_contract::StateError;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;

#[async_trait]
pub trait ToolGateHook: Send + Sync + 'static {
    async fn run(&self, ctx: &PhaseContext) -> Result<Option<ToolInterceptPayload>, StateError>;
}

pub(crate) type ToolGateHookArc = Arc<dyn ToolGateHook>;
