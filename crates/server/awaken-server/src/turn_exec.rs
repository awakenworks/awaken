//! Turn-execution routing (R3/R4): drive a thread's activation on the native
//! ingress or, when the session selected an ACP runtime, on the ACP executor.
//!
//! Both commit through the thread's coordinator and return a `Phase`, so the
//! caller's `finish_step` projection is identical either way — the ACP brain is a
//! peer `RunExecutor`, not a parallel code path.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Phase;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;

use crate::host::{HostError, SessionCtx, SharedHost};

impl SharedHost {
    /// Execute `activation` for `thread`: the ACP executor when the session chose
    /// an ACP runtime, else the native ingress (direct / durable / superseding).
    pub(crate) async fn execute_activation(
        &self,
        ctx: &Arc<SessionCtx>,
        thread: &str,
        activation: RunActivation,
        supersede: bool,
    ) -> Result<Phase, HostError> {
        // R3/R4: an ACP-selected thread runs on the external CLI (relaunched per
        // turn — R7), committing through the same coordinator as the native path.
        if let Some(acp) = &self.acp
            && acp.is_acp(thread)
        {
            return acp
                .executor
                .execute(activation, ctx.context())
                .await
                .map_err(|e| HostError::internal(e.to_string()));
        }
        if supersede {
            ctx.durable_ingress
                .as_ref()
                .expect("supersede requires durable ingress")
                .submit_superseding(activation)
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        } else if ctx.durable {
            ctx.ingress
                .submit_background(activation)
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        } else {
            ctx.ingress
                .submit(activation, ctx.context())
                .await
                .map_err(|e| HostError::internal(e.to_string()))
        }
    }
}
