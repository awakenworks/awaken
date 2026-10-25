// @formal surface requirement=model.routing.exact_binding
// @formal reason: Frozen Session coordinates select and retain the exact Agent/model publication through
// @formal reason: the same installed Session lock; context, cold replay and provider selection share this owner.
//! Immutable Agent/model publication selection for installed Sessions.
//!
//! The component sees only the publication source selected at construction,
//! the process Workspace fallback and the installed Sessions. It
//! neither owns inference executors nor reaches back into the Host.

use crate::realization_error::RealizationError;
use std::sync::Arc;

use crate::session_realizations::SessionRealizations;
use awaken_runtime_contract::{ExecutableAgentSnapshot, resolved::ResolvedModelCandidate};
use awaken_session_configuration_contract::resource as session_resource;
use awaken_session_realization_contract::CanonicalPublicationError;
use awaken_session_realization_contract::CanonicalSessionProjection;

pub struct SessionPublicationSelection {
    sessions: SessionRealizations,
    local_workspace: String,
    publications: Option<Arc<dyn awaken_runtime_contract::PublishedAgentSnapshotSource>>,
}

impl SessionPublicationSelection {
    pub fn new(
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
    pub fn workspace(&self, session: &str) -> Result<String, RealizationError> {
        Ok(self.workspace_for(self.sessions.frozen(session)?.as_ref()))
    }

    pub fn local_workspace(&self) -> &str {
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
    pub fn resolve_canonical_session_projection(
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
        RealizationError,
    > {
        awaken_session_realization_contract::resolve_canonical_session_projection(
            self.publications.as_deref(),
            workspace,
            coordinates,
            delivered,
            skill_pins,
        )
        .map_err(|error| match error {
            CanonicalPublicationError::ModelOverridePublication(error) => {
                crate::realization_error::EvidenceFault::ModelOverridePublication(error).into()
            }
            CanonicalPublicationError::Projection(error) => RealizationError::from(error),
            CanonicalPublicationError::MissingPublication => {
                RealizationError::invariant("frozen Session Agent publication is unavailable")
            }
            CanonicalPublicationError::PublicationMismatch {
                expected_agent,
                expected_revision,
                actual,
            } => RealizationError::invariant(format!(
                "Session Agent/model publication does not match its frozen coordinates \
                 (expected agent={expected_agent}, revision={expected_revision:?}; actual={actual})"
            )),
        })
    }

    /// Resolve the one immutable publication selected for a Session and enforce
    /// its projected Agent/backend fences. Context construction and cold
    /// environment adoption share this boundary so provider selection cannot
    /// drift from execution selection.
    pub fn resolve_session_publication(
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
        RealizationError,
    > {
        let realizations = &self.sessions;
        let frozen = realizations.frozen(thread)?;
        let workspace = self.workspace_for(&frozen);
        let projected_agent = frozen.agent_id.as_deref();
        if let (Some(asserted), Some(projected)) = (agent, projected_agent)
            && asserted != projected
        {
            return Err(RealizationError::invariant(format!(
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
                r.staging()
                    .transition
                    .as_ref()
                    .map(|transition| transition.desired().resources.skills().to_vec())
            })?
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
            return Err(RealizationError::invariant(format!(
                "session backend projection `{projected}` does not match publication `{published}`"
            )));
        }
        if model_candidate.is_none() && projected_backend_ref.is_some() {
            return Err(RealizationError::invariant(
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

    /// Freeze one exact publication into the existing installed Session.
    /// The installed Session is already the projection used by claimed-worker replay; direct
    /// and deferred execution reuse it instead of adding a provider-selection
    /// store or resolving a later catalog head.
    pub fn retain_session_publication(
        &self,
        thread: &str,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> Result<(), RealizationError> {
        self.sessions.retain_publication(thread, publication)
    }

    pub fn projected_session_model_provisioning(
        &self,
        thread: &str,
        agent: Option<&str>,
    ) -> Result<awaken_runtime_contract::resolved::ModelProvisioning, RealizationError> {
        let realizations = &self.sessions;
        let delivered = realizations.published_snapshot(thread)?;
        let (_, _, _, model_candidate) =
            self.resolve_session_publication(thread, agent, delivered)?;
        Ok(model_candidate
            .as_ref()
            .map(ResolvedModelCandidate::provisioning)
            .cloned()
            .unwrap_or(awaken_runtime_contract::resolved::ModelProvisioning::HostExecutor))
    }
}
