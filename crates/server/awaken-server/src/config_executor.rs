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

use async_trait::async_trait;
use awaken_config_resolver::{can_consume, resolve_inference};
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{CredentialBinding, CredentialSource, CredentialStatus, SecretStore};
use awaken_model_catalog::ProviderCatalog;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_host::{ExecutorProvider, ModelAccessRef};

use crate::executor_from_resolved;

/// Resolves `model_ref → executor` from the live config plane.
#[derive(Clone)]
pub struct ConfigExecutorProvider {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    /// Single-tenant default (Option A): the workspace whose credentials back runs.
    workspace_id: String,
    fallback: Option<HostFallback>,
}

#[derive(Clone)]
struct HostFallback {
    model_ref: String,
    executor: Arc<dyn LlmExecutor>,
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
            fallback: None,
        }
    }

    /// Install the host's existing explicit fallback as a dispatch-pinnable
    /// executor. This shares the same `Arc` used by `SharedHost`; it is not a
    /// second resolver or an implicit global-model lookup.
    #[must_use]
    pub fn with_fallback_executor(
        mut self,
        model_ref: impl Into<String>,
        executor: Arc<dyn LlmExecutor>,
    ) -> Self {
        self.fallback = Some(HostFallback {
            model_ref: model_ref.into(),
            executor,
        });
        self
    }

    fn fallback_access(&self, model_ref: &str) -> Option<ModelAccessRef> {
        self.fallback
            .as_ref()
            .filter(|fallback| fallback.model_ref == model_ref)
            .map(|_| ModelAccessRef::host_executor(model_ref))
    }

    fn fallback_executor(
        &self,
        model_ref: &str,
        access: &ModelAccessRef,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.fallback
            .as_ref()
            .filter(|fallback| {
                fallback.model_ref == model_ref && access.is_host_executor_for(model_ref)
            })
            .map(|fallback| fallback.executor.clone())
    }

    /// Resolve an executor from configured state, or `None` to fall back to the
    /// host default (no offering, no compatible credential, or a resolve error).
    async fn pin_access(&self, model_ref: &str) -> Result<ModelAccessRef, String> {
        let catalog = self
            .catalog
            .snapshot()
            .await
            .map_err(|error| error.to_string())?;
        if catalog
            .offerings
            .iter()
            .any(|offering| offering.model_id == model_ref)
        {
            let sources = self
                .credentials
                .list(&self.workspace_id)
                .await
                .map_err(|error| error.to_string())?;
            Self::pin_access_from(&catalog, &sources, model_ref)
        } else {
            self.fallback_access(model_ref)
                .ok_or_else(|| format!("model offering {model_ref} is not published"))
        }
    }

    fn pin_access_from(
        catalog: &ProviderCatalog,
        sources: &[CredentialSource],
        model_ref: &str,
    ) -> Result<ModelAccessRef, String> {
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

    async fn pin_activation_access(
        &self,
        activation: &RunActivation,
    ) -> Result<ModelAccessRef, String> {
        let model_refs = match activation
            .model_ref_override
            .as_deref()
            .filter(|model_ref| !model_ref.is_empty())
        {
            Some(model_ref) => vec![model_ref.to_string()],
            None => activation
                .snapshot
                .resolved_spec
                .candidate_bindings()
                .into_iter()
                .map(|binding| binding.model_ref.clone())
                .collect(),
        };
        // One catalog and credential-inventory snapshot pins the entire set. A
        // concurrent route update cannot produce a mixed-generation candidate
        // bundle assembled from separate reads.
        let catalog = self
            .catalog
            .snapshot()
            .await
            .map_err(|error| error.to_string())?;
        let needs_credentials = model_refs.iter().any(|model_ref| {
            catalog
                .offerings
                .iter()
                .any(|offering| offering.model_id == *model_ref)
        });
        let sources = if needs_credentials {
            self.credentials
                .list(&self.workspace_id)
                .await
                .map_err(|error| error.to_string())?
        } else {
            Vec::new()
        };
        let mut pinned = Vec::new();
        let mut last_error = None;
        for model_ref in model_refs {
            let access = if catalog
                .offerings
                .iter()
                .any(|offering| offering.model_id == model_ref)
            {
                Self::pin_access_from(&catalog, &sources, &model_ref)
            } else {
                self.fallback_access(&model_ref)
                    .ok_or_else(|| format!("model offering {model_ref} is not published"))
            };
            match access {
                Ok(access) => pinned.push((model_ref, access)),
                Err(error) => last_error = Some(error),
            }
        }
        ModelAccessRef::candidate_set(pinned).ok_or_else(|| {
            last_error.unwrap_or_else(|| "run has no materializable model candidate".to_string())
        })
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

    async fn materialize(
        &self,
        model_ref: &str,
        access: &ModelAccessRef,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if let Some(executor) = self.fallback_executor(model_ref, access) {
            Some(executor)
        } else {
            self.resolve(model_ref, Some(access)).await
        }
    }
}

struct PinnedCandidateExecutor {
    provider: ConfigExecutorProvider,
    access: ModelAccessRef,
}

impl PinnedCandidateExecutor {
    async fn executor_for(&self, model_ref: &str) -> Result<Arc<dyn LlmExecutor>, LlmError> {
        let access = self.access.for_model(model_ref).ok_or_else(|| {
            LlmError::Binding(format!(
                "model {model_ref} is outside the dispatch-pinned candidate set"
            ))
        })?;
        self.provider
            .materialize(model_ref, &access)
            .await
            .ok_or_else(|| {
                LlmError::Binding(format!(
                    "dispatch-pinned model access is unavailable for {model_ref}"
                ))
            })
    }
}

#[async_trait]
impl LlmExecutor for PinnedCandidateExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let executor = self.executor_for(&request.model_binding.model_ref).await?;
        executor.infer(request).await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        let executor = self.executor_for(&request.model_binding.model_ref).await?;
        executor.infer_streaming(request, sink).await
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

    fn model_access_for_activation(
        &self,
        activation: &RunActivation,
    ) -> Result<Option<ModelAccessRef>, String> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.pin_activation_access(activation))
                .map(Some)
        })
    }

    fn executor_for_run(
        &self,
        model_ref: &str,
        model_access: Option<&ModelAccessRef>,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let model_access = model_access?;
        if !model_access.candidates.is_empty() {
            return Some(Arc::new(PinnedCandidateExecutor {
                provider: self.clone(),
                access: model_access.clone(),
            }));
        }
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.materialize(model_ref, model_access))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{CredentialCreateParams, CredentialKind, InMemorySecretStore};
    use awaken_model_catalog::repo::InMemoryCatalogRepo;
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
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

    fn activation_with_fallback(primary: &str, fallback: &str) -> RunActivation {
        RunActivation::new(
            RunId("run".into()),
            ThreadId("thread".into()),
            ExecutableAgentSnapshot {
                id: ExecutableAgentSnapshotId("snapshot".into()),
                metadata: Default::default(),
                root_agent_id: AgentId("agent".into()),
                resolved_spec: ResolvedSpec {
                    model_candidates: vec![ModelBinding::new("openai", fallback, "genai")],
                    catalog_fingerprint: CatalogFingerprint("catalog".into()),
                    instructions: String::new(),
                    max_steps: 2,
                    delegation_limits: Default::default(),
                    model_binding: ModelBinding::new("anthropic", primary, "genai"),
                    tool_descriptors: Vec::new(),
                    plugin_ids: Vec::new(),
                    plugin_config: Default::default(),
                    context_policy: Default::default(),
                    tool_presentation: Default::default(),
                },
                fingerprint: CatalogFingerprint("catalog".into()),
            },
            Vec::new(),
        )
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
    async fn explicitly_installed_host_fallback_is_pinned_and_exact() {
        let fallback: Arc<dyn LlmExecutor> = Arc::new(crate::no_model::NoModelConfiguredExecutor);
        let p = provider("configured", None)
            .await
            .with_fallback_executor("embedded", fallback.clone());
        let activation = activation_with_fallback("configured", "other")
            .with_model_ref_override(Some("embedded".to_string()));

        let pinned = p.pin_activation_access(&activation).await.unwrap();
        assert_eq!(pinned.candidates.len(), 1);
        let exact = pinned.for_model("embedded").unwrap();
        assert!(exact.is_host_executor_for("embedded"));
        let materialized = p.materialize("embedded", &exact).await.unwrap();
        assert!(Arc::ptr_eq(&materialized, &fallback));
        assert!(p.materialize("other", &exact).await.is_none());
        assert!(p.executor_for_run("embedded", None).is_none());
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
        assert!(
            p.executor_for_run("claude-x", None).is_none(),
            "a durable run without admission-pinned access must fail closed"
        );
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

    #[tokio::test]
    async fn cross_provider_fallback_uses_only_dispatch_pinned_access() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        p.catalog
            .put_provider(Provider {
                id: ProviderId::new("openai"),
                slug: "openai".into(),
                display_name: "OpenAI".into(),
                version: 3,
            })
            .await
            .unwrap();
        p.catalog
            .put_endpoint(ProtocolEndpoint {
                id: ProtocolEndpointId::new("ep-openai"),
                provider_id: ProviderId::new("openai"),
                dialect: ApiDialect::OpenAiChat,
                base_url: Some("https://api.openai.com/v1/".into()),
                timeout_secs: 300,
                display_name: "openai-prod".into(),
                version: 7,
            })
            .await
            .unwrap();
        p.catalog
            .put_offering(Offering {
                model_id: "gpt-x".into(),
                provider_id: ProviderId::new("openai"),
                protocol_endpoint_id: ProtocolEndpointId::new("ep-openai"),
                dialect: ApiDialect::OpenAiChat,
                upstream_model: None,
            })
            .await
            .unwrap();
        enter_credential(
            CredentialCreateParams {
                workspace_id: "ws".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("openai".into()),
                env_key: Some("OPENAI_API_KEY".into()),
                secret: Some(RedactedString::new("sk-test-openai")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();

        let pinned = p
            .pin_activation_access(&activation_with_fallback("claude-x", "gpt-x"))
            .await
            .unwrap();
        assert_eq!(
            pinned
                .candidates
                .iter()
                .map(|candidate| candidate.model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-x", "gpt-x"]
        );
        assert_eq!(
            pinned.candidates[1].provider_ref.as_deref(),
            Some("openai@3")
        );
        assert_eq!(
            pinned.candidates[1].route_ref.as_deref(),
            Some("ep-openai@7")
        );

        let primary = pinned.for_model("claude-x").unwrap();
        let primary_id = awaken_credential_vault::CredentialSourceId(primary.reference.clone());
        let mut primary_row = p.credentials.get(&primary_id).await.unwrap();
        primary_row.status = CredentialStatus::Disabled;
        p.credentials.put(primary_row).await.unwrap();
        assert!(p.resolve("claude-x", Some(&primary)).await.is_none());
        assert!(
            p.resolve("gpt-x", Some(&pinned.for_model("gpt-x").unwrap()))
                .await
                .is_some(),
            "the already-pinned fallback remains materializable"
        );
        assert!(pinned.for_model("new-global-default").is_none());
        let router = PinnedCandidateExecutor {
            provider: p,
            access: pinned,
        };
        assert!(router.executor_for("claude-x").await.is_err());
        assert!(router.executor_for("gpt-x").await.is_ok());
        assert!(router.executor_for("new-global-default").await.is_err());
    }
}
