//! `awaken-provisioning-contract` — the neutral, data-only vocabulary and ports
//! for provisioning a **sandbox environment** and launching processes in it.
//!
//! # Why a process-level contract
//!
//! The sandbox must host **opaque agent processes** (Claude Code, Codex, an
//! arbitrary CLI) — not only the runtime's own in-process tools. Such a process
//! does its own `open()`/`exec()`/network syscalls and never routes through our
//! tool layer, so isolation **cannot** be done by rewriting tool-call arguments.
//! It must be enforced at the **OS/process boundary** and be *transparent* to
//! whatever runs inside. Hence the primary primitive here is [`Sandbox::spawn`]
//! (launch any [`Command`] under OS-enforced isolation), not a tool wrapper. The
//! runtime's in-process `RawTool` model is a *special case* layered on top, owned
//! by other crates; a lexical path-jail is a trusted-caller convenience only and
//! is explicitly **not** an isolation boundary for a launched process.
//!
//! # Boundary discipline
//!
//! - **Data-only + ports.** This crate names no OS mechanism, no host path, no
//!   wire/DTO, and no runtime type. Concrete realizers (lexical / namespace /
//!   container) implement [`SandboxProvider`] in their own crates.
//! - **G3 — no host path crosses the boundary.** Every path in this vocabulary is
//!   *sandbox-absolute* (e.g. `/workspace`, `/mnt/session/outputs`) or a logical
//!   reference; raw host paths live only inside a provider.
//! - **Secrets by reference.** A secret env var carries a broker *reference*
//!   ([`EnvValue::Secret`]), never the secret bytes.
//!
//! The two planes are named distinctly: [`EnvironmentKind`] is the *declared*
//! control-plane config (persisted, admitted); a live realized environment is the
//! provider's own type behind [`Sandbox`].

mod admission;
mod claimed_effect;
mod execution_authority;
mod execution_policy;
mod lease;
mod prepare;
mod sandbox;
mod spec;
mod vocab;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use awaken_sandbox_control::SandboxControlServiceKind;

pub use admission::{AdmissionError, EnvironmentDecl, check_environment_soundness};
pub use claimed_effect::{ClaimedEffect, ClaimedEffectInFlight, EffectClaim};
pub use execution_authority::{
    CurrentExecutionAuthority, SandboxEffectGuard, with_current_sandbox_effect,
};
pub use execution_policy::{
    SandboxExecutionPolicy, SandboxExecutionPolicyError, SandboxExecutionPolicyId,
    SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};
pub use lease::{
    AdoptionPlan, LeaseAction, LeaseFenceCause, LeaseGrant, LeaseLiveness, LeaseTimingError,
    LeaseTimingPolicy, LivenessSignals, capped_expiry, decide_lease_action, egress_permitted,
    reconcile_adoption,
};
pub use prepare::{
    EnvironmentPlan, PrepareError, prepare_environment, prepare_trusted_host_environment,
    validate_adopted_sandbox_control_services, validate_mount_requirements,
    validate_sandbox_control_services, validate_trusted_host_mount_requirements,
};
pub use sandbox::{
    BlobSource, CheckpointObjectMetadata, CheckpointingSandbox, ExitStatus, HandleEvidence,
    IsolationClass, IsolationPolicy, MemoryMaterializationEvidence, MemoryMaterializationHead,
    MemoryMount, MemoryMountOutcome, MemoryMounter, MemoryReconciliationAck, OnUnmet,
    PolicySelection, ProcessHandle, RealizationHolder, RepositoryHttpBasicCredential,
    RepositoryPublicationError, RepositoryPublicationExpectation, RepositoryPublicationReceipt,
    RepositoryPublicationRejection, RepositoryRealizationPlan, RepositoryRealizer,
    ResourceInputDefaultMounts, RestorableProvider, RestoreTargetProvider, Sandbox,
    SandboxCapabilities, SandboxCheckpointRef, SandboxCheckpointRequest, SandboxCheckpointStore,
    SandboxCheckpointUpload, SandboxDisposalAuthorization, SandboxDisposalPreparation,
    SandboxEffectFence, SandboxEffectFenceWire, SandboxError, SandboxHandle, SandboxObservation,
    SandboxObservationBasis, SandboxPayloadSchema, SandboxProvider, SandboxRealization,
    SandboxRequirements, SandboxRestorationEvidence, SandboxRestoreRequest, SandboxRestoreResult,
    SandboxRestoreTarget, SandboxRestoreTargetDisposition, SandboxStatus, SecretBroker,
    SecretWritebackEffect, SecretWritebackTarget, SelectionError, Signal, StoredCheckpointObject,
    WorkspaceLayout, capability_requirements_satisfied, checkpoint_exclusions_fingerprint,
    memory_materialization_mount_path, resource_input_default_mounts,
    sandbox_spec_security_fingerprint, select_provider, select_provider_with_policy,
    validate_checkpoint_exclusions_for_spec, validate_repository_mount_path,
    validate_repository_mount_paths, validate_repository_sandbox_adoption_layout,
    validate_repository_sandbox_layout,
};
pub use spec::{
    Command, EnvironmentKind, FilesystemContinuity, HostAccessPolicy, MaterializedCommand,
    MaterializedEnvValue, MaterializedEnvVar, RootfsSource, SandboxCapacityShapeId,
    SandboxCheckpointExpiryBehavior, SandboxIdleRetentionMode, SandboxIdleRetentionPolicy,
    SandboxOverride, SandboxProvisioning, SandboxRealizationFingerprint, SandboxSpec, Stdio,
    materialize_process_command,
};
pub use vocab::{
    Artifact, CacheVolumeLocation, EnvValue, EnvVar, EnvVisibility, MemoryWriteConsistency,
    MountAccess, MountLifetime, MountRequirement, MountSource, NetworkPolicy, PackageRequirements,
    RESERVED_ENV_KEYS, Realization, RealizedMount, ResourceLimits, ResourceRequests,
};
