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
use awaken_session_contract::{RunError, SessionRuntime};

use crate::{ManagedHost, SharedHost};

/// Decode the one Session-owned Resource envelope carried by durable dispatch.
/// Both execution realization and post-commit observation cross this boundary;
/// keeping scope validation here prevents either caller from inventing a second
/// wire interpretation.
pub(crate) fn decode_dispatched_resource_manifest(
    dispatch: &awaken_run_ingress::RunDispatch,
) -> Result<Option<awaken_session_contract::SessionResourceManifest>, String> {
    let Some(envelope) = &dispatch.session_resources else {
        return Ok(None);
    };
    let scope = dispatch
        .execution_scope
        .as_ref()
        .map(|scope| scope.0.0.as_str());
    if envelope.workspace_id.trim().is_empty() || scope != Some(envelope.workspace_id.as_str()) {
        return Err(format!(
            "run {} has a resource manifest outside its execution scope",
            dispatch.run_id().0
        ));
    }
    envelope.decode_manifest().map(Some).map_err(|error| {
        format!(
            "run {} has an invalid Session resource manifest: {error}",
            dispatch.run_id().0
        )
    })
}

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
    pub(super) repository_publication_binding_verifier: Option<
        Arc<
            dyn RepositoryBindingVerifier<(
                awaken_session_contract::SessionRepositoryPublicationCommand,
                awaken_session_contract::SessionRealizationLease,
            )>,
        >,
    >,
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
            repository_publication_binding_verifier: self
                .repository_publication_binding_verifier
                .clone(),
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

    /// Compile only the authored Memory binding selected by one frozen Agent.
    /// Unlike `install`, this terminal-observation seam intentionally does not
    /// stage File, Repository, Skill, mount, or Environment projections and does
    /// not publish a resident Session selection.
    async fn compile_memory_binding(
        &self,
        session_thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        binding_id: &str,
    ) -> Result<Arc<crate::memory::BoundMemory>, RunError> {
        let input = manifest
            .resources
            .inputs()
            .iter()
            .find(|input| input.binding_id.as_str() == binding_id)
            .ok_or_else(|| {
                RunError::internal(format!(
                    "frozen Memory binding `{binding_id}` is absent from the Session manifest"
                ))
            })?;
        if !matches!(
            &input.source,
            awaken_session_contract::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(RunError::internal(format!(
                "frozen binding `{binding_id}` is not a MemoryStore"
            )));
        }
        self.managed()?
            .compile_memory_binding(session_thread, &manifest.workspace_id, input, None)
            .await?
            .map(|(_, memory)| memory)
            .ok_or_else(|| RunError::internal("frozen Memory input did not compile"))
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

    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        self.managed()?.execute_terminal_cleanup(command).await
    }

    async fn execute_terminal_repository_publication(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<awaken_session_contract::SessionRepositoryPublicationReceipt, RunError> {
        self.managed()?
            .publish_terminal_repository(command, Some(lease))
            .await
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

    pub(crate) async fn compile_dispatched_memory_binding(
        &self,
        session_thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        binding_id: &str,
    ) -> Result<Arc<crate::memory::BoundMemory>, RunError> {
        self.dispatch_session_runtime()?
            .compile_memory_binding(session_thread, manifest, binding_id)
            .await
    }

    /// Build the terminal observer from the exact guarded dispatch. The logical
    /// Thread owns transcript/extraction identity; `commit` may still point at
    /// its parent Session's physical partition.
    pub(crate) async fn dispatched_memory_terminal_observer(
        &self,
        dispatch: &awaken_run_ingress::RunDispatch,
        commit: Arc<crate::store::HostCommit>,
    ) -> Result<
        Option<Arc<dyn awaken_runtime_contract::terminal::RunTerminalObserver>>,
        crate::HostError,
    > {
        let manifest =
            decode_dispatched_resource_manifest(dispatch).map_err(crate::HostError::internal)?;
        let workspace = dispatch
            .execution_scope
            .as_ref()
            .map_or_else(|| self.local_workspace(), |scope| scope.0.0.as_str());
        let frozen_publications = crate::agent_catalog::exact_run_publication_source(
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

    pub(crate) async fn execute_dispatched_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        self.dispatch_session_runtime()?
            .execute_terminal_cleanup(command)
            .await
    }

    pub(crate) async fn execute_dispatched_terminal_repository_publication(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<awaken_session_contract::SessionRepositoryPublicationReceipt, RunError> {
        self.dispatch_session_runtime()?
            .execute_terminal_repository_publication(command, lease)
            .await
    }
}
