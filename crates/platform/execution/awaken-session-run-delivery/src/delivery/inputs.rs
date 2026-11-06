// @formal surface requirement=session.lifecycle.session_state
// @formal reason: The Coordinator admits a Worker-placed Session's frozen inputs: the dispatch projection is
// @formal reason: ordered by the aggregate's receipt law, layouts are checked by their structural rules and
// @formal reason: Skill selections pin exact versions. No realization effect is reachable. Classification only.
//! The frozen-input admission of the Sessions registered Workers realize.

use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::sandbox_layout::validate_structural_sandbox_layout;
use awaken_session_configuration_contract::skill_delivery::{
    resolve_skill_selections, skill_store_run_error,
};
use awaken_session_configuration_contract::{
    SessionSandboxLayout, SessionToolConfiguration, environment_projection,
    managed_layout::binding_layout_projection,
};
use awaken_session_execution_contract::{
    CanonicalSessionProjection, FrozenSessionProjection, SessionRealizationEffectFailure,
};
use awaken_session_execution_contract::{NO_COLOCATED_WORKER, SessionInputAdmission};
use awaken_session_types::SessionRuntimePlacement;

use super::WorkerPlacedSessions;

#[async_trait::async_trait]
impl SessionInputAdmission for WorkerPlacedSessions {
    async fn install_dispatch_projection(
        &self,
        thread: &str,
        projection: FrozenSessionProjection,
    ) -> Result<(), SessionRealizationEffectFailure> {
        let manifest =
            awaken_session_configuration_contract::resource::SessionResourceManifest::at_revision(
                projection.workspace_id.clone(),
                projection.resource_revision,
                projection.resources.clone(),
            );
        self.ports.projections.install(thread, projection)?;
        // Recover every writable frozen binding even when the current Agent
        // disabled extraction: older durable intents retain their own snapshot.
        super::memory::recover_bindings(&self.ports, thread, &manifest).await?;
        Ok(())
    }

    /// Decision table (causes → effect):
    /// | Rule | layout | effect |
    /// |---|---|---|
    /// | V1 | its Agent publication or model does not resolve canonically | refused |
    /// | V2 | a local placement | refused: only a co-located Worker validates it |
    /// | V3 | a Worker placement breaking a structural rule | refused with that rule |
    /// | V4 | a structurally valid Worker placement | admitted |
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &SessionSandboxLayout,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::resolve_canonical_session_projection(
            self.ports.projections.publications(),
            &layout.workspace_id,
            CanonicalSessionProjection::Layout(layout),
            None,
            None,
        )
        .map_err(|error| RunError::bad_request(error.to_string()))?;
        if layout.runtime_placement != SessionRuntimePlacement::Worker {
            return Err(RunError::bad_request(NO_COLOCATED_WORKER));
        }
        let environment = environment_projection::project_environment(&layout.environment);
        validate_structural_sandbox_layout(
            thread,
            layout.resources.iter().map(binding_layout_projection),
            &layout.mounts,
            &layout.env,
            Some(&environment),
        )
        .map_err(|error| RunError::bad_request(error.to_string()))
    }

    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<awaken_session_configuration_contract::resource::ResolvedSkillBinding>, RunError>
    {
        if skills.is_empty() {
            return Ok(Vec::new());
        }
        resolve_skill_selections(self.ports.skill_catalog.as_deref(), workspace_id, skills)
            .await
            .map_err(skill_store_run_error)
    }

    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: SessionToolConfiguration,
    ) -> Result<(), RunError> {
        self.ports.projections.replace_tools(thread, tools)
    }
}
