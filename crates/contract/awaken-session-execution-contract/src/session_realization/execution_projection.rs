// @formal surface requirement=model.routing.exact_binding
// @formal reason: Pure frozen publication coordinates and complete model-candidate requirements shared by
// @formal reason: Coordinator admission, Worker placement and physical realization; no catalog, state or I/O.
//! Canonical Agent coordinates and physical requirements for Session realization.
//!
//! This existing realization boundary owns the complete frozen candidate set.
//! Consumers select a publication once, then derive provider requirements from
//! that same set without consulting mutable configuration or a catalog head.

use crate::{SessionBaseline, SessionModelOverride, SessionSandboxLayout};
use awaken_runtime_contract::{
    ExecutableAgentSnapshot,
    resolved::{Backend, ModelProvisioning, ResolvedModelCandidate, ResolvedSpec},
};

/// Borrowed coordinates consumed by the one Session publication/provider
/// selector. Baseline, prospective layout, and genuinely legacy callers differ
/// only in where these immutable facts are stored; selection policy must not.
pub enum CanonicalSessionProjection<'a> {
    /// The complete immutable inputs already frozen for a Session.
    Baseline(&'a SessionBaseline),
    /// Prospective immutable layout validated before its root is committed.
    Layout(&'a SessionSandboxLayout),
    /// An explicitly unpinned legacy caller with only an Agent identity.
    LegacyAgent(&'a str),
}

impl CanonicalSessionProjection<'_> {
    /// Agent identity selected by these frozen coordinates.
    pub fn agent(&self) -> &str {
        match self {
            Self::Baseline(baseline) => &baseline.agent_id,
            Self::Layout(layout) => &layout.agent_id,
            Self::LegacyAgent(agent) => agent,
        }
    }

    /// Exact authoring revision, when this is a pinned projection.
    pub fn agent_revision(&self) -> Option<u64> {
        match self {
            Self::Baseline(baseline) => baseline.agent_revision,
            Self::Layout(layout) => layout.agent_revision,
            Self::LegacyAgent(_) => None,
        }
    }

    /// Complete Session-local model publication or inference-only selection.
    pub fn model_override(&self) -> Option<&SessionModelOverride> {
        match self {
            Self::Baseline(baseline) => baseline.model_override.as_ref(),
            Self::Layout(layout) => layout.model_override.as_ref(),
            Self::LegacyAgent(_) => None,
        }
    }

    /// Compare a delivered publication using the one canonical frozen-coordinate law.
    pub fn publication_decision(
        &self,
        publication: Option<&ExecutableAgentSnapshot>,
    ) -> super::FrozenAgentPublicationDecision {
        match self {
            Self::Baseline(baseline) => {
                super::frozen_agent_publication_decision(baseline, publication)
            }
            Self::Layout(layout) => {
                super::frozen_sandbox_layout_publication_decision(layout, publication)
            }
            Self::LegacyAgent(agent) => {
                super::legacy_agent_publication_decision(agent, publication)
            }
        }
    }
}

/// Whether any publication-pinned execution candidate needs the Session's local
/// Environment. This is deliberately a set-wide decision: an A2A primary with a
/// Native/ACP fallback is not A2A-only and must retain one realizable Environment.
pub fn requires_local_environment(models: &ResolvedSpec) -> bool {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            !matches!(
                Backend::from_ref(candidate.binding().identity().2),
                Backend::Remote(_)
            )
        })
}

/// Whether any exact execution candidate launches an opaque ACP process inside
/// the Session Sandbox. Backend-owned ACP is realized outside that projected
/// Sandbox, so it must not strengthen this requirement.
#[must_use]
pub fn model_candidates_require_opaque_process<'a>(
    candidates: impl IntoIterator<Item = &'a ResolvedModelCandidate>,
) -> bool {
    candidates.into_iter().any(|candidate| {
        matches!(
            Backend::from_ref(candidate.binding().identity().2),
            Backend::Acp(_)
        ) && !matches!(
            candidate.provisioning(),
            ModelProvisioning::BackendOwned { .. }
        )
    })
}

/// Project the one effective frozen model set used by Session realization.
/// A complete baseline override replaces the Agent primary and fallbacks;
/// otherwise the retained immutable
/// Agent publication supplies primary plus fallbacks. Absence never triggers a
/// mutable catalog lookup at this physical-effect boundary.
#[must_use]
pub fn frozen_session_requires_opaque_process(
    published: Option<&ExecutableAgentSnapshot>,
    baseline: Option<&SessionBaseline>,
) -> bool {
    if let Some(publication) = baseline
        .and_then(|baseline| baseline.model_override.as_ref())
        .and_then(|override_| override_.publication.as_deref())
    {
        return model_candidates_require_opaque_process(
            std::iter::once(&publication.primary).chain(publication.candidates.iter()),
        );
    }
    published.is_some_and(|snapshot| {
        model_candidates_require_opaque_process(
            std::iter::once(&snapshot.resolved_spec.model_binding)
                .chain(snapshot.resolved_spec.model_candidates.iter()),
        )
    })
}

#[cfg(test)]
mod tests;
