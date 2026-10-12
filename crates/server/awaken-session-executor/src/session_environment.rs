// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Value vocabulary Session realization decides with: the bound-Environment preparation
// @formal reason: mode, adoption disposition and authorized effect carry no transition of their own; the
// @formal reason: transitions and their fences stay in host::session::environment_lifecycle.
//! The Session's live execution environment and its provider, as the role
//! handles the Host holds (`awaken-session-environment-contract`), and the
//! vocabulary realization decides with: how a bound Environment is prepared,
//! adopted or rebuilt, and which publication a Session realizes. The
//! sandbox-backed adapters (`awaken-session-environment`) are installed by the
//! process composition; this crate links them only in its own tests.

#[cfg(test)]
pub(crate) use awaken_session_environment_contract::AgentFilesystemClient;
pub(crate) use awaken_session_environment_contract::{
    ProviderEffect, SessionEnvironment, SessionEnvironmentProvider,
};

#[cfg(test)]
pub(crate) use awaken_session_environment::test_support::{
    SandboxEnvironmentAccess, UnusedHandExecutorFactory, relocated_provider,
    spec as environment_test_spec,
};
#[cfg(test)]
pub(crate) use awaken_session_environment::{SandboxEnvironment, SandboxEnvironmentProvider};

use std::sync::Arc;

use crate::host_error::HostError;

pub(crate) struct EnvironmentBindingPersistenceError {
    pub(crate) error: HostError,
    /// Store-read proof that the root binding committed before a later
    /// derivative dispatch-cache edge failed. The lifecycle owner consumes
    /// this only to retain the same hidden Candidate under durable identity.
    pub(crate) committed_identity:
        Option<crate::environment_owner::BoundSessionEnvironmentIdentity>,
}

impl From<HostError> for EnvironmentBindingPersistenceError {
    fn from(error: HostError) -> Self {
        Self {
            error,
            committed_identity: None,
        }
    }
}

pub(crate) struct AuthorizedSessionEnvironmentEffect {
    /// The contract result owns the operation's sole immutable intent.
    pub(crate) authorization: awaken_session_state_contract::SessionEnvironmentEffectAuthorization,
    pub(crate) provider_fence: Option<awaken_provisioning_contract::SandboxEffectFence>,
    /// Captured before provider I/O; renewal can advance expiry but cannot
    /// lend this operation another slot allocation or Worker generation.
    pub(crate) provider_authority:
        Option<Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnvironmentAdoptionDisposition {
    NoBinding,
    Ready,
    RebuildRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionEnvironmentUnavailablePolicy {
    Reject,
    Rebuild,
}

/// Select which side of the single bound-Environment recovery seam may run.
/// A live source effect may only adopt a `Ready` provider without changing its
/// lifecycle marker; terminal recovery may reconstruct the provider's closed
/// cleanup owner. Keeping the choice explicit prevents a pre-delete source
/// effect from accidentally entering terminal takeover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoundEnvironmentPreparationMode {
    LiveSource,
    /// The aggregate's terminal Repository command may reuse its exact bound
    /// terminal-retiring owner while the provider is still Ready. It never
    /// reopens ordinary execution. A validated terminal/absent source may only
    /// produce negative publication evidence, never Git I/O; disposing stays
    /// refused without inventing an outcome of an earlier publication.
    TerminalRepositoryPublication,
    Terminal,
    /// Re-observe/reconstruct only the physical owner after the aggregate has
    /// durably admitted terminal Preparation. An existing terminal Retiring
    /// owner stays under its original preparation fence; this mode adds no
    /// source-dependent work and exists only for the Disposal boundary.
    TerminalDisposal,
}

impl BoundEnvironmentPreparationMode {
    pub(crate) fn requires_live_source(self) -> bool {
        matches!(self, Self::LiveSource | Self::TerminalRepositoryPublication)
    }
}

/// Borrowed coordinates consumed by the one Session publication/provider
/// selector. Baseline, prospective layout, and genuinely legacy callers differ
/// only in where these immutable facts are stored; selection policy must not.
pub(crate) enum CanonicalSessionProjection<'a> {
    Baseline(&'a awaken_session_configuration_contract::SessionBaseline),
    Layout(&'a awaken_session_configuration_contract::SessionSandboxLayout),
    LegacyAgent(&'a str),
}

impl CanonicalSessionProjection<'_> {
    pub(crate) fn agent(&self) -> &str {
        match self {
            Self::Baseline(baseline) => &baseline.agent_id,
            Self::Layout(layout) => &layout.agent_id,
            Self::LegacyAgent(agent) => agent,
        }
    }

    pub(crate) fn agent_revision(&self) -> Option<u64> {
        match self {
            Self::Baseline(baseline) => baseline.agent_revision,
            Self::Layout(layout) => layout.agent_revision,
            Self::LegacyAgent(_) => None,
        }
    }

    pub(crate) fn model_override(
        &self,
    ) -> Option<&awaken_session_configuration_contract::SessionModelOverride> {
        match self {
            Self::Baseline(baseline) => baseline.model_override.as_ref(),
            Self::Layout(layout) => layout.model_override.as_ref(),
            Self::LegacyAgent(_) => None,
        }
    }

    pub(crate) fn publication_decision(
        &self,
        publication: Option<&awaken_runtime_contract::ExecutableAgentSnapshot>,
    ) -> awaken_session_realization_contract::FrozenAgentPublicationDecision {
        match self {
            Self::Baseline(baseline) => {
                awaken_session_realization_contract::frozen_agent_publication_decision(
                    baseline,
                    publication,
                )
            }
            Self::Layout(layout) => {
                awaken_session_realization_contract::frozen_sandbox_layout_publication_decision(
                    layout,
                    publication,
                )
            }
            Self::LegacyAgent(agent) => {
                awaken_session_realization_contract::legacy_agent_publication_decision(
                    agent,
                    publication,
                )
            }
        }
    }
}

/// Whether any publication-pinned execution candidate needs the Session's local
/// Environment. This is deliberately a set-wide decision: an A2A primary with a
/// Native/ACP fallback is not A2A-only and must retain one realizable Environment.
pub(crate) fn requires_local_environment(
    models: &awaken_runtime_contract::resolved::ResolvedSpec,
) -> bool {
    std::iter::once(&models.model_binding)
        .chain(models.model_candidates.iter())
        .any(|candidate| {
            !matches!(
                awaken_runtime_contract::resolved::Backend::from_ref(
                    candidate.binding().identity().2
                ),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            )
        })
}

#[cfg(test)]
mod tests;
