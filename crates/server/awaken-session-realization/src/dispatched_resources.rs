// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Original claimed Resource installation uses the same projection lock and staging authority.
//! Desired-only Resource installation through the canonical staging owner.
use awaken_session_types::RunError;
use std::sync::Arc;
impl crate::resource_staging::ResourceStaging {
    pub async fn install_dispatched_resources(
        &self,
        executor: &crate::realization_executor::RealizationExecutor,
        thread: &str,
        manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
        claim: Option<&awaken_dispatch_contract::RunClaim>,
    ) -> Result<(), RunError> {
        let realization_view = self.sessions.clone();
        // Installing a claim's dispatched Resources is the install path.
        realization_view.install_session(thread)?;
        // Resource generation is the innermost Session transition. A complete
        // projection may already hold `lifecycle`, while claimed realization
        // also holds `realization`; this dedicated suffix lock avoids re-entry
        // and keeps compare/realize/publish atomic for every caller.
        let resource_projection =
            realization_view.update_realization(thread, |r| r.staging.projection.clone())?;
        let _resource_projection = resource_projection.lock().await;
        let staging = self;
        let active = staging.thread_resource_manifest(thread)?;
        let live_environment = executor.session_environment(thread).await?;
        let (expected_binding, projected_transition) = realization_view
            .read_realization(thread, |r| {
                (
                    r.environment.owner.durable_binding().is_some(),
                    r.staging.transition.clone(),
                )
            })?
            .unwrap_or_default();
        if let Some(projected_transition) = projected_transition {
            if projected_transition.desired() != manifest {
                return Err(RunError::unavailable_classified(
                    "session_resource_transition_conflict",
                    "the cold Session already carries a different exact Resource transition",
                ));
            }
            // The complete Session projection already owns the exact physical
            // previous generation. Reuse that command for requirement staging
            // so desired-only dispatch cannot replace it with an inferred
            // Empty→desired transition or a no-op effect identity.
            return self
                .stage_prevalidated_resource_transition_under_resource_projection(
                    executor,
                    thread,
                    &projected_transition,
                    claim,
                    None,
                    expected_binding,
                )
                .await;
        }
        if let Some(active) = &active {
            if active != manifest {
                // Without an aggregate transition, a desired-only envelope
                // cannot prove the physical previous generation. Every
                // replacement must carry that exact command instead.
                return Err(RunError::unavailable_classified(
                    "session_resource_transition_required",
                    "a desired-only Resource manifest cannot replace the active generation",
                ));
            }
            // Re-stage even when the active manifest is unchanged: immutable
            // File bytes, config-version integrity, and credential revocation
            // are live-deny checks at every claimed operation. Staging never
            // republishes completion.
            return self
                .stage_resource_manifest(
                    executor,
                    thread,
                    &manifest.workspace_id,
                    manifest.revision,
                    &manifest.resources,
                    claim,
                )
                .await;
        }
        if live_environment.is_some() || expected_binding {
            return Err(RunError::unavailable_classified(
                "session_resource_transition_required",
                "a desired-only Resource manifest cannot identify the bound Environment's active generation",
            ));
        }
        let empty =
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                manifest.workspace_id.clone(),
                0,
                awaken_session_configuration_contract::resource::ResolvedSessionResources::default(
                ),
            );
        let proven_transition =
            awaken_session_configuration_contract::resource::SessionResourceTransition::new(
                empty,
                manifest.clone(),
            )
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        self.stage_prevalidated_resource_transition_under_resource_projection(
            executor,
            thread,
            &proven_transition,
            claim,
            None,
            false,
        )
        .await?;
        // No resident or durable binding exists, so Empty→desired is proven
        // rather than inferred from a cache. Persist only the command projection;
        // the active manifest remains absent until physical realization succeeds.
        realization_view.update_realization(thread, |r| {
            if r.staging.transition.is_none() {
                r.staging.transition = Some(proven_transition);
            }
        })?;
        Ok(())
    }
}

impl crate::resource_staging::ResourceStaging {
    /// Compile only the authored Memory binding selected by one frozen Agent.
    /// Unlike `install`, this terminal-observation seam intentionally does not
    /// stage File, Repository, Skill, mount, or Environment projections and does
    /// not publish a resident Session selection.
    pub async fn compile_frozen_memory_binding(
        &self,
        session_thread: &str,
        manifest: &awaken_session_configuration_contract::resource::SessionResourceManifest,
        binding_id: &str,
    ) -> Result<Arc<crate::memory_binding::MemoryBinding>, RunError> {
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
            awaken_session_configuration_contract::resource::ResolvedInputSource::MemoryStore { .. }
        ) {
            return Err(RunError::internal(format!(
                "frozen binding `{binding_id}` is not a MemoryStore"
            )));
        }
        self.compile_memory_binding(session_thread, &manifest.workspace_id, input, None)
            .await?
            .map(|(_, memory)| memory)
            .ok_or_else(|| RunError::internal("frozen Memory input did not compile"))
    }
}
