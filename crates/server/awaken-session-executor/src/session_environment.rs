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

#[cfg(test)]
mod tests;

/// Process-local projection of which source effects remain legal after the
/// provider observation. The provider-owned [`awaken_provisioning_contract::SandboxObservation`] remains the
/// only physical fact; this mode merely keeps live I/O, source preparation,
/// and already-prepared recovery from drifting into three independent tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TerminalEffectMode {
    LiveUnprepared,
    RecoverOnlyUnprepared,
    RecoverOnlyAlreadyPrepared,
}

impl TerminalEffectMode {
    /// A stopped process environment cannot supply the attached-exec file read
    /// used by Container. Filesystem providers retain direct data access after
    /// their marker becomes terminal. Neither row proves prior preparation.
    pub(crate) const fn for_terminal_source(data_readable: bool) -> Self {
        if data_readable {
            Self::LiveUnprepared
        } else {
            Self::RecoverOnlyUnprepared
        }
    }
}

/// One exact terminal owner plus its non-durable I/O mode. This stays inside
/// runtime-host and is never serialized, acknowledged, or used as mutation
/// authority.
pub(crate) struct PreparedBoundEnvironment {
    pub(crate) environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
    durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
    pub(crate) effect_mode: TerminalEffectMode,
    definitive_absence: bool,
    closed_publication_source: Option<awaken_provisioning_contract::SandboxObservation>,
}

impl PreparedBoundEnvironment {
    pub(crate) fn new(
        environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
        durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
        effect_mode: TerminalEffectMode,
    ) -> Self {
        Self {
            environment,
            durable_handle,
            effect_mode,
            definitive_absence: false,
            closed_publication_source: None,
        }
    }

    /// Non-executable publication observation. It retains the original handle
    /// and validated fact, but neither installs a replacement Environment nor
    /// claims complete physical cleanup of auxiliary participants.
    pub(crate) fn closed_publication_source(
        handle: awaken_provisioning_contract::SandboxHandle,
        observation: awaken_provisioning_contract::SandboxObservation,
    ) -> Self {
        let mut prepared = Self::new(
            None,
            Some(handle),
            TerminalEffectMode::RecoverOnlyUnprepared,
        );
        prepared.closed_publication_source = Some(observation);
        prepared
    }

    pub(crate) fn publication_source_closure(
        &self,
    ) -> Option<&awaken_provisioning_contract::SandboxObservation> {
        self.closed_publication_source.as_ref()
    }

    pub(crate) fn with_definitive_absence(mut self) -> Self {
        debug_assert!(self.environment.is_none() && self.durable_handle.is_none());
        self.definitive_absence = true;
        self
    }

    /// Join the provider's physical observation with the aggregate's closed
    /// continuation predecessor. The latter dominates: after A is durable,
    /// total physical absence is a response-loss recovery case rather than an
    /// excuse to repeat live source effects under terminal T.
    pub(crate) fn with_preparation_authorization(
        mut self,
        authorization: &awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Self {
        if authorization.inherited_provider_disposal().is_some() {
            self.effect_mode = TerminalEffectMode::RecoverOnlyAlreadyPrepared;
        }
        self
    }

    pub(crate) fn permits_live_io(&self) -> bool {
        self.effect_mode == TerminalEffectMode::LiveUnprepared
    }

    pub(crate) fn requires_provider_preparation(&self) -> bool {
        self.effect_mode != TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    pub(crate) fn source_effects_are_already_prepared(&self) -> bool {
        self.effect_mode == TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    pub(crate) fn durable_handle(&self) -> Option<&awaken_provisioning_contract::SandboxHandle> {
        self.durable_handle.as_ref()
    }

    pub(crate) fn memory_materializations(
        &self,
    ) -> Result<
        Option<&[awaken_provisioning_contract::MemoryMaterializationEvidence]>,
        awaken_provisioning_contract::SandboxError,
    > {
        match self.durable_handle() {
            Some(handle) => handle.memory_materializations(),
            None if self.definitive_absence => Ok(Some(&[])),
            None => Ok(None),
        }
    }
}

/// Borrowed inputs for one terminal Environment installation phase. The
/// aggregate effect fence, provider spec, handle, and optional restore fence
/// remain their canonical value objects; this context owns no lifecycle fact.
pub(crate) struct TerminalEnvironmentInstallation<'a> {
    pub(crate) thread: &'a str,
    pub(crate) binding: Option<&'a str>,
    pub(crate) committed_candidate: Option<&'a crate::environment_owner::UnboundSessionEnvironment>,
    pub(crate) provider: &'a crate::session_environment::SessionEnvironmentProvider,
    pub(crate) spec: &'a awaken_provisioning_contract::SandboxSpec,
    pub(crate) handle: Option<&'a awaken_provisioning_contract::SandboxHandle>,
    pub(crate) expected_effect_fence: Option<&'a awaken_provisioning_contract::SandboxEffectFence>,
    pub(crate) effect_fence: &'a awaken_provisioning_contract::SandboxEffectFence,
}
