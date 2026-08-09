//! Provider-connection read projection.
//!
//! The reusable application service owns the write command. This module owns
//! the only status/readiness projections consumed by setup and selection UIs,
//! avoiding frontend-derived connection and executable-model state.

#[cfg(test)]
use awaken_config_resolver::ExecutableModelReadiness;
use awaken_config_resolver::{ExecutableModelOption, project_executable_models};
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
    let descriptors = awaken_model_catalog::provider_driver_descriptors()
        .into_iter()
        .map(|descriptor| (descriptor.provider_kind.clone(), descriptor))
        .collect::<std::collections::BTreeMap<_, _>>();
    let provider_ids = descriptors
        .keys()
        .cloned()
        .chain(catalog.providers.keys().cloned())
        .chain(
            credentials
                .iter()
                .filter_map(|source| source.provider_id.clone()),
        )
        .collect::<std::collections::BTreeSet<_>>();
    let summaries = provider_ids
        .into_iter()
        .map(|provider_id| {
            let descriptor = descriptors.get(&provider_id);
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
            let display_name = provider.map_or_else(
                || {
                    descriptor.map_or_else(
                        || provider_id.clone(),
                        |descriptor| descriptor.display_name.clone(),
                    )
                },
                |provider| provider.display_name.clone(),
            );
            ProviderConnectionSummary {
                provider_id,
                display_name,
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

pub(super) async fn list_executable_models(
    State(state): State<AdminState>,
    scope: Option<Extension<ResourceWorkspace>>,
    Query(query): Query<ListProviderConnectionsQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<ExecutableModelOption>>, Problem> {
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
    Ok(Json(project_executable_models(
        &catalog,
        &credentials,
        &[awaken_config_resolver::ExecutorModelCapability::native()],
    )))
}

#[cfg(test)]
mod tests {
    use awaken_credential_contract::CredentialSourceId;
    use awaken_credential_vault::{CredentialKind, CredentialSource};
    use awaken_model_catalog::{
        ApiDialect, Offering, OfferingSource, ProtocolEndpointId, ProviderCatalog, ProviderId,
    };

    use super::*;

    fn offering(provider: &str, model: &str, source: OfferingSource) -> Offering {
        Offering {
            model_id: model.into(),
            provider_id: ProviderId::new(provider),
            protocol_endpoint_id: ProtocolEndpointId::new(format!("{provider}-endpoint")),
            dialect: ApiDialect::OpenAiChat,
            upstream_model: None,
            source,
            status: OfferingStatus::Active,
            last_seen_at_unix_ms: None,
        }
    }

    fn credential(provider: &str) -> CredentialSource {
        CredentialSource {
            id: CredentialSourceId(format!("cred:workspace:{provider}")),
            workspace_id: "workspace".into(),
            kind: CredentialKind::Vault,
            provider_id: Some(provider.into()),
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(awaken_credential_vault::SecretRef(format!(
                "sec:{provider}"
            ))),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        }
    }

    #[test]
    fn executable_model_projection_owns_the_catalog_credential_join() {
        // Cause/effect decision table:
        // R1 active BYOK + compatible active credential -> ready;
        // R2 active BYOK + only another provider's credential -> credential_unavailable;
        // R3 inactive offering -> offering_unavailable regardless of credential;
        // R4 active brokered offering without an explicit brokered Profile ->
        // credential_unavailable on this ordinary Managed-model path.
        let mut unavailable = offering("anthropic", "retired", OfferingSource::Manual);
        unavailable.status = OfferingStatus::Unavailable;
        let catalog = ProviderCatalog {
            offerings: vec![
                offering("anthropic", "claude", OfferingSource::Manual),
                offering("openai", "gpt", OfferingSource::Manual),
                unavailable,
                offering("cloud", "managed", OfferingSource::Brokered),
            ],
            ..ProviderCatalog::default()
        };
        let options = project_executable_models(
            &catalog,
            &[credential("anthropic")],
            &[awaken_config_resolver::ExecutorModelCapability::native()],
        );
        let readiness = options
            .into_iter()
            .map(|option| (option.model_id, option.readiness))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(readiness["claude"], ExecutableModelReadiness::Ready);
        assert_eq!(
            readiness["gpt"],
            ExecutableModelReadiness::CredentialUnavailable
        );
        assert_eq!(
            readiness["retired"],
            ExecutableModelReadiness::OfferingUnavailable
        );
        assert_eq!(
            readiness["managed"],
            ExecutableModelReadiness::CredentialUnavailable
        );
    }
}
