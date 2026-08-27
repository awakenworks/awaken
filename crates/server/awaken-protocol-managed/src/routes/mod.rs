//! The routing concern: the axum routers and handlers for every Managed Agents
//! surface. Handlers only decode DTOs (from `types`), call the backing state, and
//! encode responses — no runtime or protocol logic.
//!
//! One submodule per surface. Each handler decodes wire DTOs and delegates to the
//! injected application owner; protocol modules do not own business stores:
//! - [`sessions`] — the core session/events/threads/resources surface, plus the
//!   shared HTTP plumbing ([`ManagedJson`], [`error_response`]) the others reuse.
//! - [`agents_registry`], [`deployments`], [`environments`], [`user_profiles`],
//!   [`vaults`] — the management-plane resource CRUD surfaces.

use sha2::{Digest, Sha256};

pub mod agents_registry;
pub mod credential_rollouts;
pub mod deployments;
pub mod dreams;
pub mod environments;
pub mod sessions;
pub mod tunnels;
pub mod user_profiles;
pub mod vaults;

// The session surface defines the shared error-envelope conventions; re-export the
// plumbing so sibling resource routers answer bad bodies and domain errors alike.
pub(crate) use awaken_tenancy::WorkspaceScope;
pub use dreams::dreams_router;
pub(crate) use sessions::ManagedJson;
pub use sessions::{
    create_profiled_session, enforce_managed_beta, release_profiled_session,
    replace_resource_manifest, router,
};
pub use tunnels::tunnels_router;

/// Domain-separated opaque identity used when a wire secret must select a
/// durable owner without persisting or logging the secret itself.
fn sha256_identity(domain: &str, parts: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.len().to_be_bytes());
    digest.update(domain.as_bytes());
    for part in parts {
        digest.update(part.len().to_be_bytes());
        digest.update(part.as_bytes());
    }
    format!("{:x}", digest.finalize())
}
