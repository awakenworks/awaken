// @formal surface requirement=model.routing.exact_binding
// @formal reason: Frozen Session coordinates select and retain the exact Agent/model publication through
// @formal reason: the same installed-slot lock; context, cold replay and provider selection share this owner.
//! Immutable Agent/model publication selection for installed Sessions.
//!
//! The component sees only the publication source selected at construction,
//! the process Workspace fallback and the canonical realization slots. It
//! neither owns inference executors nor reaches back into the Host.

use std::sync::Arc;

use crate::host_error::HostError;
use crate::session_realizations::SessionRealizations;
use awaken_runtime_contract::{ExecutableAgentSnapshot, resolved::ResolvedModelCandidate};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::CanonicalSessionProjection;
use awaken_session_realization_contract::FrozenAgentPublicationDecision as PublicationDecision;

pub(crate) struct SessionPublicationSelection {
    sessions: SessionRealizations,
    local_workspace: String,
    publications: Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
}

impl SessionPublicationSelection {
    pub(crate) fn new(
        sessions: SessionRealizations,
        local_workspace: String,
        publications: Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
    ) -> Self {
        Self {
            sessions,
            local_workspace,
            publications,
        }
    }

    /// Workspace carried by the current frozen Session, with the process's
    /// selected fallback for genuinely unregistered flat-protocol Sessions.
    pub(crate) fn workspace(&self, session: &str) -> String {
        self.workspace_for(&self.sessions.frozen(session))
    }

    pub(crate) fn local_workspace(&self) -> &str {
        &self.local_workspace
    }

    fn workspace_for(&self, inputs: &crate::frozen_inputs::FrozenSessionInputs) -> String {
        inputs
            .workspace
            .clone()
            .unwrap_or_else(|| self.local_workspace.clone())
    }

    /// Resolve one Session publication and its exact model candidate from
    /// explicit frozen coordinates. This is the sole selector used by cold
    /// install and resident execution: a complete Session override wins even
    /// when no Agent publication exists, the exact Agent publication is next,
    /// and only a genuinely unpinned legacy Session has no candidate.
    pub(crate) fn resolve_canonical_session_projection(
        &self,
        workspace: &str,
        coordinates: CanonicalSessionProjection<'_>,
        delivered: Option<ExecutableAgentSnapshot>,
        skill_pins: Option<&[session_resource::ResolvedSkillBinding]>,
    ) -> Result<
        (
            Option<ExecutableAgentSnapshot>,
            Option<ResolvedModelCandidate>,
        ),
        HostError,
    > {
        let agent = coordinates.agent();
        let frozen_revision = coordinates.agent_revision();
        let installed = delivered.or_else(|| {
            self.publications.as_ref().and_then(|source| {
                let agent_id = awaken_runtime_contract::snapshot::AgentId(agent.to_string());
                match frozen_revision {
                    Some(revision) => source.at_revision(workspace, &agent_id, revision),
                    None => source.current(workspace, &agent_id),
                }
            })
        });
        let model_override = coordinates.model_override();
        if let Some(publication) =
            model_override.and_then(|override_| override_.publication.as_ref())
        {
            publication
                .validate_for_workspace(workspace)
                .map_err(|error| {
                    HostError::internal(format!(
                        "frozen Session model override publication is invalid: {error}"
                    ))
                })?;
        }
        match coordinates.publication_decision(installed.as_ref()) {
            PublicationDecision::Unpinned
            | PublicationDecision::OptionalMissing
            | PublicationDecision::Exact => {}
            PublicationDecision::MissingRequired => {
                return Err(HostError::internal(
                    "frozen Session Agent publication is unavailable",
                ));
            }
            PublicationDecision::Mismatch => {
                let actual = installed.as_ref().map(|publication| {
                    format!(
                        "agent={}, revision={}, backend={}",
                        publication.root_agent_id.0,
                        publication.metadata.source.revision,
                        publication.resolved_spec.model_binding.identity().2,
                    )
                });
                return Err(HostError::internal(format!(
                    "Session Agent/model publication does not match its frozen coordinates \
                     (expected agent={}, revision={:?}; actual={})",
                    coordinates.agent(),
                    coordinates.agent_revision(),
                    actual.as_deref().unwrap_or("missing"),
                )));
            }
        }
        let installed = installed
            .map(|snapshot| {
                let (prompt, skill_pins) = match &coordinates {
                    CanonicalSessionProjection::Baseline(baseline) => {
                        (baseline.system_prompt.as_ref(), skill_pins)
                    }
                    CanonicalSessionProjection::Layout(_) => (
                        &awaken_session_configuration_contract::SessionSystemPromptSelection::Inherit,
                        None,
                    ),
                    CanonicalSessionProjection::LegacyAgent(_) => return Ok(snapshot),
                };
                awaken_session_realization_contract::project_effective_agent_publication(
                    model_override,
                    prompt,
                    skill_pins,
                    workspace,
                    snapshot,
                )
                .map_err(|error| HostError::internal(error.to_string()))
            })
            .transpose()?;
        let candidate = model_override
            .and_then(|override_| override_.publication.as_ref())
            .map(|publication| publication.primary.clone())
            .or_else(|| {
                installed
                    .as_ref()
                    .map(|snapshot| snapshot.resolved_spec.model_binding.clone())
            });
        Ok((installed, candidate))
    }

    /// Resolve the one immutable publication selected for a Session and enforce
    /// its projected Agent/backend fences. Context construction and cold
    /// environment adoption share this boundary so provider selection cannot
    /// drift from execution selection.
    pub(crate) fn resolve_session_publication(
        &self,
        thread: &str,
        agent: Option<&str>,
        published_snapshot: Option<ExecutableAgentSnapshot>,
    ) -> Result<
        (
            String,
            String,
            Option<ExecutableAgentSnapshot>,
            Option<ResolvedModelCandidate>,
        ),
        HostError,
    > {
        let realizations = &self.sessions;
        let frozen = realizations.frozen(thread);
        let workspace = self.workspace_for(&frozen);
        let projected_agent = frozen.agent_id.as_deref();
        if let (Some(asserted), Some(projected)) = (agent, projected_agent)
            && asserted != projected
        {
            return Err(HostError::internal(format!(
                "session Agent projection `{projected}` does not match requested Agent `{asserted}`"
            )));
        }
        // A provider-driven re-entry may carry neither the original request
        // argument nor a resident Thread projection. The already delivered
        // immutable publication is authoritative in that case; defaulting to
        // `assistant` would manufacture a second Agent identity and make the
        // exact-publication fence reject its own retained snapshot.
        let delivered_agent = published_snapshot
            .as_ref()
            .map(|snapshot| snapshot.root_agent_id.0.clone());
        let selected_agent = agent
            .or(projected_agent)
            .or(delivered_agent.as_deref())
            .unwrap_or("assistant");
        let skill_pins = realizations
            .read_realization(thread, |r| {
                r.staging
                    .transition
                    .as_ref()
                    .map(|transition| transition.desired().resources.skills().to_vec())
            })
            .flatten();
        let coordinates = frozen.baseline.as_ref().map_or(
            CanonicalSessionProjection::LegacyAgent(selected_agent),
            CanonicalSessionProjection::Baseline,
        );
        let (installed, model_candidate) = self.resolve_canonical_session_projection(
            &workspace,
            coordinates,
            published_snapshot,
            skill_pins.as_deref(),
        )?;
        let published_backend_ref = model_candidate
            .as_ref()
            .map(|candidate| candidate.binding().identity().2.to_owned());
        let projected_backend_ref = &frozen.backend_ref;
        if let (Some(published), Some(projected)) = (&published_backend_ref, projected_backend_ref)
            && published != projected
        {
            return Err(HostError::internal(format!(
                "session backend projection `{projected}` does not match publication `{published}`"
            )));
        }
        if model_candidate.is_none() && projected_backend_ref.is_some() {
            return Err(HostError::internal(
                "session backend projection has no immutable model publication",
            ));
        }
        Ok((
            workspace,
            selected_agent.to_string(),
            installed,
            model_candidate,
        ))
    }

    /// Freeze one exact publication into the existing Session runtime slot.
    /// The slot is already the projection used by claimed-worker replay; direct
    /// and deferred execution reuse it instead of adding a provider-selection
    /// store or resolving a later catalog head.
    pub(crate) fn retain_session_publication(
        &self,
        thread: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), HostError> {
        self.sessions.retain_publication(thread, publication)
    }

    pub(crate) fn projected_session_model_provisioning(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, HostError> {
        let realizations = &self.sessions;
        let delivered = realizations.published_snapshot(thread);
        let (_, _, _, model_candidate) =
            self.resolve_session_publication(thread, agent, delivered)?;
        Ok(model_candidate
            .as_ref()
            .map(ResolvedModelCandidate::provisioning)
            .cloned()
            .unwrap_or(awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor))
    }
}

#[cfg(test)]
mod tests;
