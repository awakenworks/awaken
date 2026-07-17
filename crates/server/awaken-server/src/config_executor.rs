//! The config-plane-backed model executor provider: resolves a session's
//! `model_ref` to a real executor from the authored catalog + the workspace's
//! credential, so the console configures models through the API and the runtime
//! makes real calls — never an `AWAKEN_MODEL_SOURCE` env shortcut.
//!
//! Resolution chain (our single-tenant scenario, collapsed):
//!   `model_ref → offering(provider) → the workspace's first Active credential that
//!    `can_consume` that provider → resolve_inference → executor_from_resolved`.
//!
//! `ExecutorProvider::executor_for` is sync but the stores are async; it bridges
//! via `block_in_place` + the ambient runtime handle (the same pattern the
//! management stores use). Returning `None` falls back to the host's default
//! executor, so an unconfigured/unresolvable model still runs on the built-in
//! scenario model rather than erroring.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_config_resolver::{can_consume, resolve_inference};
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialBinding, CredentialSource, CredentialStatus, SecretStore};
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::llm::LlmExecutor;
use awaken_runtime_host::ExecutorProvider;

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
    async fn resolve(&self, model_ref: &str) -> Option<Arc<dyn LlmExecutor>> {
        let catalog = self.catalog.snapshot().await.ok()?;
        // The offering names the provider whose credential must authenticate the model.
        let provider_id = catalog
            .offerings
            .iter()
            .find(|o| o.model_id == model_ref)?
            .provider_id
            .0
            .clone();
        // Per-provider default derive: the workspace's first Active credential that
        // `can_consume` this provider (the "one default per provider" rule).
        let sources = self.credentials.list(&self.workspace_id).await.ok()?;
        let chosen = sources
            .iter()
            .find(|s| s.status == CredentialStatus::Active && can_consume(&provider_id, s))?;
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
            tokio::runtime::Handle::current().block_on(self.resolve(model_ref))
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
            p.resolve("claude-x").await.is_some(),
            "a configured model with an active, compatible credential resolves to an executor"
        );
    }

    #[tokio::test]
    async fn an_unconfigured_model_falls_back_to_the_host_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            p.resolve("no-such-model").await.is_none(),
            "no matching offering → None (the host default runs it)"
        );
    }

    #[tokio::test]
    async fn no_credential_falls_back_to_the_host_default() {
        let p = provider("claude-x", None).await;
        assert!(
            p.resolve("claude-x").await.is_none(),
            "an offering with no workspace credential → None"
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_falls_back() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            p.resolve("claude-x").await.is_none(),
            "only an Active credential is derived; a disabled one is skipped"
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_falls_back() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            p.resolve("claude-x").await.is_none(),
            "the credential must `can_consume` the offering's provider"
        );
    }
}
