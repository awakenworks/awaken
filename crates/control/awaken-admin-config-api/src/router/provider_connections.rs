//! Provider-connection read projection.
//!
//! The write command remains beside the catalog transaction in the parent
//! router. This module owns the only status projection consumed by the setup UI,
//! avoiding a second frontend-derived connection state.

use awaken_credential_vault::CredentialStatus;
use awaken_model_catalog::{CatalogSyncResult, OfferingStatus, ProtocolEndpoint, Provider};
use awaken_tenancy::WorkspaceScope as ResourceWorkspace;
use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::HeaderMap;

use super::{
    AdminState, CredentialSourceView, Problem, cred_problem, repo_problem, req_id, unix_time_ms,
};

#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderConnectionView {
    pub provider: Provider,
    pub endpoint: ProtocolEndpoint,
    pub credential: CredentialSourceView,
    pub sync: CatalogSyncResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectionStatus {
    NotConfigured,
    Connected,
    Ready,
    Stale,
    NeedsAttention,
    Unavailable,
}

#[derive(serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProviderConnectionSummary {
    pub provider_id: String,
    pub display_name: String,
    pub status: ProviderConnectionStatus,
    pub endpoint_ids: Vec<String>,
    pub active_credentials: usize,
    pub active_models: usize,
    pub unavailable_models: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at_unix_ms: Option<u64>,
}

#[derive(serde::Deserialize)]
pub(super) struct ListProviderConnectionsQuery {
    workspace_id: String,
}

const PROVIDER_CATALOG_STALE_AFTER_MS: u64 = 24 * 60 * 60 * 1_000;

pub(super) async fn list_provider_connections(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Query(query): Query<ListProviderConnectionsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProviderConnectionSummary>>, Problem> {
    let rid = req_id(&headers);
    let workspace_id = scope.map_or(query.workspace_id, |Extension(scope)| scope.0);
    let catalog = state
        .catalog
        .snapshot()
        .await
        .map_err(|error| repo_problem(&error, &rid))?;
    let credentials = state
        .credentials
        .list(&workspace_id)
        .await
        .map_err(|error| cred_problem(&error, &rid))?;
    let now = unix_time_ms();
    let summaries = awaken_model_catalog::provider_driver_descriptors()
        .into_iter()
        .map(|descriptor| {
            let provider_id = descriptor.provider_kind;
            let provider = catalog.providers.get(&provider_id);
            let mut endpoint_ids = catalog
                .endpoints
                .values()
                .filter(|endpoint| endpoint.provider_id.as_str() == provider_id)
                .map(|endpoint| endpoint.id.0.clone())
                .collect::<Vec<_>>();
            endpoint_ids.sort();
            let provider_credentials = credentials
                .iter()
                .filter(|credential| {
                    credential.provider_id.as_deref() == Some(provider_id.as_str())
                })
                .collect::<Vec<_>>();
            let active_credentials = provider_credentials
                .iter()
                .filter(|credential| credential.status == CredentialStatus::Active)
                .count();
            let offerings = catalog
                .offerings
                .iter()
                .filter(|offering| offering.provider_id.as_str() == provider_id)
                .collect::<Vec<_>>();
            let active_models = offerings
                .iter()
                .filter(|offering| offering.status == OfferingStatus::Active)
                .count();
            let unavailable_models = offerings.len().saturating_sub(active_models);
            let last_seen_at_unix_ms = offerings
                .iter()
                .filter_map(|offering| offering.last_seen_at_unix_ms)
                .max();
            let active_last_seen_at_unix_ms = offerings
                .iter()
                .filter(|offering| offering.status == OfferingStatus::Active)
                .filter_map(|offering| offering.last_seen_at_unix_ms)
                .max();
            let status = if provider.is_none() && provider_credentials.is_empty() {
                ProviderConnectionStatus::NotConfigured
            } else if active_credentials == 0 {
                ProviderConnectionStatus::NeedsAttention
            } else if offerings.is_empty() {
                ProviderConnectionStatus::Connected
            } else if active_models == 0 {
                ProviderConnectionStatus::Unavailable
            } else if active_last_seen_at_unix_ms
                .is_some_and(|seen| now.saturating_sub(seen) > PROVIDER_CATALOG_STALE_AFTER_MS)
            {
                ProviderConnectionStatus::Stale
            } else {
                ProviderConnectionStatus::Ready
            };
            ProviderConnectionSummary {
                provider_id,
                display_name: provider.map_or(descriptor.display_name, |provider| {
                    provider.display_name.clone()
                }),
                status,
                endpoint_ids,
                active_credentials,
                active_models,
                unavailable_models,
                last_seen_at_unix_ms,
            }
        })
        .collect();
    Ok(Json(summaries))
}
