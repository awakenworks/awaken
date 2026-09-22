//! Session-owned Resource resolution and realization-lease models.
//!
//! Resource Registry owns reusable resources. This crate owns only the frozen
//! Session binding and the lease that fences its Runtime realization.

mod realization_lease;
mod resource;

pub use awaken_agent_contract::{
    CanonicalJsonHashError, canonical_json_sha256, stable_fingerprint,
};
pub use awaken_provisioning_contract::{SandboxCheckpointRef, SandboxRestoreRequest};
pub use awaken_session_types::SessionId;
pub use realization_lease::{
    SessionRealizationLease, realization_lease_authorizes, realization_lease_generation_authorizes,
    realization_lease_is_live_at,
};
pub use resource::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
    ResolvedSkillBinding, SessionInputAttachment, SessionInputError, SessionInputResolver,
    SessionMutationPolicy, SessionResourceCompatibilityFacts, SessionResourceManifest,
    SessionResourceTransition, repository_transport_credential_target,
    repository_transport_credential_usage, repository_transport_material_binding,
};
