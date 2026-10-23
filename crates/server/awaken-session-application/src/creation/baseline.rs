// @formal entrypoint model_route.session_override_reuses_only_exact_identity_and_resolves_every_mismatch
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
//! What a new Session freezes into its baseline: the exact Environment
//! snapshot, the model override publication and its list-price roster, the
//! Skill bindings and the provider-effective Sandbox layout gate.

use awaken_agent_contract::McpTarget;
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_session_configuration_contract::resource::ResolvedSkillBinding;
use awaken_session_configuration_contract::{
    is_native_session_runtime, validate_sandbox_provisioning_runtime,
};
use awaken_session_types::{RunError, SessionRuntimePlacement};

use crate::{ResolvedSessionEnvironment, SessionCreationCommands, runtime_placement_for_topology};

impl SessionCreationCommands {
    /// Exact immutable fact written into a newly compiled Session baseline.
    #[must_use]
    pub fn runtime_placement(&self) -> SessionRuntimePlacement {
        runtime_placement_for_topology(self.execution_placement)
    }

    /// Invoke the Runtime's pure provider-effective Sandbox-layout gate. This
    /// method performs no durable mutation or external realization and is the
    /// one application seam used before create, live Resource, and final root
    /// persistence boundaries.
    pub fn validate_session_sandbox_layout(
        &self,
        session_id: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        crate::validate_session_sandbox_layout(
            self.realization_effects.as_ref(),
            session_id,
            layout,
        )
    }

    pub async fn resolve_managed_list_price_snapshot(
        &self,
        request: awaken_session_budget_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_budget_contract::ManagedListPriceSnapshot,
        awaken_session_budget_contract::ManagedListPriceError,
    > {
        let provider = self.managed_list_prices.as_deref().ok_or_else(|| {
            awaken_session_budget_contract::ManagedListPriceError::Unavailable(
                "no Managed list-price provider is configured".into(),
            )
        })?;
        let snapshot = provider.resolve_snapshot(request.clone()).await?;
        snapshot.validate(&request.model_refs)?;
        Ok(snapshot)
    }

    /// Resolve and validate the exact Environment snapshot frozen into a new Session.
    pub async fn resolve_session_environment(
        &self,
        requested_environment_id: Option<&str>,
        published_environment: Option<
            &awaken_executable_agent_contract::ExecutableAgentEnvironment,
        >,
        published_backend_ref: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<ResolvedSessionEnvironment, RunError> {
        let environment_id = requested_environment_id
            .map(str::to_owned)
            .or_else(|| published_environment.map(|binding| binding.environment_id.clone()))
            .unwrap_or_else(|| {
                awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID.to_string()
            });
        let exact_publication = published_environment.filter(|binding| {
            requested_environment_id.is_none_or(|requested| requested == binding.environment_id)
        });
        let required_revision = exact_publication.map(|binding| binding.revision);
        let resolved = if let Some(binding) = exact_publication {
            self.environment_projection
                .resolve_exact_for_session(
                    &binding.environment_id,
                    binding.revision,
                    published_backend_ref,
                    mcp_targets,
                )
                .await
        } else {
            self.environment_projection
                .resolve_current_for_session(&environment_id, published_backend_ref, mcp_targets)
                .await
        }
        .map_err(|error| RunError::unavailable(error.to_string()))?
        .ok_or_else(|| {
            RunError::bad_request(format!("environment `{environment_id}` is unavailable"))
        })?;
        if !awaken_session_configuration_contract::resolved_environment_snapshot_is_exact(
            resolved.snapshot.environment_id == environment_id,
            resolved.snapshot.revision.0,
            required_revision,
        ) {
            return Err(RunError::bad_request(format!(
                "environment `{environment_id}` resolver returned another identity or revision"
            )));
        }
        validate_sandbox_provisioning_runtime(
            resolved.snapshot.sandbox_provisioning,
            published_backend_ref,
        )?;
        Ok(resolved)
    }

    /// Resolve a Session override only when it differs from the immutable Agent
    /// publication. This is the single admission rule shared by every protocol
    /// and internal Session creator.
    pub async fn resolve_session_model_override(
        &self,
        workspace_id: &str,
        requested_model: &str,
        published_model: &str,
        inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
    ) -> Result<awaken_session_configuration_contract::SessionModelOverride, RunError> {
        let publication = match awaken_session_configuration_contract::session_model_override_decision(
            requested_model,
            published_model,
        ) {
            awaken_session_configuration_contract::SessionModelOverrideDecision::ReusePublished => None,
            awaken_session_configuration_contract::SessionModelOverrideDecision::ResolveComplete => {
                let resolver = self.model_publication_resolver.as_deref().ok_or_else(|| {
                    RunError::unavailable("Session model override resolution is not configured")
                })?;
                let publication = resolver
                    .resolve_session_model(workspace_id, requested_model)
                    .await
                    .map_err(|error| match error {
                        awaken_session_configuration_contract::SessionModelResolutionError::Invalid(_) => {
                            RunError::bad_request(error.to_string())
                        }
                        awaken_session_configuration_contract::SessionModelResolutionError::Unavailable(_) => {
                            RunError::unavailable(error.to_string())
                        }
                    })?;
                publication
                    .validate_for_workspace(workspace_id)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                Some(Box::new(publication))
            }
        };
        Ok(
            awaken_session_configuration_contract::SessionModelOverride {
                publication,
                inference,
            },
        )
    }

    /// Compile the complete published model roster for a budgeted Session and
    /// reject execution backends that cannot cross the per-request admission
    /// boundary. The caller supplies the already-frozen root profile and model
    /// publication; ordinary roster members are reopened only by the exact
    /// revisions carried on that profile, and the Advisor's complete candidate
    /// comes from that same executable snapshot. Primary and fallback candidates
    /// cross this one walk. It owns both backend compatibility and the complete
    /// price-snapshot model request.
    pub fn managed_session_model_refs(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_profile: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
        root_execution_model_ref: &str,
        root_backend_ref: Option<&str>,
        root_model_publication: Option<
            &awaken_session_configuration_contract::SessionModelPublication,
        >,
    ) -> Result<Vec<String>, RunError> {
        fn add_candidate(
            models: &mut std::collections::BTreeSet<String>,
            agent_id: &str,
            candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        ) -> Result<(), RunError> {
            let backend_ref = &candidate.binding().identity().2;
            if !is_native_session_runtime(Some(backend_ref)) {
                return Err(RunError::bad_request(format!(
                    "budget_backend_unsupported: Agent `{agent_id}` uses external backend `{backend_ref}`"
                )));
            }
            if candidate.binding().identity().1.trim().is_empty() {
                return Err(RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{agent_id}` has no model"
                )));
            }
            models.insert(candidate.binding().identity().1.to_owned());
            Ok(())
        }

        let mut models = std::collections::BTreeSet::new();
        let root_snapshot = root_profile
            .filter(|profile| profile.source_revision > 0)
            .map(|profile| {
                self.queries
                    .executable_snapshot_at_revision(
                        workspace_id,
                        root_agent_id,
                        profile.source_revision,
                    )
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "budget_price_roster_unavailable: Agent `{root_agent_id}` revision {} has no executable publication",
                            profile.source_revision
                        ))
                    })
            })
            .transpose()?;
        if let Some(snapshot) = &root_snapshot
            && snapshot.root_agent_id.0 != root_agent_id
        {
            return Err(RunError::internal(format!(
                "Agent `{root_agent_id}` executable identity does not match its profile"
            )));
        }
        if let Some(publication) = root_model_publication {
            for candidate in
                std::iter::once(&publication.primary).chain(publication.candidates.iter())
            {
                add_candidate(&mut models, root_agent_id, candidate)?;
            }
        } else if let Some(snapshot) = &root_snapshot {
            for candidate in std::iter::once(&snapshot.resolved_spec.model_binding)
                .chain(snapshot.resolved_spec.model_candidates.iter())
            {
                add_candidate(&mut models, root_agent_id, candidate)?;
            }
        } else {
            if !is_native_session_runtime(root_backend_ref) {
                let backend_ref = root_backend_ref.unwrap_or_default();
                return Err(RunError::bad_request(format!(
                    "budget_backend_unsupported: Agent `{root_agent_id}` uses external backend `{backend_ref}`"
                )));
            }
            if root_execution_model_ref.trim().is_empty() {
                return Err(RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{root_agent_id}` has no model"
                )));
            }
            models.insert(root_execution_model_ref.to_owned());
        }
        if let Some(advisor) = root_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.resolved_spec.plugin_config.agent.advisor.as_ref())
        {
            add_candidate(&mut models, root_agent_id, &advisor.candidate)?;
        } else if root_profile.is_some_and(|profile| profile.advisor_model.is_some()) {
            return Err(RunError::bad_request(format!(
                "budget_price_roster_unavailable: Agent `{root_agent_id}` Advisor has no exact executable publication"
            )));
        }

        let Some(root_profile) = root_profile else {
            return Ok(models.into_iter().collect());
        };
        if root_profile.delegates.is_empty() {
            return Ok(models.into_iter().collect());
        }
        if !self.queries.has_agent_profile_source() {
            return Err(RunError::bad_request(
                "budget_price_roster_unavailable: Agent publication source is unavailable",
            ));
        }
        for delegate in &root_profile.delegates {
            let agent_id = &delegate.agent_id;
            let revision = delegate.source_revision.ok_or_else(|| {
                RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{agent_id}` has no exact publication revision"
                ))
            })?;
            let snapshot = self
                .queries
                .executable_snapshot_at_revision(workspace_id, agent_id, revision)
                .ok_or_else(|| {
                    RunError::bad_request(format!(
                        "budget_price_roster_unavailable: Agent `{agent_id}` revision {revision} has no executable publication"
                    ))
                })?;
            if snapshot.root_agent_id.0 != *agent_id {
                return Err(RunError::internal(format!(
                    "Agent `{agent_id}` executable identity does not match its profile"
                )));
            }
            for candidate in std::iter::once(&snapshot.resolved_spec.model_binding)
                .chain(snapshot.resolved_spec.model_candidates.iter())
            {
                add_candidate(&mut models, agent_id, candidate)?;
            }
        }
        Ok(models.into_iter().collect())
    }

    /// Validate that one published Agent can start new Sessions in this scope.
    pub fn validate_profiled_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), RunError> {
        if self
            .queries
            .selected_session_profile(workspace_id, agent_id, None)
            .is_none()
            || self.queries.agent_unavailable(workspace_id, agent_id)
        {
            return Err(RunError::bad_request(format!(
                "agent `{agent_id}` is unavailable"
            )));
        }
        Ok(())
    }

    pub fn validate_session_skill_total(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_view: Option<&ExecutableAgentSessionProfile>,
        root_skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<(), RunError> {
        crate::runtime_commands::validate_session_skill_total(
            &self.queries,
            workspace_id,
            root_agent_id,
            root_view,
            root_skills,
            &[],
        )
    }

    pub async fn runtime_owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        let thread_reads = self.thread_reads.as_ref();

        thread_reads.owns_thread(thread).await
    }

    pub async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<ResolvedSkillBinding>, RunError> {
        let environment_runtime = self.environment.as_ref();

        environment_runtime
            .resolve_session_skills(workspace_id, skills)
            .await
    }

    #[must_use]
    pub fn model(&self) -> String {
        let execution = self.core.as_ref();

        execution.model()
    }
}
