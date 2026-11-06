//! Session-owned Resource resolution and realization-lease models.
//!
//! Resource Registry owns reusable resources. This crate owns only the frozen
//! Session binding and the lease that fences its Runtime realization.

pub mod binding_checks;
mod inputs;
mod realization_lease;

pub use inputs::{
    ResolvedInput, ResolvedInputSource, ResolvedRepositoryCredential, ResolvedSessionResources,
    ResolvedSkillBinding, SessionInputAttachment, SessionInputError, SessionInputResolver,
    SessionMutationPolicy, SessionResourceCompatibilityFacts, SessionResourceManifest,
    SessionResourceTransition, repository_transport_credential_target,
    repository_transport_credential_usage, repository_transport_material_binding,
};
pub use realization_lease::{
    SessionRealizationLease, realization_lease_authorizes, realization_lease_generation_authorizes,
    realization_lease_is_live_at,
};
