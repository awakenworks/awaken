// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Static owner: the Managed adapter's MCP attachment realizer port at the runtime host.
// @formal reason: It delegates staging, publication and drain to the Session MCP lifecycle, supplying the
// @formal reason: Session facts and credential materialization that lifecycle reads. Evidence:
// @formal reason: protocol.mcp.mcp_protocol and managed MCP Session tests; this is a product-requirement
// @formal reason: classification, not a direct proof claim.

//! The Session MCP attachment realizer and the Session facts MCP staging reads,
//! served by [`RunAssembly`].

use std::sync::Arc;

use awaken_runtime_contract::runs::RunError;
use awaken_session_realization::peers::EnvironmentPeer;

use super::RunAssembly;
use crate::PinnedCredentialMaterializer;
use crate::session_execution_error::to_run_error;

impl RunAssembly {
    #[cfg(test)]
    pub(crate) async fn stage_direct_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.mcp
            .stage_attachment(
                &self.installed_sessions,
                Arc::new(self.clone()),
                Self::mcp_credential_materialization,
                request,
            )
            .await
    }

    /// The credential materialization this adapter lends one staging call.
    pub(crate) fn mcp_credential_materialization(
        &self,
    ) -> awaken_session_mcp::McpCredentialMaterialization<'_> {
        awaken_session_mcp::McpCredentialMaterialization {
            credentials: self.capabilities.exec.credential_materializer.as_ref(),
            refresh: self.session_execution.mcp_credential_refresh.as_deref(),
            platform_egress: self.session_execution.platform_mcp_egress.as_deref(),
        }
    }
}

// The Session facts MCP staging reads, answered from this adapter's Host.
#[async_trait::async_trait]
impl awaken_session_mcp::McpStagingSession for RunAssembly {
    fn stdio_tool_catalog_access(
        &self,
        request: &awaken_session_configuration_contract::mcp::StageMcpAttachment,
    ) -> Option<awaken_session_mcp::StdioCatalogAccess> {
        use awaken_session_configuration_contract::mcp::tool_catalog::StdioToolCatalogKey;
        let target = request.target.sandbox_stdio_target()?;
        if !target.share_tool_catalog || request.credential.is_some() || request.prompts_as_skills {
            return None;
        }
        let session = request.generation.session_id();
        let inputs = self
            .installed_sessions
            .realizations()
            .frozen(session.as_str())
            .ok()?;
        let baseline = inputs.baseline.as_ref()?;
        if inputs.workspace.as_deref() != Some(request.workspace_id.as_str())
            || !baseline.env.is_empty()
            || !baseline.mounts.is_empty()
        {
            return None;
        }
        let (key, launch_fingerprint) = StdioToolCatalogKey::for_environment(
            &request.workspace_id,
            target,
            &baseline.environment,
        )?;
        let slots = Arc::downgrade(&self.installed_sessions);
        let session = session.as_str().to_owned();
        let may_share = Arc::new(move || {
            slots.upgrade().is_some_and(|slots| {
                slots
                    .read(&session, |installed| {
                        let resources = installed.realization.staging();
                        let is_initial_empty = |manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest| {
                            // Managed creation prepares generation one even for an empty manifest.
                            manifest.revision <= 1
                                && manifest.resources.inputs().is_empty()
                                && manifest.resources.skills().is_empty()
                        };
                        resources.run_manifest(true).as_ref().is_some_and(is_initial_empty)
                            && resources.run_manifest(false).as_ref().is_none_or(is_initial_empty)
                    })
                    .is_ok_and(|result| result == Some(true))
            })
        });
        may_share().then_some(awaken_session_mcp::StdioCatalogAccess {
            key,
            launch_fingerprint,
            may_share,
        })
    }

    fn deferred_stdio_environment(
        &self,
        session: &str,
    ) -> Result<Option<awaken_session_mcp::DeferredStdioEnvironment>, RunError> {
        let inputs = self.installed_sessions.realizations().frozen(session)?;
        if !inputs
            .environment_projection
            .as_ref()
            .is_some_and(|environment| {
                environment.provisioning
                    == awaken_provisioning_contract::SandboxProvisioning::OnToolUse
            })
        {
            return Ok(None);
        }
        let (_, _, _, model) = self
            .realization_executor
            .environments()
            .publication()
            .resolve_session_publication(session, None, inputs.published_snapshot.clone())
            .map_err(RunError::from)?;
        if model.as_ref().is_some_and(|model| {
            !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(model.binding().identity().2),
                awaken_runtime_contract::resolved::Backend::Native
            )
        }) {
            return Ok(None);
        }
        let executor = Arc::downgrade(&self.realization_executor);
        let session = session.to_owned();
        Ok(Some(Arc::new(move || {
            let executor = executor.clone();
            let session = session.clone();
            Box::pin(async move {
                let executor = executor.upgrade().ok_or_else(|| {
                    RunError::unavailable_classified(
                        "mcp_sandbox_unavailable",
                        "Session executor was released",
                    )
                })?;
                executor
                    .ensure_session_environment_for_tool(&session)
                    .await
                    .map_err(RunError::from)
            })
        })))
    }

    fn mcp_plugin_config(
        &self,
        session: &str,
        plugin_id: &str,
    ) -> Result<Option<serde_json::Value>, RunError> {
        let inputs = self.installed_sessions.realizations().frozen(session)?;
        Ok(match inputs.published_snapshot.as_ref() {
            Some(snapshot) => snapshot.resolved_spec.plugin_config.get(plugin_id),
            None => self.capabilities.tools.plugin_config.get(plugin_id),
        }
        .cloned())
    }

    fn mcp_generation_is_authorized_at(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
        now_unix_ms: u64,
    ) -> Result<bool, RunError> {
        self.mcp_generation_is_authorized_at(generation, now_unix_ms)
    }

    fn relay_authority(&self) -> std::sync::Weak<dyn awaken_session_mcp::McpGenerationAuthority> {
        Arc::downgrade(&self.mcp_root_authority)
            as std::sync::Weak<dyn awaken_session_mcp::McpGenerationAuthority>
    }

    fn execution_backend(
        &self,
        session: &str,
    ) -> Result<Option<awaken_runtime_contract::resolved::Backend>, RunError> {
        let realizations = self.installed_sessions.realizations();
        Ok(realizations
            .frozen(session)?
            .backend_ref
            .clone()
            .map(|backend_ref| awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref)))
    }

    fn supports_secret_egress_without_bypass(&self, session: &str) -> Result<bool, RunError> {
        let provider = self
            .realization_executor
            .environments()
            .projected_session_environment_provider(session, None)
            .map_err(RunError::from)?;
        Ok(provider
            .readiness()
            .capabilities()
            .supports_secret_egress_without_bypass())
    }

    fn frozen_network(
        &self,
        session: &str,
    ) -> Result<Option<awaken_provisioning_contract::NetworkPolicy>, RunError> {
        let realizations = self.installed_sessions.realizations();
        Ok(realizations
            .frozen(session)?
            .environment_projection
            .as_ref()
            .map(|projection| projection.network.clone()))
    }

    fn acp_client_injection_capabilities(
        &self,
        materializer: &PinnedCredentialMaterializer,
        cli: &str,
        http_transport: bool,
    ) -> Result<Option<awaken_credential_contract::CredentialRealizationCapabilities>, RunError>
    {
        Ok(crate::host::acp_mcp_client_injection_capabilities(
            materializer,
            self.capabilities
                .acp_adapter_policy()
                .map_err(to_run_error)?,
            cli,
            http_transport,
        ))
    }

    async fn resident_environment(
        &self,
        session: &str,
    ) -> Result<Option<Arc<crate::session_environment::SessionEnvironment>>, RunError> {
        self.realization_executor
            .environments()
            .session_environment(session)
            .await
            .map_err(RunError::from)
    }

    fn frozen_agent(&self, session: &str) -> Result<Option<String>, RunError> {
        Ok(self
            .installed_sessions
            .read(session, |installed| {
                installed.inputs.agent_id.clone().or_else(|| {
                    installed
                        .inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.agent_id.clone())
                })
            })
            .map_err(|error| {
                RunError::from(crate::realization_error::RealizationError::from(error))
            })?
            .flatten())
    }

    async fn realize_environment(&self, session: &str, agent: &str) -> Result<(), RunError> {
        let context = self
            .ctx_for(session, Some(agent))
            .await
            .map_err(to_run_error)?;
        if context.env.is_some() {
            return Ok(());
        }
        if !self.runs_local_dispatch_pool()
            || !awaken_session_execution_contract::requires_local_environment(
                &context.config.resolved_spec,
            )
        {
            return Err(RunError::classified(
                "mcp_sandbox_unavailable",
                "this Session cannot realize a local Sandbox for stdio MCP",
            ));
        }
        // The ordinary context may defer a Native environment until it is
        // needed. Staging stdio MCP is a physical consumer, like a Hand tool.
        self.realization_executor
            .ensure_session_environment_for_tool(session)
            .await
            .map(|_| ())
            .map_err(RunError::from)
    }
}

// Runtime realization of one Session-owned MCP attachment generation.
#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for RunAssembly {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.mcp
            .stage_assigned_attachment(
                &self.installed_sessions,
                Arc::new(self.clone()),
                Self::mcp_credential_materialization,
                request,
            )
            .await
    }

    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.mcp
            .publish_attachment(&self.installed_sessions, self, generation)
            .await
    }

    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.mcp
            .drain_attachment(self.installed_sessions.as_ref(), generation)
            .await
    }
}
