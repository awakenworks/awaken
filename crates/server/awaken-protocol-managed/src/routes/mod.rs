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

pub mod agents_registry;
pub mod deployments;
pub mod dreams;
pub mod environments;
pub mod sessions;
pub mod user_profiles;
pub mod vaults;

// The session surface defines the shared error-envelope conventions; re-export the
// plumbing so sibling resource routers answer bad bodies and domain errors alike.
pub(crate) use awaken_tenancy::WorkspaceScope;
pub use dreams::{DREAMING_BETA, dreams_router};
pub(crate) use sessions::ManagedJson;
pub use sessions::{
    MEMORY_BETA, SKILLS_BETA, enforce_managed_beta, replace_resource_manifest, router,
};
