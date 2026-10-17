// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: The Runtime Host side of Session MCP: its slot registry and root realization authority as
// @formal reason: the MCP lifecycle reads them, the Host projection of MCP failures, and thin delegates. The
// @formal reason: lifecycle itself is classified at its own module; this is a product-requirement
// @formal reason: classification, not a direct proof claim.
//! The Runtime Host side of Session MCP: the slot registry and root
//! authority the MCP lifecycle reads, the Host error projection of its
//! failures, and the `SharedHost` methods other Host modules call, each a thin
//! delegate to [`awaken_session_mcp::McpSessions`].

use std::sync::{Arc, Weak};

use crate::installed_session::InstalledSessions;
use crate::{HostError, SharedHost};
use awaken_session_mcp::{
    McpError, McpGenerationAuthority, McpInputHost, McpInputRecipients, McpQuiescenceProof,
    McpSessionSlots,
};
use awaken_session_mcp::{McpGenerationProjection, SessionMcpState};

impl McpSessionSlots for InstalledSessions {
    type Runtime = Arc<crate::host::SessionCtx>;

    fn read_mcp<R>(&self, session: &str, read: impl FnOnce(&SessionMcpState) -> R) -> Option<R> {
        self.read(session, |slot| read(&slot.realization.staging.mcp))
    }

    fn update_mcp<R>(&self, session: &str, update: impl FnOnce(&mut SessionMcpState) -> R) -> R {
        self.update(session, |slot| update(&mut slot.realization.staging.mcp))
    }

    fn modify_mcp<R>(
        &self,
        session: &str,
        modify: impl FnOnce(&mut SessionMcpState) -> R,
    ) -> Option<R> {
        self.modify(session, |slot| modify(&mut slot.realization.staging.mcp))
    }

    fn modify_mcp_and_runtime<R>(
        &self,
        session: &str,
        modify: impl FnOnce(&mut SessionMcpState, &mut Option<Self::Runtime>) -> R,
    ) -> Option<R> {
        self.modify(session, |slot| {
            modify(&mut slot.realization.staging.mcp, &mut slot.run.context)
        })
    }

    fn modify_mcp_with_root<R>(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
        modify: impl FnOnce(&mut SessionMcpState, Option<bool>) -> R,
    ) -> Option<R> {
        self.modify(generation.session_id().as_str(), |slot| {
            let root = slot.realization.mcp_generation_root_authority(generation);
            modify(&mut slot.realization.staging.mcp, root)
        })
    }

    /// An unreadable foreground record signals nothing; `runtime_has_active_run`
    /// then reports the Run active, so the drain refuses with its timeout
    /// instead of manufacturing quiescence.
    fn cancel_runtime(runtime: &Self::Runtime) {
        if let Ok(crate::host::ForegroundRun {
            cancellation: Some(token),
            ..
        }) = runtime.foreground.observe()
        {
            token.cancel();
        }
    }

    fn runtime_has_active_run(runtime: &Self::Runtime) -> bool {
        runtime.has_active_run()
    }
}

impl McpGenerationAuthority for SharedHost {
    fn mcp_generation_root_authority(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
    ) -> Option<bool> {
        SharedHost::mcp_generation_root_authority(self, generation)
    }
}

#[async_trait::async_trait]
impl McpInputHost for SharedHost {
    fn active_mcp_projections(&self, session: &str) -> Vec<McpGenerationProjection> {
        SharedHost::active_mcp_projections(self, session)
    }

    async fn cancel_durable_run(&self, session: &str, run_id: &str) -> bool {
        self.cancel_durable(session, run_id).await.is_ok()
    }
}

impl SharedHost {
    pub(crate) fn active_mcp_projections(&self, thread: &str) -> Vec<McpGenerationProjection> {
        self.mcp
            .active_projections(self.installed_sessions.as_ref(), thread)
    }

    #[cfg(test)]
    pub(crate) fn mcp_projection(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Option<McpGenerationProjection> {
        self.mcp
            .projection(self.installed_sessions.as_ref(), generation)
    }

    #[cfg(test)]
    pub(crate) fn insert_mcp_projection(
        &self,
        projection: McpGenerationProjection,
    ) -> Result<(), HostError> {
        Ok(self
            .mcp
            .insert_projection(self.installed_sessions.as_ref(), projection)?)
    }

    /// Transfer a freshly spawned stdio process into the already-installed
    /// exact-generation owner before any further fallible await.
    #[cfg(test)]
    pub(crate) fn attach_staging_mcp_process(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        process: Arc<dyn awaken_provisioning_contract::ProcessHandle>,
    ) -> Result<bool, HostError> {
        Ok(self.mcp.attach_staging_process(
            self.installed_sessions.as_ref(),
            generation,
            process,
        )?)
    }

    /// Publish only wiring into the private staged projection. Durable publish
    /// remains the separate `Staged -> Active` transition.
    #[cfg(test)]
    pub(crate) fn complete_staging_mcp_projection(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
        wiring: awaken_session_mcp::McpWiring,
    ) -> Result<(), HostError> {
        Ok(self
            .mcp
            .complete_staging(self.installed_sessions.as_ref(), generation, wiring)?)
    }

    #[cfg(test)]
    pub(crate) fn renew_mcp_projection(
        &self,
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Result<Option<awaken_session_configuration_contract::mcp::McpRealizationReceipt>, HostError>
    {
        Ok(self
            .mcp
            .renew_projection(self.installed_sessions.as_ref(), request)?)
    }

    #[cfg(test)]
    pub(crate) fn forget_replayable_removed_mcp_projection(
        &self,
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> bool {
        self.mcp
            .forget_replayable_removed_projection(self.installed_sessions.as_ref(), request)
    }

    #[cfg(test)]
    pub(crate) async fn publish_mcp_projection(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), HostError> {
        Ok(self
            .mcp
            .publish_projection(self.installed_sessions.as_ref(), generation)
            .await?)
    }

    #[cfg(test)]
    pub(crate) async fn drain_mcp_projection(
        &self,
        generation: &awaken_session_configuration_contract::mcp::McpGenerationRef,
    ) -> Result<(), HostError> {
        Ok(self
            .mcp
            .drain_projection(self.installed_sessions.as_ref(), generation)
            .await?)
    }

    pub(crate) async fn drain_mcp_projections(
        &self,
        thread: &str,
        expected: &[crate::session_mcp::McpGenerationRef],
    ) -> Result<McpQuiescenceProof, HostError> {
        Ok(self
            .mcp
            .drain_projections(self.installed_sessions.as_ref(), thread, expected)
            .await?)
    }

    pub(crate) fn validate_mcp_input_placement(
        &self,
        configuration: &std::collections::BTreeMap<String, serde_json::Value>,
        opaque_backend: bool,
    ) -> Result<(), HostError> {
        Ok(self.mcp.validate_input_placement(
            configuration,
            opaque_backend,
            !self.authority.hosts_session_application(),
        )?)
    }

    pub(crate) fn install_mcp_input_handlers(
        &self,
        projections: &[McpGenerationProjection],
        configuration: &std::collections::BTreeMap<String, serde_json::Value>,
    ) -> Result<(), HostError> {
        let recipients = || {
            let application = self
                .session_application
                .background_run_submission()
                .cloned()
                .filter(|p| p.strong_count() > 0)
                .ok_or_else(|| {
                    McpError::internal("MCP application input has no Session admission authority")
                })?;
            let host: Weak<dyn McpInputHost> = self.session_effects().host;
            Ok(McpInputRecipients { application, host })
        };
        Ok(self
            .mcp
            .install_input_handlers(projections, configuration, &recipients)?)
    }

    /// Bind the configured external relay before a Worker can publish Ready.
    /// The endpoint is still only a transport prerequisite: provider-owned
    /// substitution and no-bypass evidence remain independent admission facts.
    pub async fn prepare_container_mcp_relay(self: &Arc<Self>) -> Result<(), std::io::Error> {
        let authority = Arc::downgrade(self) as Weak<dyn McpGenerationAuthority>;
        self.mcp.prepare_container_relay(authority).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoLlm;

    #[async_trait::async_trait]
    impl awaken_runtime_contract::llm::LlmExecutor for NoLlm {
        async fn infer(
            &self,
            _: awaken_runtime_contract::llm::ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<awaken_runtime_contract::llm::ChatResponse>
        {
            unreachable!("MCP reaper admission never runs inference")
        }
    }

    #[tokio::test]
    async fn sandbox_stdio_requires_reaper_before_environment_or_staging_effects() {
        // Causes: C1 Native sandbox-stdio MCP target; C2 no installed process
        // reaper. Effects: E1 stable unavailable reason; E2 no Environment
        // realization, Staging projection, or process spawn. Constraint: an
        // anonymous target needs no credential materialization. Decision row:
        // S0 C1+C2 -> E1+E2, before any Session Environment effect.
        let host = (crate::HostDependencies::ephemeral(Arc::new(NoLlm), "test").await)
            .build()
            .expect("validated Host dependencies");
        let generation =
            crate::test_mcp::mcp_fence("mcp-no-reaper", "stdio", 1, "runtime-1", 1, u64::MAX);
        let request = awaken_session_configuration_contract::mcp::StageMcpAttachment {
            workspace_id: "workspace-a".into(),
            generation: generation.clone(),
            realization_id: "realize-1".into(),
            stage_idempotency_key: "stage-1".into(),
            name: "stdio".into(),
            target: awaken_agent_contract::McpTarget::sandbox_stdio("/usr/bin/true", vec![])
                .unwrap(),
            prompts_as_skills: false,
            credential: None,
            selected_plaintext_holder: None,
        };
        let error = crate::ManagedHost::new(host.clone())
            .stage_direct_mcp_attachment(request)
            .await
            .expect_err("S0 missing process owner");
        assert_eq!(error.code, "mcp_generation_process_reaper_missing", "S0/E1");
        assert!(host.mcp_projection(&generation).is_none(), "S0/no Staging");
        assert!(
            host.realization_executor
                .environments()
                .session_environment("mcp-no-reaper")
                .await
                .expect("installed Sessions")
                .is_none(),
            "S0/no Environment"
        );
    }
}

#[cfg(test)]
#[path = "mcp_host/true_retirement.rs"]
mod true_retirement;

#[cfg(test)]
mod admission_tests;
