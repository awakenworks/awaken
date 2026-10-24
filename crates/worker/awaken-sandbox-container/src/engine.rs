//! The vocabulary a container engine adapter shares with this provider.
//!
//! The provider drives every engine through the [`crate::ContainerRuntime`]
//! port. An engine adapter that lives in its own crate (the Kubernetes engine)
//! must stamp, read and recover exactly the evidence the in-crate Docker and
//! Podman adapters do, so the shared object labels, the effect-fence and
//! restoration metadata codecs, the exec PID protocol, the existing-realization
//! decision and terminal-handle recovery are defined once here and named by
//! every engine. Nothing in this module is a composition-root concern.

pub use crate::cache_volume::PVC_BIND_REF_PREFIX;
pub use crate::exec_attempts::{exec_pid_file, exec_pid_file_prelude};
pub use crate::fresh_create_attempt;
pub use crate::live_inputs::manages_live_input;
pub use crate::process_env::environment_keepalive_command;
pub use crate::recovery::{source_mount_paths, terminal_handle};
pub use crate::restore_target::{
    RESTORE_PLAN_LABEL, restoration_evidence_from_metadata, restoration_metadata,
    restoration_plan_fingerprint,
};
pub use crate::runtime::{
    CONTAINER_HARDENING_REVISION, ExistingRealization, ExistingRealizationDecision,
    ExistingRealizationPhase, ExistingRealizationRecovery, PhysicalIncarnation,
    RUNTIME_OWNER_LABEL, RebuildContinuityEvidence, SANDBOX_ATTEMPT_LABEL, SANDBOX_EXECUTION_ID,
    container_adoption_fingerprint, container_effect_fence_from_values,
    container_realization_fingerprint, existing_realization_decision, legacy_unfenced_fingerprint,
    runtime_owner_id, sandbox_observation, sandbox_observation_identity, sandbox_scope_identity,
    validate_runtime_effect_fence,
};
pub use crate::writable::{WritableRootTopology, writable_dirs, writable_root_topology};
