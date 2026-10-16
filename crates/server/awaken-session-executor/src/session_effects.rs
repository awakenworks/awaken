// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The original weak configured Session effects adapter retains the same lifecycle and MCP authorities.
//! Run-side effects reached by claimed execution.
use crate::{ManagedHost, SharedHost};
use awaken_session_types::RunError;
use std::sync::{Arc, Weak};
/// Weak, cloneable Worker-side adapter over the same configured Managed
/// `SessionRuntime`.
#[derive(Clone)]
pub(crate) struct SessionEffects {
    pub(super) host: Weak<SharedHost>,
}

impl SessionEffects {
    pub(crate) fn managed(&self) -> Result<ManagedHost, RunError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime host was dropped"))?;
        Ok(ManagedHost::new(host))
    }
}

impl SharedHost {
    /// Build the terminal observer from the exact guarded dispatch. The logical
    /// Thread owns transcript/extraction identity; `commit` may still point at
    /// its parent Session's physical partition.
    pub(crate) async fn dispatched_memory_terminal_observer(
        &self,
        dispatch: &awaken_dispatch_contract::RunDispatch,
        commit: Arc<crate::store::HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let manifest =
            awaken_dispatch_execution::session_envelope::decode_dispatched_resource_manifest(
                dispatch,
            )
            .map_err(crate::HostError::internal)?;
        let workspace = dispatch
            .execution_scope
            .as_ref()
            .map_or_else(|| self.local_workspace(), |scope| scope.0.0.as_str());
        let frozen_publications =
            awaken_dispatch_execution::run_publications::exact_run_publication_source(
                &dispatch.activation.snapshot,
                &dispatch.agent_publications,
                workspace,
            )
            .map_err(crate::HostError::internal)?;
        self.memory_terminal_observer(
            &dispatch.session_thread_id().0,
            &dispatch.activation.snapshot,
            dispatch.activation.effective_model_ref(),
            manifest.as_ref(),
            frozen_publications.as_ref(),
            commit,
        )
        .await
    }

    pub(crate) fn session_effects(&self) -> SessionEffects {
        self.dispatch.session_effects.clone()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for SessionEffects {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        let managed = self.managed()?;
        match &managed.host.session_execution.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                awaken_session_execution_contract::McpAttachmentRealizer::stage_mcp_attachment(
                    &managed, request,
                )
                .await
            }
        }
    }

    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        match &managed.host.session_execution.mcp_realizer {
            Some(realizer) => realizer.publish_mcp_generation(generation).await,
            None => {
                awaken_session_execution_contract::McpAttachmentRealizer::publish_mcp_generation(
                    &managed, generation,
                )
                .await
            }
        }
    }

    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        match &managed.host.session_execution.mcp_realizer {
            Some(realizer) => realizer.drain_mcp_generation(generation).await,
            None => {
                awaken_session_execution_contract::McpAttachmentRealizer::drain_mcp_generation(
                    &managed, generation,
                )
                .await
            }
        }
    }
}
