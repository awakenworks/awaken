//! Backend-erased contracts for Session-owned container environments.

use crate::ContainerHandleCodec as _;
use std::sync::Arc;

/// Source-compatible container name for the provider-neutral effect fence.
/// The contract crate is the sole field/validation authority.
pub use pc::SandboxEffectFence as ContainerEffectFence;

/// Exact physical reservation returned by the backend authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedContainer {
    pub locator: String,
    pub authority_attempt_id: String,
    pub disposition: PreparedContainerDisposition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedContainerDisposition {
    FreshReservation,
    RecoveringReservation,
    AlreadyActive,
}

impl PreparedContainer {
    pub(crate) fn fresh(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::FreshReservation,
        }
    }

    #[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
    pub(crate) fn recovering(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::RecoveringReservation,
        }
    }

    #[cfg(any(test, feature = "docker", feature = "podman", feature = "k8s"))]
    pub(crate) fn already_active(locator: String, authority_attempt_id: String) -> Self {
        Self {
            locator,
            authority_attempt_id,
            disposition: PreparedContainerDisposition::AlreadyActive,
        }
    }
}

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_sandbox_control::SandboxControlServicePublisher;

use crate::RuntimeAgentProcess;

/// Stable namespace used to discover a provider effect after a Worker process
/// replacement. Construction is centralized here so adapters cannot disagree
/// about delimiter escaping, validation, or whether a process incarnation is
/// part of durable identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ContainerRealizationNamespace(String);

impl ContainerRealizationNamespace {
    pub fn from_stable_parts<'a>(
        parts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, pc::SandboxError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"awaken-container-realization-namespace/v1\0");
        let mut count = 0_u64;
        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                return Err(pc::SandboxError::invalid(
                    "container realization namespace has an empty stable identity part",
                ));
            }
            count = count.saturating_add(1);
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        if count == 0 {
            return Err(pc::SandboxError::invalid(
                "container realization namespace requires stable identity evidence",
            ));
        }
        hasher.update(&count.to_be_bytes());
        Ok(Self(hasher.finalize().to_hex().to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod realization_namespace_tests {
    use super::*;

    #[test]
    fn stable_namespace_decision_table_uses_every_installation_fact() {
        // Cause/effect table: C1 ordered stable installation facts are
        // equal/different; C2 process incarnation changes; C3 any stable part is
        // empty. R1 exact C1 produces the same namespace across C2; R2 any C1
        // difference produces a different namespace; R3 C3 rejects rather than
        // falling back to a process identity. Callers own which installation
        // facts are stable; the generic value object owns only exact hashing.
        let first = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
        ])
        .unwrap();
        let restarted = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
        ])
        .unwrap();
        let other_workspace = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-b",
        ])
        .unwrap();
        assert_eq!(first, restarted, "R1");
        assert_ne!(first, other_workspace, "R2/C1");
        assert!(
            ContainerRealizationNamespace::from_stable_parts(["worker-installation", "",]).is_err(),
            "R3"
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerRealizationIntent {
    Create,
    /// Replace only the exact physical incarnation carried by the already
    /// authorized source SandboxHandle. Stable scope and lease age alone are
    /// never deletion authority.
    Rebuild {
        source_incarnation: String,
        /// Exact runtime-owned continuation evidence from the aggregate-authorized
        /// source handle. Kubernetes revalidates the persisted claim UID before
        /// it may recreate an absent Pod; host-bind runtimes carry `None` and
        /// therefore cannot turn an absent retained workspace into a fresh one.
        source_runtime_handle: Option<crate::ContainerContinuationHandle>,
    },
}

impl ContainerRealizationIntent {
    #[must_use]
    pub fn source_incarnation(&self) -> Option<&str> {
        match self {
            Self::Create => None,
            Self::Rebuild {
                source_incarnation, ..
            } => Some(source_incarnation),
        }
    }

    #[must_use]
    pub fn source_runtime_handle(&self) -> Option<&crate::ContainerContinuationHandle> {
        match self {
            Self::Create => None,
            Self::Rebuild {
                source_runtime_handle,
                ..
            } => source_runtime_handle.as_ref(),
        }
    }
}

/// Ephemeral lifecycle witness for one in-process create attempt. It is stamped
/// on the backend object only to distinguish response loss in this call (whose
/// host staging and Memory handles are still alive) from a prior process/call
/// that must be recreated. It is never a durable identity or deletion fence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerCreateAttempt(pub(crate) String);

impl ContainerCreateAttempt {
    pub(crate) fn fresh() -> Self {
        Self(crate::runtime_owner_id())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One canonical, borrowed context shared by every runtime realization phase.
///
/// The context does not own or reinterpret any identity. It keeps the existing
/// adoption fingerprint, effect fence, lifecycle intent, and create-attempt
/// authorities together. The mutable [`crate::ContainerPlan`] and phase-specific
/// physical fingerprint remain explicit port arguments and retain their
/// existing compile-time lifecycle.
#[derive(Clone, Copy, Debug)]
pub struct ContainerRealizationContext<'a> {
    pub scope: &'a str,
    pub adoption_fingerprint: &'a pc::SandboxRealizationFingerprint,
    pub effect_fence: Option<&'a ContainerEffectFence>,
    pub intent: &'a ContainerRealizationIntent,
    pub attempt: &'a ContainerCreateAttempt,
}

impl<'a> ContainerRealizationContext<'a> {
    #[must_use]
    pub fn new(
        scope: &'a str,
        adoption_fingerprint: &'a pc::SandboxRealizationFingerprint,
        effect_fence: Option<&'a ContainerEffectFence>,
        intent: &'a ContainerRealizationIntent,
        attempt: &'a ContainerCreateAttempt,
    ) -> Self {
        Self {
            scope,
            adoption_fingerprint,
            effect_fence,
            intent,
            attempt,
        }
    }

    #[must_use]
    pub fn with_attempt<'b>(
        &'b self,
        attempt: &'b ContainerCreateAttempt,
    ) -> ContainerRealizationContext<'b> {
        ContainerRealizationContext {
            scope: self.scope,
            adoption_fingerprint: self.adoption_fingerprint,
            effect_fence: self.effect_fence,
            intent: self.intent,
            attempt,
        }
    }
}

#[cfg(test)]
mod realization_context_tests {
    use super::*;

    #[test]
    fn realization_context_preserves_one_exact_effect_identity() {
        // Cause/effect decision table: C1 fence is absent/present; C2 intent is
        // Create/Rebuild; C3 attempt is original/recovery. Q1 one context
        // preserves scope, adoption, C1, C2, and original C3 by reference; Q2
        // replacing C3 changes only the ephemeral attempt witness. The context
        // borrows the existing intent/fence/attempt authorities and creates no
        // parallel identity. Phase-specific physical fingerprints remain typed
        // arguments on the runtime ports and are covered by the provider test.
        let spec = pc::SandboxSpec {
            process_tree_supervision: false,
            scope: "session-a".into(),
            isolation: pc::IsolationClass::Container,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: pc::NetworkPolicy::None,
            outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
            requests: Default::default(),
            limits: Default::default(),
            filesystem_continuity: pc::FilesystemContinuity::Retained,
            control_services: Default::default(),
            lease_ttl_secs: None,
        };
        let adoption = pc::SandboxRealizationFingerprint::from_spec(&spec);
        let fence =
            ContainerEffectFence::new("operation-a", "owner-a", "runtime-a", 1, u64::MAX).unwrap();
        let intent = ContainerRealizationIntent::Create;
        let attempt = ContainerCreateAttempt("attempt-a".into());
        let recovery_attempt = ContainerCreateAttempt("attempt-recovery".into());

        let context = ContainerRealizationContext::new(
            &spec.scope,
            &adoption,
            Some(&fence),
            &intent,
            &attempt,
        );
        assert_eq!(context.scope, spec.scope, "Q1 scope");
        assert_eq!(context.adoption_fingerprint, &adoption, "Q1 adoption");
        assert_eq!(context.effect_fence, Some(&fence), "Q1 fence");
        assert_eq!(context.intent, &intent, "Q1 intent");
        assert_eq!(context.attempt, &attempt, "Q1 attempt");

        let recovery = context.with_attempt(&recovery_attempt);
        assert_eq!(recovery.attempt, &recovery_attempt, "Q2 attempt");
        assert_eq!(recovery.scope, context.scope, "Q2 scope");
        assert_eq!(
            recovery.adoption_fingerprint, context.adoption_fingerprint,
            "Q2 adoption"
        );
    }
}

/// Object-safe live container environment owned by one Session.
#[async_trait]
pub trait ContainerEnvironment: pc::Sandbox + SandboxControlServicePublisher {
    /// Persist/replay the exact upload within this provider's physical owner.
    /// Does not capture bytes, upload objects, dispose, or commit Session state.
    async fn checkpoint_upload(
        &self,
        _access: crate::ContainerCheckpointAccess<'_>,
        _mutation: crate::ContainerCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "container environment does not implement a fenced checkpoint participant",
        ))
    }

    /// Borrowed proof that the creating wrapper still owns its local participants.
    /// Never serialized in a SandboxHandle or reconstructed after process loss.
    fn retained_create_attempt(&self) -> Option<&ContainerCreateAttempt> {
        None
    }

    fn outputs_path(&self) -> &str {
        "/outputs"
    }

    fn is_recovered(&self) -> bool {
        false
    }

    fn supports_live_mount_replacement(
        &self,
        _previous: &[pc::MountRequirement],
        _next: &[pc::MountRequirement],
    ) -> bool {
        false
    }

    async fn remove_live_input_path(&self, _path: &str) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "late mount removal is unsupported on this container tier",
        ))
    }

    /// Record one successfully materialized sandbox-visible tree in the
    /// provider's current V2 durable-handle evidence.
    fn record_owned_path(&self, _path: &str) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "container environment does not support durable owned-path evidence",
        ))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError>;

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, pc::SandboxError> {
        Err(pc::SandboxError::unsupported(
            "container environment does not expose a resident agent channel",
        ))
    }

    async fn read_files(&self, root: &str) -> Result<Vec<EnvironmentFile>, pc::SandboxError>;
}

/// One file harvested from a live container environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentFile {
    pub path: String,
    pub bytes: Vec<u8>,
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

/// Complete durable evidence presented to one runtime's effect-free observer.
/// The runtime adapter, not the Host, interprets backend-specific continuation
/// evidence (for example Kubernetes Pod and PVC UIDs). V1 has no realization
/// fingerprint and is therefore continuity-only: it may report Ready or
/// Provisioning but can never prove destructive unavailability.
#[derive(Debug, Clone, Copy)]
pub struct ContainerObservationExpectation<'a> {
    pub purpose: ContainerObservationPurpose,
    pub container_id: &'a str,
    pub adoption_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub realization_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub runtime_handle: Option<&'a crate::ContainerContinuationHandle>,
    pub effect_fence: Option<&'a pc::SandboxEffectFence>,
    pub retained_attempt: Option<&'a ContainerCreateAttempt>,
}

/// One durable container handle decoded once by its owning codec, so an
/// observation expectation can borrow its locator and fingerprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedContainerHandle {
    pub payload: crate::ContainerSandboxHandleV1,
    pub adoption_fingerprint: Option<pc::SandboxRealizationFingerprint>,
    pub realization_fingerprint: Option<pc::SandboxRealizationFingerprint>,
}

impl DecodedContainerHandle {
    pub fn decode(handle: &pc::SandboxHandle) -> Result<Self, pc::SandboxError> {
        Ok(Self {
            payload: handle.container_payload()?,
            adoption_fingerprint: handle.container_adoption_fingerprint()?,
            realization_fingerprint: handle.realization_fingerprint().cloned(),
        })
    }
}

impl<'a> ContainerObservationExpectation<'a> {
    #[must_use]
    pub fn from_decoded(handle: &'a DecodedContainerHandle) -> Self {
        Self {
            purpose: ContainerObservationPurpose::Recovery,
            container_id: &handle.payload.container_id,
            adoption_fingerprint: handle.adoption_fingerprint.as_ref(),
            realization_fingerprint: handle.realization_fingerprint.as_ref(),
            runtime_handle: handle.payload.runtime_handle.as_ref(),
            effect_fence: None,
            retained_attempt: None,
        }
    }

    #[must_use]
    pub fn from_decoded_for_effect(
        handle: &'a DecodedContainerHandle,
        effect_fence: &'a pc::SandboxEffectFence,
    ) -> Self {
        Self {
            effect_fence: Some(effect_fence),
            ..Self::from_decoded(handle)
        }
    }
}

/// Who owns one container realization effect. A fence travels together with
/// the current authority that admits it, so a fence without authority, or an
/// omitted fence that silently selects the unfenced path, cannot be written.
#[derive(Clone, Copy)]
pub(crate) enum ContainerEffect<'a> {
    /// No aggregate effect owns the environment.
    Unattached,
    /// An aggregate-authorized effect and its current authority.
    Fenced {
        fence: &'a ContainerEffectFence,
        authority: &'a dyn pc::CurrentExecutionAuthority,
    },
}

impl<'a> ContainerEffect<'a> {
    pub(crate) const fn fence(self) -> Option<&'a ContainerEffectFence> {
        match self {
            Self::Unattached => None,
            Self::Fenced { fence, .. } => Some(fence),
        }
    }

    pub(crate) fn authority(self) -> Option<&'a dyn pc::CurrentExecutionAuthority> {
        match self {
            Self::Unattached => None,
            Self::Fenced { authority, .. } => Some(authority),
        }
    }

    /// Enter current authority for a fenced effect; an unattached effect has
    /// no authority to enter.
    pub(crate) fn admit(self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Unattached => Ok(()),
            Self::Fenced { fence, authority } => {
                pc::with_current_sandbox_effect(fence, authority, |_| Ok(()))
            }
        }
    }
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
        effect_fence: &ContainerEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
        intent: ContainerRealizationIntent,
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
        effect_fence: &ContainerEffectFence,
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
        expected_effect_fence: Option<&ContainerEffectFence>,
        terminal_effect_fence: &ContainerEffectFence,
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
