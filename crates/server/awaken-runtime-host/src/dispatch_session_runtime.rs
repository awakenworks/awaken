//! Worker-side adapter over the configured Managed Session runtime.
//!
//! Durable dispatch carries only frozen, secret-free projections. This adapter
//! reuses the installed Resource, credential, and MCP authorities when a cold
//! Worker realizes those projections; it owns no parallel store or policy.

use std::sync::{Arc, Weak};

use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::RepositoryBindingVerifier;
use awaken_run_ingress_contract::{
    SessionResourceInstallDecision, session_resource_install_decision,
};
use awaken_session_contract::RunError;

use crate::{ManagedHost, SharedHost};

/// Weak, cloneable Worker-side adapter over the same configured Managed
/// `SessionRuntime`.
#[derive(Clone)]
pub(crate) struct DispatchSessionRuntime {
    pub(super) host: Weak<SharedHost>,
    pub(super) credentials: Option<PinnedCredentialMaterializer>,
    pub(super) credential_refresh_factory: Option<Arc<dyn CredentialRefreshFactory>>,
    pub(super) resource_validator:
        Option<Arc<dyn awaken_resource_contract::LiveResourceBindingVerifier>>,
    pub(super) repository_binding_verifier:
        Option<Arc<dyn RepositoryBindingVerifier<awaken_run_ingress::RunClaim>>>,
    pub(super) mcp_realizer: Option<Arc<dyn awaken_session_contract::McpAttachmentRealizer>>,
}

impl DispatchSessionRuntime {
    fn managed(&self) -> Result<ManagedHost, RunError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime host was dropped"))?;
        Ok(ManagedHost {
            host,
            credentials: self.credentials.clone(),
            credential_refresh_factory: self.credential_refresh_factory.clone(),
            resource_validator: self.resource_validator.clone(),
            repository_binding_verifier: self.repository_binding_verifier.clone(),
            mcp_realizer: None,
        })
    }

    async fn install(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        let previous = managed.host.thread_resource_manifest(thread);
        let decision = match &previous {
            Some(previous) => session_resource_install_decision(
                true,
                previous == manifest,
                previous.workspace_id == manifest.workspace_id,
                previous.revision,
                manifest.revision,
            ),
            None => session_resource_install_decision(false, false, false, 0, manifest.revision),
        };
        match decision {
            SessionResourceInstallDecision::Reject => Err(RunError::bad_request(
                "a claimed Worker cannot replace the active Session Resource generation",
            )),
            // A newer/different generation reuses the canonical live Session
            // transition with claim-fenced remote reads and without mutating the
            // authority-side reference graph.
            SessionResourceInstallDecision::Replace => {
                managed
                    .apply_session_inputs_with_context(
                        thread,
                        &manifest.workspace_id,
                        manifest.revision,
                        &manifest.resources,
                        claim,
                    )
                    .await
            }
            SessionResourceInstallDecision::Stage => {
                // Re-stage even when the manifest is unchanged: immutable File
                // bytes, config-version integrity, and credential revocation are
                // live-deny checks at every claimed operation.
                managed
                    .stage_resource_manifest(
                        thread,
                        &manifest.workspace_id,
                        manifest.revision,
                        &manifest.resources,
                        claim,
                    )
                    .await?;
                Ok(())
            }
        }
    }

    async fn stage_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.stage_mcp_attachment(request).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::stage_mcp_attachment(
                    &self.managed()?,
                    request,
                )
                .await
            }
        }
    }

    async fn publish_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.publish_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::publish_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }

    async fn drain_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        match &self.mcp_realizer {
            Some(realizer) => realizer.drain_mcp_generation(generation).await,
            None => {
                awaken_session_contract::McpAttachmentRealizer::drain_mcp_generation(
                    &self.managed()?,
                    generation,
                )
                .await
            }
        }
    }
}

impl SharedHost {
    pub(crate) async fn install_dispatched_resources(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        let preparer = self
            .dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| {
                RunError::internal("durable resource dispatch has no Session Runtime")
            })?;
        preparer.install(thread, manifest, claim).await
    }

    pub(crate) fn dispatch_session_runtime(&self) -> Result<DispatchSessionRuntime, RunError> {
        self.dispatch_session_runtime
            .read()
            .expect("dispatch Session Runtime lock poisoned")
            .clone()
            .ok_or_else(|| RunError::internal("dispatch Session Runtime is not configured"))
    }

    pub(crate) async fn stage_dispatched_mcp(
        &self,
        request: awaken_session_contract::StageMcpAttachment,
    ) -> Result<awaken_session_contract::McpRealizationReceipt, RunError> {
        self.dispatch_session_runtime()?.stage_mcp(request).await
    }

    pub(crate) async fn publish_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .publish_mcp(generation)
            .await
    }

    pub(crate) async fn drain_dispatched_mcp(
        &self,
        generation: awaken_session_contract::McpGenerationRef,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?.drain_mcp(generation).await
    }
}
