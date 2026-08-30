//! Backend-erased contracts for Session-owned container environments.

use std::sync::Arc;

/// Source-compatible container name for the provider-neutral effect fence.
/// The contract crate is the sole field/validation authority.
pub use pc::SandboxEffectFence as ContainerEffectFence;

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
                return Err(pc::SandboxError::new(
                    "container realization namespace has an empty stable identity part",
                ));
            }
            count = count.saturating_add(1);
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        if count == 0 {
            return Err(pc::SandboxError::new(
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
        // Cause/effect table: C1 installation workspace equal/different; C2
        // stable Worker identity equal/different; C3 process incarnation changes;
        // C4 any stable part is empty. R1 exact C1+C2 produces the same namespace
        // across C3; R2 either stable fact differs produces a different namespace;
        // R3 C4 rejects rather than falling back to a process identity.
        let first = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
            "worker-a",
        ])
        .unwrap();
        let restarted = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
            "worker-a",
        ])
        .unwrap();
        let other_workspace = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-b",
            "worker-a",
        ])
        .unwrap();
        let other_worker = ContainerRealizationNamespace::from_stable_parts([
            "worker-installation",
            "workspace-a",
            "worker-b",
        ])
        .unwrap();
        assert_eq!(first, restarted, "R1");
        assert_ne!(first, other_workspace, "R2/C1");
        assert_ne!(first, other_worker, "R2/C2");
        assert!(
            ContainerRealizationNamespace::from_stable_parts([
                "worker-installation",
                "",
                "worker-a",
            ])
            .is_err(),
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
        source_runtime_handle: Option<pc::ContainerContinuationHandle>,
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
    pub fn source_runtime_handle(&self) -> Option<&pc::ContainerContinuationHandle> {
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
        Err(pc::SandboxError::new(
            "late mount removal is unsupported on this container tier",
        ))
    }

    /// Record one successfully materialized sandbox-visible tree in the
    /// provider's current V2 durable-handle evidence.
    fn record_owned_path(&self, _path: &str) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container environment does not support durable owned-path evidence",
        ))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError>;

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannel>, pc::SandboxError> {
        Err(pc::SandboxError::new(
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
}

impl<'a> ContainerEnvironmentAdoption<'a> {
    #[must_use]
    pub const fn new(spec: &'a pc::SandboxSpec, handle: &'a pc::SandboxHandle) -> Self {
        Self { spec, handle }
    }
}

/// Complete durable evidence presented to one runtime's effect-free observer.
/// The runtime adapter, not the Host, interprets backend-specific continuation
/// evidence (for example Kubernetes Pod and PVC UIDs). V1 has no realization
/// fingerprint and is therefore continuity-only: it may report Ready or
/// Provisioning but can never prove destructive unavailability.
#[derive(Debug, Clone, Copy)]
pub struct ContainerObservationExpectation<'a> {
    pub container_id: &'a str,
    pub adoption_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub realization_fingerprint: Option<&'a pc::SandboxRealizationFingerprint>,
    pub runtime_handle: Option<&'a pc::ContainerContinuationHandle>,
    pub effect_fence: Option<&'a pc::SandboxEffectFence>,
}

impl<'a> ContainerObservationExpectation<'a> {
    pub fn from_handle(handle: &'a pc::SandboxHandle) -> Result<Self, pc::SandboxError> {
        let payload = handle.container_payload()?;
        Ok(Self {
            container_id: &payload.container_id,
            adoption_fingerprint: handle.container_adoption_fingerprint()?,
            realization_fingerprint: handle.realization_fingerprint(),
            runtime_handle: payload.runtime_handle.as_ref(),
            effect_fence: None,
        })
    }

    pub fn from_handle_for_effect(
        handle: &'a pc::SandboxHandle,
        effect_fence: &'a pc::SandboxEffectFence,
    ) -> Result<Self, pc::SandboxError> {
        Ok(Self {
            effect_fence: Some(effect_fence),
            ..Self::from_handle(handle)?
        })
    }
}

/// Backend-erased provider for Session-owned container environments.
#[async_trait]
pub trait ContainerEnvironmentProvider: Send + Sync {
    fn sandbox_capabilities(&self) -> pc::SandboxCapabilities {
        pc::SandboxCapabilities {
            isolation: pc::IsolationClass::Workdir,
            tool_transparent: false,
            path_fidelity: false,
            enforced_readonly: false,
            network_isolation: false,
            enforced_network_allowlist: false,
            secret_egress_substitution: false,
            resource_limits: false,
            custom_rootfs: false,
            package_provisioning: false,
            control_services: Default::default(),
        }
    }

    fn checkpoint_formats(&self) -> Vec<String> {
        Vec::new()
    }

    fn install_memory_mounter(&self, _mounter: Arc<dyn pc::MemoryMounter>) {}

    fn install_secret_broker(&self, _broker: Arc<dyn pc::SecretBroker>) {}

    async fn probe_ready(&self) -> Result<(), pc::SandboxError>;

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    async fn create_environment_for_effect(
        &self,
        spec: &pc::SandboxSpec,
        effect_fence: Option<&ContainerEffectFence>,
        intent: ContainerRealizationIntent,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        if effect_fence.is_some() {
            return Err(pc::SandboxError::new(
                "container provider does not implement fenced Environment creation",
            ));
        }
        let _ = intent;
        self.create_environment(spec).await
    }

    async fn observe_environment(
        &self,
        _adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement effect-free observation",
        ))
    }

    async fn observe_environment_for_effect(
        &self,
        _adoption: ContainerEnvironmentAdoption<'_>,
        _effect_fence: &pc::SandboxEffectFence,
    ) -> Result<pc::SandboxObservation, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement fenced Environment observation",
        ))
    }

    async fn adopt_environment(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError>;

    /// Revalidate and adopt under the already-authorized Session Environment
    /// effect. The additive default preserves legacy unfenced providers while
    /// failing closed for durable callers until the provider can re-observe the
    /// exact handle without treating an indeterminate backend failure as Gone.
    async fn adopt_environment_for_effect(
        &self,
        adoption: ContainerEnvironmentAdoption<'_>,
        effect_fence: Option<&ContainerEffectFence>,
    ) -> Result<Arc<dyn ContainerEnvironment>, pc::SandboxError> {
        if effect_fence.is_some() {
            return Err(pc::SandboxError::new(
                "container provider does not implement fenced Environment adoption",
            ));
        }
        self.adopt_environment(adoption).await
    }

    /// Reconstruct the exact terminal substrate and its frozen cleanup
    /// participants without renewing it or making it runnable. The caller has
    /// already authorized the terminal fence; implementations must re-observe
    /// the exact optional handle before returning an environment that may first
    /// enter `Sandbox::prepare_disposal_for_effect` and, only after aggregate
    /// admission of that preparation, `Sandbox::dispose_for_effect`. A prior
    /// effect fence is evidence for an
    /// in-flight continuation takeover, never mutation authority. Returning
    /// `None` is reserved for provider-proved exact absence.
    async fn prepare_terminal_environment_for_effect(
        &self,
        _spec: &pc::SandboxSpec,
        _handle: Option<&pc::SandboxHandle>,
        _expected_effect_fence: Option<&ContainerEffectFence>,
        _terminal_effect_fence: &ContainerEffectFence,
    ) -> Result<Option<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement fenced terminal preparation",
        ))
    }

    /// Acquire the one physical target for an exact restore effect. Canonical
    /// checkpoint decorators use this instead of ordinary create.
    async fn acquire_restore_environment(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
    ) -> Result<pc::SandboxRestoreTarget<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement exact restore target acquisition",
        ))
    }

    async fn restore_environment(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
        _store: &dyn pc::SandboxCheckpointStore,
    ) -> Result<pc::SandboxRestoreResult<Arc<dyn ContainerEnvironment>>, pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement checkpoint restore",
        ))
    }

    async fn dispose_restored_environment(
        &self,
        _spec: &pc::SandboxSpec,
        _request: &pc::SandboxRestoreRequest,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new(
            "container provider does not implement exact restored-target disposal",
        ))
    }
}
