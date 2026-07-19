//! The config-plane-backed model executor provider: resolves a session's
//! `model_ref` to a real executor from the authored catalog + the workspace's
//! credential, so the console configures models through the API and the runtime
//! makes real calls — never an `AWAKEN_MODEL_SOURCE` env shortcut.
//!
//! Resolution chain (our single-tenant scenario, collapsed):
//!   `model_ref → offering(provider) → the workspace's first Active credential that
//!    `can_consume` that provider → resolve_inference → executor_from_resolved`.
//!
//! `ExecutorProvider` is sync but the stores are async; it bridges via
//! `block_in_place` + the ambient runtime handle. Durable admission pins the
//! non-secret provider/endpoint/credential ids, and execution fails closed if
//! any of those facts changed or the credential was disabled.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_config_resolver::{can_consume, resolve_inference};
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialBinding, CredentialSource, CredentialStatus, SecretStore};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::{ExecutorProvider, ModelAccessRef};

use crate::executor_from_resolved;

/// Resolves `model_ref → executor` from the live config plane.
pub struct ConfigExecutorProvider {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    /// Single-tenant default (Option A): the workspace whose credentials back runs.
    workspace_id: String,
}

impl ConfigExecutorProvider {
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            secrets,
            workspace_id: workspace_id.into(),
        }
    }

    /// Resolve an executor from configured state, or `None` to fall back to the
    /// host default (no offering, no compatible credential, or a resolve error).
    async fn pin_access(&self, model_ref: &str) -> Result<ModelAccessRef, String> {
        let catalog = self
            .catalog
            .snapshot()
            .await
            .map_err(|error| error.to_string())?;
        let offering = catalog
            .offerings
            .iter()
            .find(|offering| offering.model_id == model_ref)
            .ok_or_else(|| format!("model offering {model_ref} is not published"))?;
        let provider = catalog
            .providers
            .get(offering.provider_id.as_str())
            .ok_or_else(|| "offering provider is missing".to_string())?;
        let endpoint = catalog
            .endpoints
            .get(offering.protocol_endpoint_id.as_str())
            .ok_or_else(|| "offering endpoint is missing".to_string())?;
        let sources = self
            .credentials
            .list(&self.workspace_id)
            .await
            .map_err(|error| error.to_string())?;
        let chosen = sources
            .iter()
            .find(|source| {
                source.status == CredentialStatus::Active
                    && can_consume(&offering.provider_id.0, source)
            })
            .ok_or_else(|| format!("no active credential can consume model {model_ref}"))?;
        Ok(ModelAccessRef::exact_credential(
            chosen.id.0.clone(),
            format!("{}@{}", offering.provider_id.0, provider.version),
            format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version),
        ))
    }

    async fn resolve(
        &self,
        model_ref: &str,
        access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let catalog = self.catalog.snapshot().await.ok()?;
        // The offering names the provider whose credential must authenticate the model.
        let offering = catalog.offerings.iter().find(|o| o.model_id == model_ref)?;
        let provider_id = offering.provider_id.0.clone();
        let provider_version = catalog.providers.get(&provider_id)?.version;
        let endpoint_version = catalog
            .endpoints
            .get(offering.protocol_endpoint_id.as_str())?
            .version;
        let pinned_provider = format!("{provider_id}@{provider_version}");
        let pinned_route = format!("{}@{endpoint_version}", offering.protocol_endpoint_id.0);
        // Per-provider default derive: the workspace's first Active credential that
        // `can_consume` this provider (the "one default per provider" rule).
        let sources = self.credentials.list(&self.workspace_id).await.ok()?;
        let chosen = match access {
            Some(access)
                if access.scheme == "credential-source/v1"
                    && access.provider_ref.as_deref() == Some(pinned_provider.as_str())
                    && access.route_ref.as_deref() == Some(pinned_route.as_str()) =>
            {
                sources.iter().find(|source| {
                    source.id.0 == access.reference
                        && source.status == CredentialStatus::Active
                        && can_consume(&provider_id, source)
                })?
            }
            Some(_) => return None,
            None => sources.iter().find(|source| {
                source.status == CredentialStatus::Active && can_consume(&provider_id, source)
            })?,
        };
        let binding = CredentialBinding::Exact {
            credential_source_id: chosen.id.clone(),
        };
        let lookup: HashMap<String, CredentialSource> = sources
            .iter()
            .map(|s| (s.id.0.clone(), s.clone()))
            .collect();
        let inference = resolve_inference(
            &catalog,
            model_ref,
            &binding,
            &lookup,
            self.secrets.as_ref(),
        )
        .await
        .ok()?;
        executor_from_resolved(&inference).ok()
    }
}

impl ExecutorProvider for ConfigExecutorProvider {
    fn executor_for(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        // Bridge the sync port to the async stores on the ambient multi-thread
        // runtime (mirrors the management stores' block_in_place bridge).
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.resolve(model_ref, None))
        })
    }

    fn model_access_for(&self, model_ref: &str) -> Result<Option<ModelAccessRef>, String> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.pin_access(model_ref))
                .map(Some)
        })
    }

    fn executor_for_run(
        &self,
        model_ref: &str,
        model_access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.resolve(model_ref, model_access))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_model_catalog::repo::InMemoryCatalogRepo;
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    /// Author a catalog with one anthropic offering for `model`, and optionally a
    /// workspace credential `(provider, active)`. The secret is a fake — resolution
    /// and executor construction never call the network, so every branch is
    /// reachable offline.
    async fn provider(model: &str, credential: Option<(&str, bool)>) -> ConfigExecutorProvider {
        let catalog = Arc::new(InMemoryCatalogRepo::new());
        catalog
            .put_provider(Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("https://api.anthropic.com/v1/".into()),
                timeout_secs: 300,
                display_name: "prod".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_offering(Offering {
                model_id: model.into(),
                provider_id: ProviderId::new("anthropic"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
                dialect: ApiDialect::AnthropicMessages,
                upstream_model: None,
            })
            .await
            .unwrap();

        let secrets = Arc::new(InMemorySecretStore::new());
        let creds = Arc::new(InMemoryCredentialRepo::new());
        if let Some((prov, active)) = credential {
            let source = enter_credential(
                CredentialCreateParams {
                    workspace_id: "ws".into(),
                    kind: CredentialKind::Vault,
                    provider_id: Some(prov.into()),
                    env_key: Some("ANTHROPIC_API_KEY".into()),
                    secret: Some(RedactedString::new("sk-test-fake")),
                    oauth_command: None,
                },
                secrets.as_ref(),
                creds.as_ref(),
            )
            .await
            .unwrap();
            if !active {
                let mut row = creds.get(&source.id).await.unwrap();
                row.status = CredentialStatus::Disabled;
                creds.put(row).await.unwrap();
            }
        }
        ConfigExecutorProvider::new(catalog, creds, secrets, "ws")
    }

    #[tokio::test]
    async fn resolves_a_configured_model_to_an_executor() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            p.resolve("claude-x", None).await.is_some(),
            "a configured model with an active, compatible credential resolves to an executor"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_model_falls_back_to_the_host_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            p.resolve("no-such-model", None).await.is_none(),
            "no matching offering → None (the host default runs it)"
        );
    }

    #[tokio::test]
    async fn no_credential_falls_back_to_the_host_default() {
        let p = provider("claude-x", None).await;
        assert!(
            p.resolve("claude-x", None).await.is_none(),
            "an offering with no workspace credential → None"
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_falls_back() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            p.resolve("claude-x", None).await.is_none(),
            "only an Active credential is derived; a disabled one is skipped"
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_falls_back() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            p.resolve("claude-x", None).await.is_none(),
            "the credential must `can_consume` the offering's provider"
        );
    }

    #[tokio::test]
    async fn pinned_credential_never_switches_to_a_new_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let pinned = p.pin_access("claude-x").await.unwrap();
        assert_eq!(pinned.scheme, "credential-source/v1");
        assert_eq!(pinned.provider_ref.as_deref(), Some("anthropic@1"));
        assert_eq!(pinned.route_ref.as_deref(), Some("ep1@1"));

        let second = enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY_2".into()),
                secret: Some(RedactedString::new("sk-test-second")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();
        assert_ne!(pinned.reference, second.id.0);
        assert!(p.resolve("claude-x", Some(&pinned)).await.is_some());

        let pinned_id = awaken_credential_vault::CredentialSourceId(pinned.reference.clone());
        let mut old = p.credentials.get(&pinned_id).await.unwrap();
        old.status = CredentialStatus::Disabled;
        p.credentials.put(old).await.unwrap();
        assert!(
            p.resolve("claude-x", Some(&pinned)).await.is_none(),
            "revoking the pinned credential fails closed instead of selecting the new default"
        );
    }

    #[tokio::test]
    async fn pinned_route_does_not_follow_a_catalog_update() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let pinned = p.pin_access("claude-x").await.unwrap();
        p.catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("https://new.example/v1/".into()),
                timeout_secs: 300,
                display_name: "new".into(),
                version: 2,
            })
            .await
            .unwrap();
        assert!(
            p.resolve("claude-x", Some(&pinned)).await.is_none(),
            "an admitted run cannot silently move to the updated route"
        );
    }
}
