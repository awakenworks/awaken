// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Value vocabulary Session realization decides with: the bound-Environment preparation
// @formal reason: mode, adoption disposition and authorized effect carry no transition of their own; the
// @formal reason: transitions and their fences stay in environment_lifecycle.
//! The Session's live execution environment and its provider, as the role
//! handles the Host holds (`awaken-session-environment-contract`), and the
//! vocabulary realization decides with: how a bound Environment is prepared,
//! adopted or rebuilt, and which publication a Session realizes. The
//! sandbox-backed adapters (`awaken-session-environment`) are installed by the
//! process composition; this crate links them only in its own tests.

#[cfg(any(test, feature = "test-support"))]
pub use awaken_session_environment_contract::AgentFilesystemClient;
pub use awaken_session_environment_contract::{SandboxProviderRoles, SessionEnvironment};

use std::sync::Arc;

pub struct AuthorizedSessionEnvironmentEffect {
    /// The contract result owns the operation's sole immutable intent.
    pub authorization: awaken_session_contract::SessionEnvironmentEffectAuthorization,
    pub provider_effect: AuthorizedProviderEffect,
}

/// Who owns the provider effect of one authorized Environment operation: the
/// owned form of [`awaken_provisioning_contract::SandboxEffect`]. An unowned
/// authorization is `Unattached`; an aggregate-owned one always carries its
/// fence together with the authority captured for it, so neither can travel
/// without the other.
pub enum AuthorizedProviderEffect {
    Unattached,
    Fenced {
        fence: awaken_provisioning_contract::SandboxEffectFence,
        /// Captured before provider I/O; renewal can advance expiry but cannot
        /// lend this operation another installed Session allocation or Worker generation.
        authority: Arc<dyn awaken_provisioning_contract::CurrentExecutionAuthority>,
    },
}

impl AuthorizedProviderEffect {
    /// The aggregate fence; an unattached effect has none.
    #[must_use]
    pub fn fence(&self) -> Option<&awaken_provisioning_contract::SandboxEffectFence> {
        match self {
            Self::Unattached => None,
            Self::Fenced { fence, .. } => Some(fence),
        }
    }

    /// The borrowed effect every provider operation receives.
    #[must_use]
    pub fn sandbox_effect(&self) -> awaken_provisioning_contract::SandboxEffect<'_> {
        match self {
            Self::Unattached => awaken_provisioning_contract::SandboxEffect::Unattached,
            Self::Fenced { fence, authority } => {
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence,
                    authority: authority.as_ref(),
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentAdoptionDisposition {
    NoBinding,
    Ready,
    RebuildRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentUnavailablePolicy {
    Reject,
    Rebuild,
}

/// Select which side of the single bound-Environment recovery seam may run.
/// A live source effect may only adopt a `Ready` provider without changing its
/// lifecycle marker; terminal recovery may reconstruct the provider's closed
/// cleanup owner. Keeping the choice explicit prevents a pre-delete source
/// effect from accidentally entering terminal takeover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundEnvironmentPreparationMode {
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
    pub fn requires_live_source(self) -> bool {
        matches!(self, Self::LiveSource | Self::TerminalRepositoryPublication)
    }
}

/// What terminal preparation observed about the bound Environment's source:
/// the physical input of [`TerminalEffectMode`]. The provider-owned
/// [`awaken_provisioning_contract::SandboxObservation`] stays the only physical
/// fact; this names which case of it (or of the aggregate's binding) the
/// preparation reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalSourceObservation {
    /// No durable binding: creation never ran, the Hibernated source was
    /// already released, or the original create fence adopted (or proved
    /// absent) the owner it installs. Nothing was prepared for release yet.
    Unbound,
    /// The provider reports the bound source Ready.
    Ready,
    /// The provider reports a terminal marker. `data_readable`: the provider
    /// keeps direct data access after the marker (filesystem providers); a
    /// stopped process environment cannot serve Container's attached-exec read.
    Terminal { data_readable: bool },
    /// The provider proved the primary source absent.
    DefinitivelyUnavailable,
    /// The provider reports Disposing: durable proof that every
    /// source-durability participant crossed the provider cleanup gate.
    Disposing,
    /// The aggregate is Restoring: only the exact restore request exists.
    RestoreAwaiting,
    /// A terminal Repository publication found its source closed: negative
    /// publication evidence only, never Git I/O.
    ClosedPublicationSource,
}

/// The aggregate's frozen fact the mode joins: whether the terminal cleanup
/// authorization inherited a closed continuation's provider disposal
/// preparation. Once that is durable, total physical absence is a
/// response-loss recovery, not an excuse to repeat live source effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceReleasePredecessor {
    None,
    InheritedProviderDisposal,
}

/// Process-local projection of which source effects remain legal: live I/O,
/// source preparation, or already-prepared recovery. It is never stored or
/// written; [`TerminalEffectMode::decide`] derives it, so Artifact, Memory and
/// provider callers cannot drift into independent observation policies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalEffectMode {
    LiveUnprepared,
    RecoverOnlyUnprepared,
    RecoverOnlyAlreadyPrepared,
}

impl TerminalEffectMode {
    /// The one terminal-effect mode decision (table HT0 in `terminal_effects`).
    ///
    /// | Rule | predecessor | observation | mode |
    /// |---|---|---|---|
    /// | M1 | inherited disposal | any | RecoverOnlyAlreadyPrepared |
    /// | M2 | none | Unbound, Ready, readable Terminal | LiveUnprepared |
    /// | M3 | none | unreadable Terminal, DefinitivelyUnavailable, RestoreAwaiting, ClosedPublicationSource | RecoverOnlyUnprepared |
    /// | M4 | none | Disposing | RecoverOnlyAlreadyPrepared |
    pub const fn decide(
        observation: TerminalSourceObservation,
        predecessor: SourceReleasePredecessor,
    ) -> Self {
        use TerminalSourceObservation as O;
        match (predecessor, observation) {
            (SourceReleasePredecessor::InheritedProviderDisposal, _) | (_, O::Disposing) => {
                Self::RecoverOnlyAlreadyPrepared
            }
            (
                _,
                O::Unbound
                | O::Ready
                | O::Terminal {
                    data_readable: true,
                },
            ) => Self::LiveUnprepared,
            (
                _,
                O::Terminal {
                    data_readable: false,
                }
                | O::DefinitivelyUnavailable
                | O::RestoreAwaiting
                | O::ClosedPublicationSource,
            ) => Self::RecoverOnlyUnprepared,
        }
    }
}

/// One exact terminal owner plus its non-durable I/O mode. This stays inside
/// the process-local realization executor and is never serialized, acknowledged, or used as mutation
/// authority.
pub struct PreparedBoundEnvironment {
    pub environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
    durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
    observation: TerminalSourceObservation,
    predecessor: SourceReleasePredecessor,
    definitive_absence: bool,
    closed_publication_source: Option<awaken_provisioning_contract::SandboxObservation>,
}

impl PreparedBoundEnvironment {
    pub fn new(
        environment: Option<(Arc<crate::session_environment::SessionEnvironment>, bool)>,
        durable_handle: Option<awaken_provisioning_contract::SandboxHandle>,
        observation: TerminalSourceObservation,
    ) -> Self {
        Self {
            environment,
            durable_handle,
            observation,
            predecessor: SourceReleasePredecessor::None,
            definitive_absence: false,
            closed_publication_source: None,
        }
    }

    /// The source effects that remain legal, decided once from the
    /// observation and the frozen predecessor.
    pub fn effect_mode(&self) -> TerminalEffectMode {
        TerminalEffectMode::decide(self.observation, self.predecessor)
    }

    /// Non-executable publication observation. It retains the original handle
    /// and validated fact, but neither installs a replacement Environment nor
    /// claims complete physical cleanup of auxiliary participants.
    pub fn closed_publication_source(
        handle: awaken_provisioning_contract::SandboxHandle,
        observation: awaken_provisioning_contract::SandboxObservation,
    ) -> Self {
        let mut prepared = Self::new(
            None,
            Some(handle),
            TerminalSourceObservation::ClosedPublicationSource,
        );
        prepared.closed_publication_source = Some(observation);
        prepared
    }

    pub fn publication_source_closure(
        &self,
    ) -> Option<&awaken_provisioning_contract::SandboxObservation> {
        self.closed_publication_source.as_ref()
    }

    pub fn with_definitive_absence(mut self) -> Self {
        debug_assert!(self.environment.is_none() && self.durable_handle.is_none());
        self.definitive_absence = true;
        self
    }

    /// Join the provider's physical observation with the aggregate's closed
    /// continuation predecessor. The latter dominates: after A is durable,
    /// total physical absence is a response-loss recovery case rather than an
    /// excuse to repeat live source effects under terminal T.
    pub fn with_preparation_authorization(
        self,
        authorization: &awaken_session_execution_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Self {
        self.with_source_release_predecessor(
            if authorization.inherited_provider_disposal().is_some() {
                SourceReleasePredecessor::InheritedProviderDisposal
            } else {
                SourceReleasePredecessor::None
            },
        )
    }

    /// Record the aggregate's frozen source-release predecessor.
    pub(crate) fn with_source_release_predecessor(
        mut self,
        predecessor: SourceReleasePredecessor,
    ) -> Self {
        self.predecessor = predecessor;
        self
    }

    pub fn permits_live_io(&self) -> bool {
        self.effect_mode() == TerminalEffectMode::LiveUnprepared
    }

    pub fn requires_provider_preparation(&self) -> bool {
        self.effect_mode() != TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    pub fn source_effects_are_already_prepared(&self) -> bool {
        self.effect_mode() == TerminalEffectMode::RecoverOnlyAlreadyPrepared
    }

    pub fn durable_handle(&self) -> Option<&awaken_provisioning_contract::SandboxHandle> {
        self.durable_handle.as_ref()
    }

    pub fn memory_materializations(
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

#[cfg(test)]
mod tests {
    use super::AuthorizedProviderEffect;
    use awaken_provisioning_contract::test_support::EffectAuthority;
    use awaken_provisioning_contract::{SandboxEffect, SandboxEffectFence};
    use std::sync::Arc;

    /// Decision table for the owned provider effect of one authorization:
    /// | Rule | authorization          | fence | provider effect      | admit   |
    /// |---|---|---|---|---|
    /// | A1 | unowned                | none  | `Unattached`         | admits  |
    /// | A2 | owned, live authority  | its   | `Fenced` (same fence) | admits  |
    /// | A3 | owned, revoked         | its   | `Fenced` (same fence) | refuses |
    /// | A4 | fence without authority | -    | unrepresentable      | -       |
    #[test]
    fn authorized_provider_effect_carries_its_fence_with_its_authority() {
        let unowned = AuthorizedProviderEffect::Unattached;
        assert!(unowned.fence().is_none(), "A1 fence");
        assert!(
            matches!(unowned.sandbox_effect(), SandboxEffect::Unattached),
            "A1 effect"
        );
        assert!(unowned.sandbox_effect().admit().is_ok(), "A1 admit");

        let fence = SandboxEffectFence::new("op", "owner", "boot", 1, u64::MAX).unwrap();
        for (rule, authority, admits) in [
            ("A2", EffectAuthority::new(&fence), true),
            ("A3", EffectAuthority::denied(), false),
        ] {
            let owned = AuthorizedProviderEffect::Fenced {
                fence: fence.clone(),
                authority: Arc::new(authority),
            };
            assert_eq!(owned.fence(), Some(&fence), "{rule} fence");
            assert!(
                matches!(
                    owned.sandbox_effect(),
                    SandboxEffect::Fenced { fence: carried, .. } if carried == &fence
                ),
                "{rule} effect"
            );
            assert_eq!(
                owned.sandbox_effect().admit().is_ok(),
                admits,
                "{rule} admit"
            );
        }
    }
}
