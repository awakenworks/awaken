//! Admin config API (ADR-0043) — the self-hosted-only management surface the
//! Anthropic wire does not define: CRUD over provider / protocol-endpoint /
//! offering and credential entry. Built on `awaken-api-contract` (`ApiError`
//! problem details / pagination / allowlisted queries) and generates the TS
//! contract (P2).
//!
//! The domain types it authors live in `awaken-model-catalog` / `awaken-credential-vault`;
//! this crate is the HTTP adapter over their repos, storing nothing itself.

#![forbid(unsafe_code)]

mod router;
pub mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteAdminStore;

pub use router::{
    AdminState, CredentialProbe, CredentialValidation, InMemoryMcpStore, InMemoryProfileStore,
    InMemoryProjectStore, InMemoryResourceStore, InferenceProfileStore, McpStore, ProbeStatus,
    ProjectStore, ResolveRequest, ResolvedInferenceView, ResolvedMcpServerView, ResourceStore,
    admin_router,
};

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
