//! Admin config API (ADR-0043) — the self-hosted-only management surface the
//! Anthropic wire does not define: CRUD over provider / protocol-endpoint /
//! offering and credential entry. Built on `awaken-api-contract` (`ApiError`
//! problem details / pagination / allowlisted queries) and generates the TS
//! contract (P2).
//!
//! The domain types it authors live in `awaken-model-catalog` / `awaken-credential-vault`;
//! this crate is the HTTP adapter over their repos, storing nothing itself.

#![forbid(unsafe_code)]

#[cfg(feature = "schema")]
pub mod openapi;
#[cfg(feature = "postgres")]
pub mod postgres;
mod router;
pub mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(feature = "sqlite")]
mod sqlite_resource_catalog;

#[cfg(feature = "postgres")]
pub use postgres::PostgresAdminStore;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteAdminStore;

// The read ports + in-memory impls now live in the open resolver crate; re-export
// them so existing `awaken_admin_config_api::…Store` paths keep resolving (same
// type). The authoring HTTP surface writes through these ports; the SQLite backend
// (`SqliteAdminStore`) implements them.
pub use awaken_config_resolver::{
    InMemoryMcpStore, InMemoryMemoryStoreRegistry, InMemoryProfileStore, InMemoryResourceStore,
    InMemoryWebhookStore, InferenceProfileStore, McpStore, MemoryStoreDef, MemoryStoreRegistry,
    ResourceStore, WebhookStore,
};
pub use router::{
    AdminState, CooldownRequest, CredentialProbe, CredentialValidation, EnterCredentialRequest,
    PoolEligibleView, ProbeStatus, ResolveAgentMcpRequest, ResolveProfileRequest, ResolveRequest,
    ResolvedCandidatesView, ResolvedInferenceView, ResolvedMcpServerView,
    ValidateCredentialRequest, admin_router,
};

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
