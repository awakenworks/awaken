// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Declares the per-role ports of the provider that realizes, adopts, observes, restores
// @formal reason: and terminally reconstructs one Session environment. Effects carry the aggregate's fence
// @formal reason: and authority unchanged; the adapter revalidates them. This is not a formal-proof claim.
//! The Session's roles over the one selected Sandbox provider, and the record
//! of handles the Host carries that provider as.
//!
//! Environment supply has one abstraction, `awaken_provisioning_contract::
//! SandboxProvider`. The Session adds no second provider: it reads readiness
//! and installs shared services through the provisioning roles themselves, and
//! adds only the three roles below, each of which hands out a Hand-bound
//! [`SessionEnvironment`] rather than a bare Sandbox, carries the retained
//! instance a rebind or observation must match, or answers a Session-only
//! question (host identity, terminal data readability).

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{EnvironmentAdoptionLayout, SessionEnvironment};
use awaken_provisioning_contract::SandboxEffect;

/// What one realization does relative to the Session's previous environment.
/// Consumer: a provider whose Rebuild replaces a physical incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentRealizationKind {
    /// A fresh environment.
    Create,
    /// Replace the environment named by the effect's source binding.
    Rebuild,
}

/// Realizing a fresh environment. Consumers: Session realization, the Worker
/// projection synchronizer. Backend facts and liveness are not realization:
/// they belong to the Sandbox readiness role, which the same selected provider
/// supplies through its own handle ([`SandboxProviderRoles::readiness`]).
///
/// Readiness type decision table: RR1 a realizer handle no longer answers
/// readiness, so a caller that only reads capabilities cannot be handed
/// realization authority. This compile-only case executes no effect.
///
/// ```compile_fail,E0599
/// # use awaken_session_environment_contract::EnvironmentRealizer;
/// fn capabilities_through_realization(realizer: &dyn EnvironmentRealizer) {
///     let _ = realizer.capabilities();
/// }
/// ```
#[async_trait]
pub trait EnvironmentRealizer: Send + Sync {
    /// Whether environments run Agents as the trusted host user.
    fn supports_host_identity(&self) -> bool;

    /// The exact provider-visible spec, including provider-owned isolation
    /// and creation mounts, computed without I/O. Admission, capacity, create,
    /// adopt and restore all consume this one projection.
    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError>;

    /// Prepare immutable images through the same physical provider, regardless
    /// of whether this deployment owns never-used warm capacity.
    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError>;

    /// Realize an environment for an already-effective `spec` under `effect`.
    /// `source_handle` names the environment a Rebuild replaces.
    async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: SandboxEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError>;

    /// Realize an environment no aggregate effect owns: this provider's
    /// effective spec of `spec`, then an unattached Create. Consumers: the ACP
    /// capability probe and tests. A Session-owned creation is always fenced
    /// through [`Self::create_effective_for_effect`]; the name says which case
    /// the call site means, as `Sandbox::dispose_unattached` does.
    async fn create_unattached(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let spec = self.effective_spec(spec)?;
        self.create_effective_for_effect(
            &spec,
            SandboxEffect::Unattached,
            None,
            EnvironmentRealizationKind::Create,
        )
        .await
    }
}

/// Reopening an environment from its durable handle. Consumers: environment
/// lifecycle recovery, terminal preparation.
///
/// Observation purpose belongs to the Sandbox contract. Type-ownership rules:
/// O1 both purposes are accepted directly by this role; O2 the retired Session
/// spelling must not resolve. These compile-only cases execute no observation
/// or effect; the adapter's purpose/fence table owns runtime behavior.
///
/// ```no_run
/// # use awaken_provisioning_contract as pc;
/// # use awaken_session_environment_contract::EnvironmentAdoption;
/// # async fn inspect(
/// #     provider: &dyn EnvironmentAdoption,
/// #     spec: &pc::SandboxSpec,
/// #     handle: &pc::SandboxHandle,
/// # ) {
/// for purpose in [
///     pc::SandboxObservationPurpose::Recovery,
///     pc::SandboxObservationPurpose::TerminalCleanup,
/// ] {
///     let _ = provider.observe_effective(
///         spec, handle, None, pc::SandboxObservationBasis::Unattached, purpose,
///     ).await;
/// }
/// # }
/// ```
///
/// ```compile_fail
/// use awaken_session_environment_contract::ObservationPurpose;
/// ```
#[async_trait]
pub trait EnvironmentAdoption: Send + Sync {
    /// The exact layout this provider will reopen `handle` with, so the Host
    /// can reject a stale conflicting layout before any provider I/O.
    fn effective_adoption_layout(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<EnvironmentAdoptionLayout, pc::SandboxError>;

    /// Observe one exact durable handle without create/adopt/delete effects.
    /// `retained` is the live environment the Host still owns for it, if any.
    async fn observe_effective(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: Option<&SessionEnvironment>,
        basis: pc::SandboxObservationBasis<'_>,
        purpose: pc::SandboxObservationPurpose,
    ) -> Result<pc::SandboxObservation, pc::SandboxError>;

    /// Validate the physical evidence one closed observation carries against
    /// the durable handle.
    fn validate_closed_observation(
        &self,
        handle: &pc::SandboxHandle,
        observation: &pc::SandboxObservation,
    ) -> Result<(), pc::SandboxError>;

    /// Adopt the already-observed exact handle under `effect`.
    async fn adopt_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError>;

    /// Rebuild only the revoked process bindings of a retained environment
    /// this provider realized; never a new physical sandbox.
    async fn rebind_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: &SessionEnvironment,
        effect: SandboxEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError>;

    /// Reopen `handle` with no aggregate effect: this provider's own layout
    /// for it, then an unattached adoption. Consumer: tests; a Session-owned
    /// adoption is always fenced through [`Self::adopt_effective_for_effect`].
    async fn adopt_unattached(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let layout = self.effective_adoption_layout(spec, handle)?;
        self.adopt_effective_for_effect(&layout.spec, handle, SandboxEffect::Unattached)
            .await
    }
}

/// Finishing and restoring environments the Session no longer runs in.
/// Consumers: terminal preparation and cleanup, environment continuation.
#[async_trait]
pub trait EnvironmentTerminalRealization: Send + Sync {
    /// Whether an environment this provider realized still offers direct data
    /// access once its marker is terminal: a filesystem tier does, while a
    /// container whose file reads need a running process does not.
    fn terminal_data_is_readable(&self) -> bool;

    /// Reconstruct only the lifecycle object needed to finish an exact
    /// terminal realization; never renews a lease or starts a Hand.
    async fn prepare_terminal_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<SessionEnvironment>, pc::SandboxError>;

    /// Restore only the exact target frozen into the durable continuation
    /// operation, returning its verified handle.
    async fn restore(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxHandle, pc::SandboxError>;

    /// Dispose the exact restore target a durable request names without
    /// restoring it.
    async fn dispose_restored(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError>;
}

/// The deployment-selected Sandbox provider, held as one handle per consuming
/// role, every handle over the same adapter instance. It is not a second
/// provider abstraction: two of its handles are provisioning roles
/// ([`pc::SandboxReadiness`], [`pc::SandboxInstallation`]) and the other three
/// are the Session roles above. Consumers: Session realization (through the
/// executor's environment providers), startup selection and installation, and
/// the ACP capability probe.
///
/// Retired spelling decision table: RS1 the Session-specific provider name
/// must not resolve; environment supply has one abstraction.
///
/// ```compile_fail,E0432
/// use awaken_session_environment_contract::SessionEnvironmentProvider;
/// ```
///
/// Installation type decision table: SI2 the retired Session-specific spelling
/// must not resolve. The physical installation role has exactly one owner in
/// the provisioning contract; an alias would make the forbidden import compile.
///
/// ```compile_fail
/// use awaken_session_environment_contract::EnvironmentProviderInstallation;
/// ```
///
/// Unattached shortcut decision table: US1 the record carries handles only;
/// an unattached create or adopt is named on the role that performs it
/// ([`EnvironmentRealizer::create_unattached`],
/// [`EnvironmentAdoption::adopt_unattached`]), so the record's retired
/// shortcuts must not resolve.
///
/// ```compile_fail,E0599
/// # use awaken_provisioning_contract as pc;
/// # use awaken_session_environment_contract::SandboxProviderRoles;
/// async fn create(provider: &SandboxProviderRoles, spec: &pc::SandboxSpec) {
///     let _ = provider.create(spec).await;
/// }
/// ```
///
/// ```compile_fail,E0599
/// # use awaken_provisioning_contract as pc;
/// # use awaken_session_environment_contract::SandboxProviderRoles;
/// async fn adopt(
///     provider: &SandboxProviderRoles,
///     spec: &pc::SandboxSpec,
///     handle: &pc::SandboxHandle,
/// ) {
///     let _ = provider.adopt(spec, handle).await;
/// }
/// ```
#[derive(Clone)]
pub struct SandboxProviderRoles {
    readiness: Arc<dyn pc::SandboxReadiness>,
    realizer: Arc<dyn EnvironmentRealizer>,
    adoption: Arc<dyn EnvironmentAdoption>,
    terminal: Arc<dyn EnvironmentTerminalRealization>,
    installation: Arc<dyn pc::SandboxInstallation>,
    origin: Arc<dyn Any + Send + Sync>,
}

impl SandboxProviderRoles {
    /// The handles of one adapter that plays every role over one selected
    /// Sandbox provider.
    pub fn new<P>(provider: Arc<P>) -> Self
    where
        P: pc::SandboxReadiness
            + EnvironmentRealizer
            + EnvironmentAdoption
            + EnvironmentTerminalRealization
            + pc::SandboxInstallation
            + Any
            + Send
            + Sync
            + 'static,
    {
        Self {
            readiness: provider.clone(),
            realizer: provider.clone(),
            adoption: provider.clone(),
            terminal: provider.clone(),
            installation: provider.clone(),
            origin: provider,
        }
    }

    /// The selected provider's capabilities, checkpoint formats and liveness.
    /// Consumers: Session admission and capacity projection, managed input
    /// layout, MCP attachment, the Worker manifest and startup verification.
    #[must_use]
    pub fn readiness(&self) -> &Arc<dyn pc::SandboxReadiness> {
        &self.readiness
    }

    #[must_use]
    pub fn realizer(&self) -> &Arc<dyn EnvironmentRealizer> {
        &self.realizer
    }

    #[must_use]
    pub fn adoption(&self) -> &Arc<dyn EnvironmentAdoption> {
        &self.adoption
    }

    #[must_use]
    pub fn terminal(&self) -> &Arc<dyn EnvironmentTerminalRealization> {
        &self.terminal
    }

    #[must_use]
    pub fn installation(&self) -> &Arc<dyn pc::SandboxInstallation> {
        &self.installation
    }

    /// The adapter every handle refers to, for the composition that built it
    /// to verify its startup or relocate a copy. Consumers: the sandbox-backed
    /// adapter's test support (`relocated_provider`) and composition startup
    /// tests. The Host never reads it.
    #[must_use]
    pub fn origin(&self) -> &(dyn Any + Send + Sync) {
        self.origin.as_ref()
    }
}

#[cfg(test)]
mod tests;
