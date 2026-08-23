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
    AdminState, BrokeredCatalogDiscovery, CloudLoginApplication, CloudLoginState,
    CloudLoginStatusView, ConfigCapabilitiesSource, ConfigCapabilitiesView, CooldownRequest,
    CredentialProbe, CredentialSourceView, CredentialValidation, EnterCredentialRequest,
    IdentityCapabilityView, ModelSupplyCapabilityView, PoolEligibleView, ProbeStatus,
    ProductSurfaceCapabilityView, ProviderConnectionStatus, ProviderConnectionSummary,
    ProviderConnectionView, PutModelAttributesRequest, ResolveProfileRequest, ResolveRequest,
    ResolvedCandidatesView, ResolvedInferenceView, RotateCredentialRequest,
    SaveProviderConnectionRequest, ValidateCredentialRequest, admin_router,
    admin_router_with_capabilities, admin_router_with_runtime_capabilities,
    reconcile_brokered_catalog,
};

/// The API surface version this crate serves.
pub const API_VERSION: &str = "0";
