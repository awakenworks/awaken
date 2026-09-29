// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Declares the per-role ports of the provider that realizes, adopts, observes, restores
// @formal reason: and terminally reconstructs one Session environment. Effects carry the aggregate's fence
// @formal reason: and authority unchanged; the adapter revalidates them. This is not a formal-proof claim.
//! The provider of a Session's environment, as one port per consuming role,
//! and the record of handles the Host carries it as.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use crate::{EnvironmentAdoptionLayout, ProviderEffect, SessionEnvironment};

/// What one realization does relative to the Session's previous environment.
/// Consumer: a provider whose Rebuild replaces a physical incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentRealizationKind {
    /// A fresh environment.
    Create,
    /// Replace the environment named by the effect's source binding.
    Rebuild,
}

/// Why the Host observes a durable environment handle. Observation never
/// authorizes a mutation; the purpose only selects what evidence a provider
/// may report. Consumers: environment recovery and terminal cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationPurpose {
    Recovery,
    TerminalCleanup,
}

/// Realizing a fresh environment. Consumers: Session realization, the Worker
/// projection synchronizer.
#[async_trait]
pub trait EnvironmentRealizer: Send + Sync {
    /// What every environment this provider realizes enforces.
    fn capabilities(&self) -> pc::SandboxCapabilities;

    /// Whether environments run Agents as the trusted host user.
    fn supports_host_identity(&self) -> bool;

    /// The exact provider-visible spec, including provider-owned isolation
    /// and creation mounts, computed without I/O. Admission, capacity, create,
    /// adopt and restore all consume this one projection.
    fn effective_spec(&self, spec: &pc::SandboxSpec) -> Result<pc::SandboxSpec, pc::SandboxError>;

    /// Realize an environment for an already-effective `spec` under `effect`.
    /// `source_handle` names the environment a Rebuild replaces.
    async fn create_effective_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect: ProviderEffect<'_>,
        source_handle: Option<&pc::SandboxHandle>,
        kind: EnvironmentRealizationKind,
    ) -> Result<SessionEnvironment, pc::SandboxError>;
}

/// Reopening an environment from its durable handle. Consumers: environment
/// lifecycle recovery, terminal preparation.
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
        purpose: ObservationPurpose,
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
        effect: ProviderEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError>;

    /// Rebuild only the revoked process bindings of a retained environment
    /// this provider realized; never a new physical sandbox.
    async fn rebind_retained(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        retained: &SessionEnvironment,
        effect: ProviderEffect<'_>,
    ) -> Result<SessionEnvironment, pc::SandboxError>;
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

/// Never-used capacity ahead of demand. A provider without a capacity owner
/// reports none. Consumer: warm capacity.
#[async_trait]
pub trait EnvironmentCapacity: Send + Sync {
    /// Pull the images these specs will need.
    async fn prewarm_images(&self, specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError>;

    /// Pre-create capacity for `spec` up to `target`, returning how many are
    /// ready.
    async fn prewarm(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError>;

    /// Drop unused capacity of `spec`'s shape.
    async fn discard_capacity(&self, spec: &pc::SandboxSpec);

    /// Ready unused capacity of `spec`'s shape.
    fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize;

    /// Drain every unused capacity; live environments keep their lifecycle.
    async fn shutdown_capacity(&self);
}

/// Process-startup installation of the services every environment shares.
/// Consumer: Host startup.
pub trait EnvironmentProviderInstallation: Send + Sync {
    /// The governed MemoryStore mount adapter.
    fn install_memory_mounter(&self, mounter: Arc<dyn pc::MemoryMounter>);

    /// The credential-file broker for secret mounts.
    fn install_secret_broker(&self, broker: Arc<dyn pc::SecretBroker>);
}

/// The provider of a Session's environment, carried as one handle per
/// consuming role over the same adapter. Consumers: every Host module listed
/// on the roles above.
#[derive(Clone)]
pub struct SessionEnvironmentProvider {
    realizer: Arc<dyn EnvironmentRealizer>,
    adoption: Arc<dyn EnvironmentAdoption>,
    terminal: Arc<dyn EnvironmentTerminalRealization>,
    capacity: Arc<dyn EnvironmentCapacity>,
    installation: Arc<dyn EnvironmentProviderInstallation>,
    origin: Arc<dyn Any + Send + Sync>,
}

impl SessionEnvironmentProvider {
    /// The handles of one adapter that plays every provider role.
    pub fn new<P>(provider: Arc<P>) -> Self
    where
        P: EnvironmentRealizer
            + EnvironmentAdoption
            + EnvironmentTerminalRealization
            + EnvironmentCapacity
            + EnvironmentProviderInstallation
            + Any
            + Send
            + Sync
            + 'static,
    {
        Self {
            realizer: provider.clone(),
            adoption: provider.clone(),
            terminal: provider.clone(),
            capacity: provider.clone(),
            installation: provider.clone(),
            origin: provider,
        }
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
    pub fn capacity(&self) -> &Arc<dyn EnvironmentCapacity> {
        &self.capacity
    }

    #[must_use]
    pub fn installation(&self) -> &Arc<dyn EnvironmentProviderInstallation> {
        &self.installation
    }

    /// Realize an unattached environment for `spec`: the effective spec, then
    /// an unfenced create. Consumers: capability probes, the Worker projection
    /// synchronizer, and tests; owned creation is always fenced.
    pub async fn create(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let spec = self.realizer.effective_spec(spec)?;
        self.realizer
            .create_effective_for_effect(
                &spec,
                ProviderEffect::Unattached,
                None,
                EnvironmentRealizationKind::Create,
            )
            .await
    }

    /// Adopt `handle` unattached through its own layout: the pure preflight,
    /// then an unfenced adoption. Consumer: tests; owned adoption is fenced.
    pub async fn adopt(
        &self,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
    ) -> Result<SessionEnvironment, pc::SandboxError> {
        let layout = self.adoption.effective_adoption_layout(spec, handle)?;
        self.adoption
            .adopt_effective_for_effect(&layout.spec, handle, ProviderEffect::Unattached)
            .await
    }

    /// The adapter behind the handles, for the composition that built it to
    /// reconfigure a copy. The Host never reads it.
    #[must_use]
    pub fn origin(&self) -> &(dyn Any + Send + Sync) {
        self.origin.as_ref()
    }
}
