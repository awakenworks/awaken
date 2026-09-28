// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Defines the fail-closed backend-erased container provider and live-environment
// @formal reason: authority boundary; this is a product-requirement classification, not a formal-proof claim.
//! The backend-erased provider of Session-owned container environments.

use awaken_provisioning_contract as pc;

use std::sync::Arc;

use async_trait::async_trait;

use super::{ContainerCreateAttempt, ContainerEnvironment};

/// Which physical realization one Session Environment effect asks a container
/// provider for. Consumer: the Session owner's create path, which knows only the
/// already-authorized source handle; the provider interprets its own payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentRealizationIntent {
    /// Realize a new physical environment.
    Create,
    /// Replace only the exact physical incarnation recorded in the
    /// aggregate-authorized `source` handle. Stable scope and lease age alone
    /// are never deletion authority; the provider decodes the incarnation (and
    /// any runtime continuation evidence) from this handle and refuses one it
    /// cannot interpret.
    Rebuild { source: Box<pc::SandboxHandle> },
}

/// Canonical inputs required to reattach one existing container environment.
///
/// The durable handle identifies the runtime object; the frozen Sandbox
/// specification remains the authority for security-sensitive mount and
/// writable-root policy. Keeping both inputs in one typed request prevents
/// adapters from reconstructing policy from an intentionally minimal handle.
#[derive(Debug, Clone, Copy)]
pub struct ContainerEnvironmentAdoption<'a> {
    pub spec: &'a pc::SandboxSpec,
    pub handle: &'a pc::SandboxHandle,
    pub retained_attempt: Option<&'a ContainerCreateAttempt>,
    /// Observation semantics only; never authorizes adoption or deletion.
    pub observation_purpose: ContainerObservationPurpose,
}

impl<'a> ContainerEnvironmentAdoption<'a> {
    /// The observation basis for revalidating this adoption at an
    /// aggregate-authorized effect boundary: its own effective spec and `fence`.
    #[must_use]
    pub const fn effect_basis(
        &self,
        effect_fence: &'a pc::SandboxEffectFence,
    ) -> pc::SandboxObservationBasis<'a> {
        pc::SandboxObservationBasis::Effect {
            spec: self.spec,
            effect_fence,
        }
    }

    #[must_use]
    pub const fn new(spec: &'a pc::SandboxSpec, handle: &'a pc::SandboxHandle) -> Self {
        Self {
            spec,
            handle,
            retained_attempt: None,
            observation_purpose: ContainerObservationPurpose::Recovery,
        }
    }
}

/// What absence must establish for the caller. Recovery may retain durable
/// continuation bytes; terminal cleanup must never acknowledge those bytes as
/// gone merely because their execution process disappeared. Neither purpose
/// authorizes a mutation or substitutes for the aggregate effect fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerObservationPurpose {
    Recovery,
    TerminalCleanup,
}

/// The layout a container provider reopens from one of its durable handles:
/// the outputs root and the base process environment frozen at creation.
/// Consumer: the Session owner's pure adoption-layout preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerAdoptionLayout {
    pub outputs_path: String,
    pub base_env: Vec<pc::EnvVar>,
}

/// Backend-erased provider for Session-owned container environments.
///
/// This is the container projection of [`pc::SandboxProvider`]: the same
/// ownership split (an unattached environment versus one owned by an aggregate
/// effect), the same typed observation basis, and the same required terminal
/// obligation. It differs only where containers genuinely differ: environments
/// are shared `Arc<dyn ContainerEnvironment>` values carrying create-attempt and
/// checkpoint-upload participants, adoption carries retained attempt evidence,
/// and the Memory mounter and Secret broker are installed after composition.
/// There is no `Option` fence: an unfenced path is a separately named method.
#[async_trait]
pub trait ContainerEnvironmentProvider: Send + Sync {
    /// Prepare only immutable images from canonical mount-less specs. No Session
    /// filesystem is allocated and success acknowledges demand, not readiness.
    async fn prewarm_images(&self, _specs: &[pc::SandboxSpec]) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities::default()
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {}

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {}

    async fn probe_ready(&self) -> Result<(), pc::SandboxError>;

    /// The layout this provider reopens from `handle`. The handle payload is
    /// opaque outside the provider's own codec, so a Session owner asks here
    /// before any adoption I/O instead of decoding it.
    fn adoption_layout(
        &self,
        handle: &pc::SandboxHandle,
    ) -> Result<ContainerAdoptionLayout, pc::SandboxError>;

    /// The exact physical incarnation recorded in `handle`: the evidence a
    /// closed observation must name. Fails closed for a handle that carries no
    /// current incarnation evidence.
    fn physical_incarnation(&self, handle: &pc::SandboxHandle) -> Result<String, pc::SandboxError>;

    /// Create an environment no aggregate effect owns: warm capacity, a
    /// capability probe, a cache-volume initializer, or an unowned Session.
    async fn create_unattached_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    /// Realize one environment for an aggregate-authorized effect. The fence is
    /// recorded as backend evidence; `authority` is re-entered after every
    /// backend wait and before publishing the result.
    async fn create_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
        intent: EnvironmentRealizationIntent,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    /// Observe one exact durable handle without creating, adopting, renewing,
    /// or deleting it. `basis` has the same meaning as for
    /// [`pc::SandboxProvider::observe`]; with an effect basis, its spec must be
    /// the adoption's effective spec.
    async fn observe_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        basis: pc::SandboxObservationBasis<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError>;

    /// Adopt an environment no aggregate effect owns.
    async fn adopt_unattached_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    /// Revalidate and adopt under the already-authorized Session Environment
    /// effect, re-observing the exact handle without treating an indeterminate
    /// backend failure as Gone.
    async fn adopt_environment_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    /// Reconstruct the exact terminal substrate and its frozen cleanup
    /// participants without renewing it or making it runnable. The caller has
    /// supplied the current operation-bound authority; implementations must
    /// re-enter it after waits and before subsequent effects or success,
    /// including exact absence. Implementations must re-observe
    /// the exact optional handle before returning an environment that may first
    /// enter `Sandbox::prepare_disposal_for_effect` and, only after aggregate
    /// admission of that preparation, `Sandbox::dispose_for_effect`. A prior
    /// effect fence is evidence for an
    /// in-flight continuation takeover, never mutation authority. Returning
    /// `None` is reserved for provider-proved exact absence.
    async fn prepare_terminal_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        handle: Option<&pc::SandboxHandle>,
        expected_effect_fence: Option<&pc::SandboxEffectFence>,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<Option<Arc<dyn ContainerEnvironment>>, pc::SandboxError>;

    /// Exact-restore target capability. `None` means this provider cannot own
    /// a restore target, and the caller refuses at selection time instead of
    /// discovering it at the physical effect (ADR-0081 capability ladder).
    fn restore_targets(&self) -> Option<&dyn RestoreTargetContainerEnvironmentProvider> {
        None
    }

    /// Checkpoint-byte restoration capability. Restorable providers also own
    /// restore targets; [`restore_ladder_is_consistent`] states that law.
    fn restorable(&self) -> Option<&dyn RestorableContainerEnvironmentProvider> {
        None
    }
}

/// Ladder law: a provider that can restore checkpoint bytes must also be able
/// to acquire and dispose the exact physical target those bytes land in.
#[must_use]
pub fn restore_ladder_is_consistent(provider: &dyn ContainerEnvironmentProvider) -> bool {
    provider.restorable().is_none() || provider.restore_targets().is_some()
}

/// Container providers that own the one physical target of an exact restore
/// effect. Canonical checkpoint decorators use this instead of ordinary create.
#[async_trait]
pub trait RestoreTargetContainerEnvironmentProvider: ContainerEnvironmentProvider {
    /// Acquire the one physical target for an exact restore effect.
    async fn acquire_restore_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn ContainerEnvironment>>, pc::SandboxError>;

    /// Remove only the unpublished physical target named by one durable
    /// restoring tuple under the current disposal authorization.
    async fn dispose_restored_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError>;
}

/// Container providers that can materialize verified checkpoint bytes into an
/// acquired restore target and return canonical restore evidence.
#[async_trait]
pub trait RestorableContainerEnvironmentProvider:
    RestoreTargetContainerEnvironmentProvider
{
    async fn restore_environment(
        &self,
        spec: &pc::SandboxSpec,
        request: &pc::SandboxRestoreRequest,
        store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxRestoreResult<Arc<dyn ContainerEnvironment>>, pc::SandboxError>;
}

/// Backend-erased lifecycle for never-used container capacity.
///
/// This is deliberately separate from [`ContainerEnvironmentProvider`]: every
/// container backend can create a Session environment, while only a deployment
/// that opted into a warm pool owns pre-created capacity. A composition root keeps
/// this handle long enough to prewarm before advertising readiness and to drain the
/// unused capacity during shutdown.
#[async_trait]
pub trait ContainerEnvironmentCapacity: Send + Sync {
    /// Ensure at least `target` ready, never-used containers exist for `spec`'s
    /// exact creation shape. Returns the resulting ready count. Non-poolable specs
    /// (currently any spec with mounts) return zero without creating anything.
    async fn prewarm_to(
        &self,
        spec: &pc::SandboxSpec,
        target: usize,
    ) -> Result<usize, pc::SandboxError>;

    /// Current ready, never-used capacity for one exact shape. Receipts are
    /// derived from this value so an eviction or Session checkout cannot leave a
    /// stale positive placement hint.
    fn ready_capacity(&self, spec: &pc::SandboxSpec) -> usize;

    /// Dispose unused capacity for one exact shape. This is used when the
    /// authoritative current Environment catalog no longer desires an older
    /// revision; active Session environments have already left capacity.
    async fn discard_shape(&self, spec: &pc::SandboxSpec);

    /// Stop replenishment and dispose all never-used capacity. Active Session
    /// environments have already left the pool and are not affected.
    async fn shutdown_capacity(&self);
}
