//! Run-execution routing (R3/R4): drive a thread's activation on the native
//! ingress or, when the session selected an ACP runtime, on the ACP executor.
//!
//! Both commit through the thread's coordinator and return a `Phase`, so the
//! caller's `finish_step` projection is identical either way — the ACP brain is a
//! peer `RunExecutor`, not a parallel code path.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Phase;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;

use crate::host::{HostError, SessionCtx, SharedHost};

impl SharedHost {
    /// Execute `activation` for `thread`: the ACP executor when the session chose
    /// an ACP runtime, else the native ingress (direct / durable / superseding).
    ///
    /// `sink`, when set, receives the engine's best-effort live progress — only
    /// the in-process direct path wires it (the durable/ACP paths run elsewhere
    /// and simply omit live events, degrading to the committed projection).
    pub(crate) async fn execute_activation(
        &self,
        ctx: &Arc<SessionCtx>,
        thread: &str,
        activation: RunActivation,
        supersede: bool,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<Phase, HostError> {
        // R3/R4: an ACP-selected thread runs on the external CLI (relaunched per
        // turn — R7), committing through the same coordinator as the native path.
        if let Some(acp) = &self.acp
            && acp.is_acp(thread)
        {
            // The ACP executor runs inline under this session ctx, so it can drain
            // the same live inbox the offer side reaches (ADR-0054 P4): wire it so
            // steer/redirect works for external-CLI runs, then close it on return.
            let context = ctx
                .context_for(&activation)
                .await
                .with_live_inbox(ctx.open_live_inbox());
            let result = acp
                .executor
                .execute(activation, context)
                .await
                .map_err(|e| HostError::internal(e.to_string()));
            ctx.close_live_inbox();
            return result;
        }
        // Fail closed: a cloud-managed gateway grant needs the mediated egress an ACP
        // CLI in a sandbox gets. The native in-process provider path builds its
        // `LlmExecutor` from the catalog, not the per-run grant, so it cannot honor a
        // gateway grant — and must reject rather than degrade to local credentials,
        // which would defeat the secret-custody the grant exists to enforce (ADR-0004).
        // (When the native path learns to dial the grant's endpoint, this becomes a
        // materialize-and-honor instead of a rejection.)
        if activation.model_access.is_gateway() {
            return Err(HostError::bad_request(
                "native runtime cannot honor a cloud-managed gateway grant; \
                 select an ACP runtime for gateway-mediated egress",
            ));
        }
        if supersede {
            // Durable + superseding: enqueue (marking prior pending superseded) and
            // let the process pool drive it on this session's worker (O2).
            self.submit_durable_foreground(ctx, activation, true).await
        } else if ctx.durable {
            // Durable: enqueue and await the pool driving it to a settled phase. The
            // session's own worker must not claim (it would grab foreign threads'
            // runs on the shared queue); the pool is the sole claimer.
            self.submit_durable_foreground(ctx, activation, false).await
        } else {
            // Native direct turn: the only path whose engine drains a live
            // inbox in-process, so it is the only path that opens one. The
            // inbox closes when the attempt returns — success or error — and
            // unconsumed messages carry over to the thread's next attempt.
            let mut context = ctx
                .context_for(&activation)
                .await
                .with_live_inbox(ctx.open_live_inbox());
            if let Some(sink) = sink {
                context = context.with_stream_sink(sink);
            }
            let result = ctx
                .ingress
                .submit(activation, context)
                .await
                .map_err(|e| HostError::internal(e.to_string()));
            ctx.close_live_inbox();
            result
        }
    }
}
