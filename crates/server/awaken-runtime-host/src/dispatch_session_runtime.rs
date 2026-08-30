//! Worker-side adapter over the configured Managed Session runtime.
//!
//! Durable dispatch carries only frozen, secret-free projections. This adapter
//! reuses the installed Resource, credential, and MCP authorities when a cold
//! Worker realizes those projections; it owns no parallel store or policy.

use std::sync::{Arc, Weak};

use awaken_credential_materializer::{CredentialRefreshFactory, PinnedCredentialMaterializer};
use awaken_resource_contract::RepositoryBindingVerifier;
use awaken_session_contract::RunError;

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
    pub(crate) fn managed(&self) -> Result<ManagedHost, RunError> {
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
        // Resource generation is the innermost Session transition. A complete
        // projection may already hold `lifecycle`, while claimed realization
        // also holds `realization`; this dedicated suffix lock avoids re-entry
        // and keeps compare/realize/publish atomic for every caller.
        let resource_projection = managed
            .host
            .session_slots
            .update(thread, |slot| slot.resource_projection.clone());
        let _resource_projection = resource_projection.lock().await;
        let active = managed.host.thread_resource_manifest(thread);
        if let Some(active) = &active {
            if active != manifest {
                // A desired-only envelope cannot prove the physical previous
                // generation. Every replacement, including an unattempted
                // Coordinator amendment, must carry the exact aggregate
                // `SessionResourceTransition` instead.
                return Err(RunError::unavailable_classified(
                    "session_resource_transition_required",
                    "a desired-only Resource manifest cannot replace the active generation",
                ));
            }
            // Re-stage even when the active manifest is unchanged: immutable
            // File bytes, config-version integrity, and credential revocation
            // are live-deny checks at every claimed operation. Staging never
            // republishes completion.
            return managed
                .stage_resource_manifest(
                    thread,
                    &manifest.workspace_id,
                    manifest.revision,
                    &manifest.resources,
                    claim,
                )
                .await;
        }

        let live_environment = managed.host.session_environment(thread).await;
        let (expected_binding, projected_transition) = managed
            .host
            .session_slots
            .read(thread, |slot| {
                (
                    slot.environment_owner.durable_binding().is_some(),
                    slot.resource_transition.clone(),
                )
            })
            .unwrap_or_default();
        if live_environment.is_some() || expected_binding {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_required",
                "a desired-only Resource manifest cannot identify the bound Environment's active generation",
            ));
        }
        let empty = awaken_session_contract::SessionResourceManifest::at_revision(
            manifest.workspace_id.clone(),
            0,
            awaken_session_contract::ResolvedSessionResources::default(),
        );
        let proven_transition =
            awaken_session_contract::SessionResourceTransition::new(empty, manifest.clone())
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        if projected_transition
            .as_ref()
            .is_some_and(|projected| projected != &proven_transition)
        {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_conflict",
                "the cold Session already carries a different exact Resource transition",
            ));
        }
        managed
            .stage_resource_manifest(
                thread,
                &manifest.workspace_id,
                manifest.revision,
                &manifest.resources,
                claim,
            )
            .await?;
        // No resident or durable binding exists, so Empty→desired is proven
        // rather than inferred from a cache. Persist only the command projection;
        // the active manifest remains absent until physical realization succeeds.
        managed.host.session_slots.update(thread, |slot| {
            if slot.resource_transition.is_none() {
                slot.resource_transition = Some(proven_transition);
            }
        });
        Ok(())
    }

    async fn stage_prevalidated_transition_under_resource_projection(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
        prospective_environment_binding: bool,
    ) -> Result<(), RunError> {
        let managed = self.managed()?;
        managed
            .stage_prevalidated_resource_transition_under_resource_projection(
                thread,
                transition,
                claim,
                prospective_environment_binding,
            )
            .await
    }

    async fn apply_transition(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.managed()?
            .apply_session_inputs_with_context(thread, transition, claim)
            .await
    }

    async fn apply_transition_under_lifecycle(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.managed()?
            .apply_session_inputs_under_lifecycle(thread, transition, claim)
            .await
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
}

impl SharedHost {
    pub(crate) async fn install_dispatched_resources(
        &self,
        thread: &str,
        manifest: &awaken_session_contract::SessionResourceManifest,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .install(thread, manifest, claim)
            .await
    }

    /// Stage cold requirements from one already-preflighted complete projection.
    /// The caller holds `resource_projection` across prospective validation,
    /// this staging, and publication of every immutable Session fact. The exact
    /// aggregate transition is retained as cache identity, while the active
    /// manifest remains owned by physical transition completion.
    pub(crate) async fn stage_prevalidated_dispatched_resource_transition_under_resource_projection(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
        prospective_environment_binding: bool,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .stage_prevalidated_transition_under_resource_projection(
                thread,
                transition,
                claim,
                prospective_environment_binding,
            )
            .await
    }

    pub(crate) async fn apply_dispatched_resource_transition(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .apply_transition(thread, transition, claim)
            .await
    }

    pub(crate) async fn apply_dispatched_resource_transition_under_lifecycle(
        &self,
        thread: &str,
        transition: &awaken_session_contract::SessionResourceTransition,
        claim: Option<&awaken_run_ingress::RunClaim>,
    ) -> Result<(), RunError> {
        self.dispatch_session_runtime()?
            .apply_transition_under_lifecycle(thread, transition, claim)
            .await
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
}
