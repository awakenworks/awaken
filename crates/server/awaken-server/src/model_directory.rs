//! Workspace-aware Managed `/v1/models` projection.

use std::sync::Arc;

use awaken_credential_vault::repo::CredentialRepo;
use awaken_managed_routers::{ModelDirectory, ModelDirectoryFuture, ModelEntry};
use awaken_model_catalog::{Offering, OfferingStatus, ProviderCatalog, repo::CatalogRepo};

pub struct CatalogModelDirectory {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    executors: Arc<dyn ExecutorModelCapabilitySource>,
}

#[async_trait::async_trait]
pub trait ExecutorModelCapabilitySource: Send + Sync {
    async fn current(&self) -> Vec<awaken_config_resolver::ExecutorModelCapability>;
}

struct StaticExecutorModelCapabilities(Vec<awaken_config_resolver::ExecutorModelCapability>);

#[async_trait::async_trait]
impl ExecutorModelCapabilitySource for StaticExecutorModelCapabilities {
    async fn current(&self) -> Vec<awaken_config_resolver::ExecutorModelCapability> {
        self.0.clone()
    }
}

impl CatalogModelDirectory {
    #[must_use]
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        credentials: Arc<dyn CredentialRepo>,
        executors: Arc<Vec<awaken_config_resolver::ExecutorModelCapability>>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            executors: Arc::new(StaticExecutorModelCapabilities(executors.as_ref().clone())),
        }
    }

    #[must_use]
    pub fn with_source(
        catalog: Arc<dyn CatalogRepo>,
        credentials: Arc<dyn CredentialRepo>,
        executors: Arc<dyn ExecutorModelCapabilitySource>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            executors,
        }
    }
}

/// The sole built-in executor-to-model capability projection. Runtime
/// diagnostics, model discovery, and publication consume this data rather than
/// rebuilding CLI-specific dialect tables.
#[must_use]
pub fn installed_executor_model_capabilities()
-> Vec<awaken_config_resolver::ExecutorModelCapability> {
    std::iter::once(awaken_config_resolver::ExecutorModelCapability::native())
        .chain(
            awaken_run_executor_acp::known_acp_clis()
                .iter()
                .map(|executor| awaken_config_resolver::ExecutorModelCapability {
                    backend_ref: format!("acp:{}", executor.id),
                    model_api_dialects: executor
                        .model_api_dialects
                        .iter()
                        .map(|dialect| (*dialect).to_string())
                        .collect(),
                    available: true,
                }),
        )
        .collect()
}

fn endpoint_qualifier(offering: &Offering, executable: &[&Offering]) -> String {
    let endpoint_id = offering.protocol_endpoint_id.as_str();
    let provider_prefix = format!("{}.", offering.provider_id);
    let relative = endpoint_id
        .strip_prefix(&provider_prefix)
        .unwrap_or(endpoint_id);
    let named_prefix = format!("{}.", offering.dialect.as_str());
    let short = relative.strip_prefix(&named_prefix).unwrap_or(relative);
    let short_collides = executable.iter().any(|candidate| {
        *candidate != offering
            && candidate.model_id == offering.model_id
            && candidate.provider_id == offering.provider_id
            && {
                let candidate_endpoint = candidate.protocol_endpoint_id.as_str();
                let candidate_relative = candidate_endpoint
                    .strip_prefix(&provider_prefix)
                    .unwrap_or(candidate_endpoint);
                let candidate_named_prefix = format!("{}.", candidate.dialect.as_str());
                candidate_relative
                    .strip_prefix(&candidate_named_prefix)
                    .unwrap_or(candidate_relative)
                    == short
            }
    });
    if short_collides {
        relative.to_string()
    } else {
        short.to_string()
    }
}

fn route_id(offering: &Offering, executable: &[&Offering]) -> String {
    let same_model = executable
        .iter()
        .filter(|candidate| candidate.model_id == offering.model_id)
        .count();
    if same_model == 1 && !offering.model_id.contains('/') {
        return offering.model_id.clone();
    }
    let same_provider_model = executable
        .iter()
        .filter(|candidate| {
            candidate.model_id == offering.model_id && candidate.provider_id == offering.provider_id
        })
        .count();
    if same_provider_model == 1 {
        return format!("{}/{}", offering.provider_id, offering.model_id);
    }
    let endpoint_name = endpoint_qualifier(offering, executable);
    format!(
        "{}@{}/{}",
        offering.provider_id, endpoint_name, offering.model_id
    )
}

fn provider_route_id(offering: &Offering, executable: &[&Offering]) -> String {
    let same_provider_model = executable
        .iter()
        .filter(|candidate| {
            candidate.model_id == offering.model_id && candidate.provider_id == offering.provider_id
        })
        .count();
    if same_provider_model == 1 {
        return offering.provider_id.to_string();
    }
    let endpoint_name = endpoint_qualifier(offering, executable);
    format!("{}@{endpoint_name}", offering.provider_id)
}

impl ModelDirectory for CatalogModelDirectory {
    fn list<'a>(&'a self, workspace_id: &'a str) -> ModelDirectoryFuture<'a> {
        Box::pin(async move {
            let catalog = self
                .catalog
                .snapshot()
                .await
                .map_err(|error| error.to_string())?;
            let credentials = self
                .credentials
                .list(workspace_id)
                .await
                .map_err(|error| error.to_string())?;
            let executors = self.executors.current().await;
            let routes = awaken_config_resolver::project_executable_models(
                &catalog,
                &credentials,
                &executors,
            );
            let executable = catalog
                .offerings
                .iter()
                .filter(|offering| offering.status == OfferingStatus::Active)
                .collect::<Vec<_>>();

            let mut entries = Vec::new();
            for route in routes.into_iter().filter(|route| {
                route.readiness == awaken_config_resolver::ExecutableModelReadiness::Ready
            }) {
                let Some(offering) = executable.iter().copied().find(|offering| {
                    offering.provider_id.as_str() == route.provider_id
                        && offering.protocol_endpoint_id.as_str() == route.endpoint_id
                        && offering.model_id == route.model_id
                }) else {
                    continue;
                };
                if route.backend_ref == "genai" {
                    let native_id = route_id(offering, &executable);
                    entries.push(entry(&catalog, offering, native_id));
                } else if let Some(cli) = route.backend_ref.strip_prefix("acp:") {
                    let provider_route = provider_route_id(offering, &executable);
                    entries.push(entry(
                        &catalog,
                        offering,
                        format!("acp:{cli}@{provider_route}/{}", offering.model_id),
                    ));
                }
            }
            entries.sort_by(|left, right| left.id.cmp(&right.id));
            entries.dedup_by(|left, right| left.id == right.id);
            Ok(entries)
        })
    }
}

fn entry(catalog: &ProviderCatalog, offering: &Offering, id: String) -> ModelEntry {
    let mut entry = ModelEntry::new(&id, &offering.model_id);
    if let (Some(context), Some(output)) = (
        catalog.context_window(&offering.model_id),
        catalog.max_output_tokens(&offering.model_id),
    ) {
        entry = entry.with_limits(context, output);
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, OfferingSource, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use http_body_util::BodyExt as _;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tower::ServiceExt as _;

    fn offering_with_dialect(
        provider: &str,
        endpoint: &str,
        model: &str,
        dialect: ApiDialect,
    ) -> Offering {
        Offering {
            model_id: model.into(),
            provider_id: ProviderId::new(provider),
            protocol_endpoint_id: ProtocolEndpointId::new(endpoint),
            dialect,
            upstream_model: None,
            source: OfferingSource::Manual,
            status: OfferingStatus::Active,
            last_seen_at_unix_ms: None,
        }
    }

    fn offering(provider: &str, endpoint: &str, model: &str) -> Offering {
        offering_with_dialect(provider, endpoint, model, ApiDialect::OpenAiChat)
    }

    #[test]
    fn canonical_directory_ids_add_only_the_qualifier_needed_for_uniqueness() {
        // Causes: C1 model unique globally; C2 repeated across providers; C3
        // repeated within one provider across endpoints; C4 endpoint short names
        // collide across dialects. Effects: E1 bare id; E2 provider/model; E3
        // provider@endpoint/model using dialect/default or endpoint name; E4 a
        // colliding name is dialect-qualified. Decision rules exercise all four
        // levels and the ACP provider-route form.
        let unique = offering("openai", "openai.open_ai_chat", "unique");
        let namespaced = offering("anyrouter", "anyrouter.open_ai_chat", "qwen/qwen3");
        let any_primary = offering(
            "anyrouter",
            "anyrouter.open_ai_chat.primary",
            "shared/model",
        );
        let any_backup = offering("anyrouter", "anyrouter.open_ai_chat.backup", "shared/model");
        let qwen = offering("qwen", "qwen.open_ai_chat", "shared/model");
        let any_openai_primary =
            offering("collision", "collision.open_ai_chat.primary", "same/model");
        let any_anthropic_primary = offering_with_dialect(
            "collision",
            "collision.anthropic_messages.primary",
            "same/model",
            ApiDialect::AnthropicMessages,
        );
        let all = vec![
            &unique,
            &namespaced,
            &any_primary,
            &any_backup,
            &qwen,
            &any_openai_primary,
            &any_anthropic_primary,
        ];
        assert_eq!(route_id(&unique, &all), "unique", "E1");
        assert_eq!(
            route_id(&namespaced, &all),
            "anyrouter/qwen/qwen3",
            "slash-bearing model ids require an explicit provider route"
        );
        assert_eq!(route_id(&qwen, &all), "qwen/shared/model", "E2");
        assert_eq!(
            route_id(&any_primary, &all),
            "anyrouter@primary/shared/model",
            "E3"
        );
        assert_eq!(provider_route_id(&qwen, &all), "qwen", "ACP E2");
        assert_eq!(
            provider_route_id(&any_backup, &all),
            "anyrouter@backup",
            "ACP E3"
        );
        assert_eq!(
            route_id(&any_openai_primary, &all),
            "collision@open_ai_chat.primary/same/model",
            "E4"
        );
        assert_eq!(
            route_id(&any_anthropic_primary, &all),
            "collision@anthropic_messages.primary/same/model",
            "E4"
        );
    }

    #[tokio::test]
    async fn retrieve_accepts_the_complete_slash_bearing_managed_model_id() {
        // Causes: C1 provider/model ids contain route separators; C2 retrieve
        // addresses the exact id. Effects: E1 the catch-all path preserves the
        // complete id; E2 the directory resolves it. A one-segment path is
        // already covered by the same route and lookup function.
        let id = "anyrouter/qwen/qwen3-235b";
        let app =
            awaken_managed_routers::models_router(Arc::new(vec![ModelEntry::new(id, "Qwen 3")]));
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/models/{id}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK, "E1/E2");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["id"], id);
    }

    struct ChangingExecutors(AtomicBool);

    #[async_trait::async_trait]
    impl ExecutorModelCapabilitySource for ChangingExecutors {
        async fn current(&self) -> Vec<awaken_config_resolver::ExecutorModelCapability> {
            let acp_available = self.0.swap(true, Ordering::SeqCst);
            vec![
                awaken_config_resolver::ExecutorModelCapability::native(),
                awaken_config_resolver::ExecutorModelCapability {
                    backend_ref: "acp:codex".into(),
                    model_api_dialects: vec!["open_ai_chat".into()],
                    available: acp_available,
                },
            ]
        }
    }

    #[tokio::test]
    async fn model_directory_rechecks_live_executor_readiness_on_every_list() {
        // Causes: C1 catalog Offering and compatible credential are stable; C2
        // native executor is available; C3 ACP capability is unavailable then
        // Verified/available. Effects: E1 native id is always visible; E2 ACP id
        // is absent on the first list and appears on the second. The directory
        // stores no parallel readiness snapshot.
        //
        // | Rule | C1 | C2 | C3 | visible ids |
        // | L1   | Y  | Y  | N  | native      |
        // | L2   | Y  | Y  | Y  | native+ACP  |
        let catalog = Arc::new(InMemoryCatalogRepo::new());
        catalog
            .put_provider(Provider {
                id: ProviderId::new("openai"),
                slug: "openai".into(),
                display_name: "OpenAI".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("openai.open_ai_chat"),
                provider_id: ProviderId::new("openai"),
                dialect: ApiDialect::OpenAiChat,
                base_url: Some("https://openai.example/v1".into()),
                timeout_secs: 30,
                display_name: "OpenAI".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_offering(offering("openai", "openai.open_ai_chat", "gpt-5"))
            .await
            .unwrap();
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("test-secret")),
                oauth_command: None,
            },
            &InMemorySecretStore::new(),
            credentials.as_ref(),
        )
        .await
        .unwrap();
        let directory = CatalogModelDirectory::with_source(
            catalog,
            credentials,
            Arc::new(ChangingExecutors(AtomicBool::new(false))),
        );
        let first = directory.list("workspace-a").await.unwrap();
        assert_eq!(
            first
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["gpt-5"],
            "L1"
        );
        let second = directory.list("workspace-a").await.unwrap();
        assert_eq!(
            second
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["acp:codex@openai/gpt-5", "gpt-5"],
            "L2"
        );
    }
}
