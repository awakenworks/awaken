// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Static owner: the Managed adapter's MCP attachment realizer port at the runtime host.
// @formal reason: It delegates staging, publication and drain to the Session MCP lifecycle, supplying the
// @formal reason: Session facts and credential materialization that lifecycle reads. Evidence:
// @formal reason: protocol.mcp.mcp_protocol and managed MCP Session tests; this is a product-requirement
// @formal reason: classification, not a direct proof claim.

impl ManagedHost {
    #[cfg(test)]
    pub(crate) async fn stage_direct_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.host
            .mcp
            .stage_attachment(
                &self.host.installed_sessions,
                self,
                self.mcp_credential_materialization(),
                request,
            )
            .await
    }

    /// The credential materialization this adapter lends one staging call.
    pub(crate) fn mcp_credential_materialization(
        &self,
    ) -> awaken_session_mcp::McpCredentialMaterialization<'_> {
        awaken_session_mcp::McpCredentialMaterialization {
            credentials: self.host.exec.credential_materializer.as_ref(),
            refresh_factory: self
                .host
                .session_execution
                .credential_refresh_factory
                .as_deref(),
            platform_egress: self.host.session_execution.platform_mcp_egress.as_deref(),
        }
    }
}

// The Session facts MCP staging reads, answered from this adapter's Host.
#[async_trait::async_trait]
impl awaken_session_mcp::McpStagingSession for ManagedHost {
    fn mcp_generation_is_authorized_at(
        &self,
        generation: &crate::session_mcp::McpGenerationRef,
        now_unix_ms: u64,
    ) -> bool {
        self.host
            .mcp_generation_is_authorized_at(generation, now_unix_ms)
    }

    fn relay_authority(&self) -> std::sync::Weak<dyn awaken_session_mcp::McpGenerationAuthority> {
        Arc::downgrade(&self.host)
            as std::sync::Weak<dyn awaken_session_mcp::McpGenerationAuthority>
    }

    fn execution_backend(
        &self,
        session: &str,
    ) -> Option<awaken_runtime_contract::resolved::Backend> {
        let realizations = self.host.installed_sessions.realizations();
        realizations
            .frozen(session)
            .expect("session runtime slots mutex poisoned")
            .backend_ref
            .clone()
            .map(|backend_ref| awaken_runtime_contract::resolved::Backend::from_ref(&backend_ref))
    }

    fn supports_secret_egress_without_bypass(&self, session: &str) -> Result<bool, RunError> {
        let provider = self
            .host
            .realization_executor
            .environments()
            .projected_session_environment_provider(session, None)
            .map_err(RunError::from)?;
        Ok(provider
            .realizer()
            .capabilities()
            .supports_secret_egress_without_bypass())
    }

    fn frozen_network(&self, session: &str) -> Option<awaken_provisioning_contract::NetworkPolicy> {
        let realizations = self.host.installed_sessions.realizations();
        realizations
            .frozen(session)
            .expect("session runtime slots mutex poisoned")
            .environment_projection
            .as_ref()
            .map(|projection| projection.network.clone())
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
            self.host.acp_adapter_policy().map_err(to_run_error)?,
            cli,
            http_transport,
        ))
    }

    async fn resident_environment(
        &self,
        session: &str,
    ) -> Option<Arc<crate::session_environment::SessionEnvironment>> {
        self.host
            .realization_executor
            .environments()
            .session_environment(session)
            .await
            .expect("session runtime slots mutex poisoned")
    }

    fn frozen_agent(&self, session: &str) -> Option<String> {
        self.host
            .installed_sessions
            .read(session, |slot| {
                slot.inputs.agent_id.clone().or_else(|| {
                    slot.inputs
                        .baseline
                        .as_ref()
                        .map(|baseline| baseline.agent_id.clone())
                })
            })
            .flatten()
    }

    async fn realize_environment(&self, session: &str, agent: &str) -> Result<(), String> {
        self.host
            .ctx_for(session, Some(agent))
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

// Runtime realization of one Session-owned MCP attachment generation.
#[async_trait::async_trait]
impl awaken_session_execution_contract::McpAttachmentRealizer for ManagedHost {
    async fn stage_mcp_attachment(
        &self,
        request: crate::session_mcp::StageMcpAttachment,
    ) -> Result<crate::session_mcp::McpRealizationReceipt, RunError> {
        self.host
            .mcp
            .stage_assigned_attachment(
                &self.host.installed_sessions,
                self,
                self.mcp_credential_materialization(),
                request,
            )
            .await
    }

    async fn publish_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.host
            .mcp
            .publish_attachment(self.host.installed_sessions.as_ref(), self, generation)
            .await
    }

    async fn drain_mcp_generation(
        &self,
        generation: crate::session_mcp::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.host
            .mcp
            .drain_attachment(self.host.installed_sessions.as_ref(), generation)
            .await
    }
}
