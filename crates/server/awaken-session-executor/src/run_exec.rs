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
        // A cloud-managed gateway grant (ADR-0004): build the per-run executor that
        // dials the gateway with the lease token (fail-closed on no factory / an
        // unservable dialect); a local grant yields `None`.
        let gateway_executor = self.gateway_model_executor(&activation.model_access)?;
        // The gateway grant is honored on BOTH paths: the direct path uses the
        // per-run executor built above; the durable path enqueues, and the pool
        // worker rebuilds the same executor from the session's gateway builder
        // (ADR-0004, the durable-path half of the secretless worker). The early build
        // above also validates — a gateway grant with no factory or an unservable
        // dialect fails closed here, before the run is enqueued.
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
            // Route this run's inference through the gateway executor built from its
            // grant (ADR-0004), overriding the session's bound model for this attempt.
            if let Some(gateway_executor) = gateway_executor {
                context = context.with_model_executor(gateway_executor);
            }
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

    /// Build the per-run model executor for a run's access grant (ADR-0004). A
    /// gateway grant is materialized and dialed through the installed gateway
    /// factory; a local grant yields `None` (use the runtime's bound executor). A
    /// gateway grant with no factory installed, or a dialect the factory cannot
    /// serve, is rejected — fail closed, never degrading to local credentials.
    fn gateway_model_executor(
        &self,
        grant: &awaken_runtime_contract::model_access::ModelAccessGrant,
    ) -> Result<Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>>, HostError> {
        grant
            .resolve_executor(|endpoint| {
                self.gateway_executor_factory
                    .as_ref()
                    .and_then(|factory| factory.build(endpoint))
            })
            .map_err(|unservable| HostError::bad_request(unservable.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_runtime_contract::model_access::ModelAccessGrant;

    use crate::host::SharedHost;
    use crate::test_support::{ServingFactory, StubModel, UnservingFactory, gateway_grant};

    #[test]
    fn a_local_grant_yields_no_gateway_executor() {
        let host = SharedHost::new(Arc::new(StubModel), "t");
        // Default (local) grant → None, whether or not a factory is installed.
        assert!(
            host.gateway_model_executor(&ModelAccessGrant::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_gateway_grant_with_a_factory_builds_an_executor() {
        let host = SharedHost::new(Arc::new(StubModel), "t")
            .with_gateway_executor_factory(Arc::new(ServingFactory));
        assert!(
            host.gateway_model_executor(&gateway_grant())
                .unwrap()
                .is_some(),
            "a gateway grant + a serving factory yields an executor"
        );
    }

    #[test]
    fn a_gateway_grant_fails_closed_with_no_factory_or_an_unservable_dialect() {
        // No factory installed → reject.
        let bare = SharedHost::new(Arc::new(StubModel), "t");
        assert!(
            bare.gateway_model_executor(&gateway_grant()).is_err(),
            "a gateway grant with no factory fails closed"
        );
        // A factory that cannot serve the dialect → reject.
        let host = SharedHost::new(Arc::new(StubModel), "t")
            .with_gateway_executor_factory(Arc::new(UnservingFactory));
        assert!(
            host.gateway_model_executor(&gateway_grant()).is_err(),
            "a gateway grant whose dialect the factory cannot serve fails closed"
        );
    }
}
