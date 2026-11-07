//! Admin configuration HTTP adapters and SQL persistence (ADR-0043).
//! The self-hosted management surface covers provider connections, protocol
//! endpoints, offerings and credential entry, using `awaken-api-contract` for
//! problem details, pagination and allowlisted queries.
//!
//! Catalog and credential requests delegate to their existing domain repositories
//! in `awaken-model-catalog` and `awaken-credential-vault`. Their data and migration
//! bundles remain separate from this crate's secret-free admin persistence.
//!
//! The SQLite and PostgreSQL admin stores implement five existing ports:
//! `AgentInputBindingRepository`, `InferenceProfileStore`, `WebhookStore`,
//! `McpConfigurationRepository` and `WorkspaceTelemetryRepository`. Their schema
//! and migrations are owned by [`schema`]. Process startup selects one physical
//! admin store and shares its `Arc` through these five views; it does not open a
//! second store for MCP configuration or telemetry. Catalog persistence remains
//! owned by `awaken-model-catalog-store`; these adapters do not implement
//! `CatalogRepo`.

#![forbid(unsafe_code)]

#[cfg(feature = "schema")]
pub mod openapi;
#[cfg(feature = "postgres")]
pub mod postgres;
mod provider_connection;
mod router;
pub mod schema;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::PostgresAdminStore;
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteAdminStore;

pub use provider_connection::{
    ConnectProviderCommand, ModelCatalogDiscovery, ModelCatalogDiscoveryError,
    ProviderConnectionAuthentication, ProviderConnectionError, ProviderConnectionResult,
    ProviderConnectionService,
};
pub use router::{
    AdminState, ArchiveCredentialRequest, BrokeredCatalogDiscovery, CloudLoginApplication,
    CloudLoginState, CloudLoginStatusView, ConfigCapabilitiesSource, ConfigCapabilitiesView,
    CooldownRequest, CredentialGrantRenewal, CredentialProbe, CredentialRenewalObservation,
    CredentialSourceView, CredentialValidation, EnterCredentialRequest, IdentityCapabilityView,
    ModelSupplyAuthorization, ModelSupplyCapabilityView, PoolEligibleView, ProbeStatus,
    ProductSurfaceCapabilityView, ProviderConnectionStatus, ProviderConnectionSummary,
    ProviderConnectionView, PutModelAttributesRequest, RenewCredentialRequest,
    ResolveProfileRequest, ResolveRequest, ResolvedCandidatesView, ResolvedInferenceView,
    RotateCredentialRequest, SaveProviderConnectionRequest, ValidateCredentialRequest,
    admin_router, admin_router_with_capabilities, admin_router_with_runtime_capabilities,
    reconcile_brokered_catalog,
};

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
