//! The two composition adapters around immutable published inference access.
//! Configuration publication selects a complete route and credential reference
//! once for its trusted workspace scope. Runtime execution only realizes that
//! exact pin; it cannot enumerate the catalog, choose another credential, or
//! distinguish a local endpoint from a gateway.
//!
//! `InferenceExecutorMaterializer` is sync but exact stores are async; it bridges via
//! `block_in_place` + the ambient runtime handle. Durable admission pins the
//! non-secret provider/endpoint/credential ids, and execution fails closed if
//! any of those facts changed or the credential was disabled.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_config_resolver::{InferenceAccessPublisher, can_consume};
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialSource, CredentialSourceId, CredentialStatus, SecretStore,
};
use awaken_model_catalog::ProviderCatalog;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::{
    CredentialInjectionKind, CredentialUsage, InferenceAccess, InferenceEndpoint, ModelBinding,
};
use awaken_runtime_host::InferenceExecutorMaterializer;

use crate::executor_from_materialized_access;

/// Configuration-plane adapter that pins model routes and credential references
/// into an immutable executable snapshot. It has no secret-store or executor
/// dependency and is never needed by a worker.
#[derive(Clone)]
pub struct CatalogInferenceAccessPublisher {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    fallback_model_ref: Option<String>,
}

/// Runtime adapter that materializes only the access already pinned in an
/// executable snapshot. It cannot enumerate the model catalog or select a
/// different credential.
#[derive(Clone)]
pub struct CredentialInferenceMaterializer {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    fallback: Option<HostFallback>,
}

#[derive(Clone)]
struct HostFallback {
    model_ref: String,
    executor: Arc<dyn LlmExecutor>,
}

struct PinnedModelExecutor {
    inner: Arc<dyn LlmExecutor>,
    upstream_model: String,
}

#[async_trait]
impl LlmExecutor for PinnedModelExecutor {
    async fn infer(&self, mut request: ChatRequest) -> Result<ChatResponse, LlmError> {
        request
            .model_binding
            .model_ref
            .clone_from(&self.upstream_model);
        self.inner.infer(request).await
    }

    async fn infer_streaming(
        &self,
        mut request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        request
            .model_binding
            .model_ref
            .clone_from(&self.upstream_model);
        self.inner.infer_streaming(request, sink).await
    }
}

impl CatalogInferenceAccessPublisher {
    pub fn new(catalog: Arc<dyn CatalogRepo>, credentials: Arc<dyn CredentialRepo>) -> Self {
        Self {
            catalog,
            credentials,
            fallback_model_ref: None,
        }
    }

    /// Declare the host-provided model that publication may pin without a
    /// catalog offering. Runtime installation of its executor is a separate
    /// composition-root concern.
    #[must_use]
    pub fn with_fallback_model(mut self, model_ref: impl Into<String>) -> Self {
        self.fallback_model_ref = Some(model_ref.into());
        self
    }

    /// Configuration-side resolution entrypoint for compositions and tests. The
    /// runtime materialization port deliberately exposes no equivalent selection.
    pub async fn resolve_for_scope(
        &self,
        scope: &str,
        models: &[ModelBinding],
    ) -> Result<InferenceAccess, String> {
        self.pin_models_access(scope, models).await
    }

    fn fallback_access(&self, model_ref: &str) -> Option<InferenceAccess> {
        self.fallback_model_ref
            .as_ref()
            .filter(|fallback| fallback.as_str() == model_ref)
            .map(|_| InferenceAccess::host_executor(model_ref))
    }

    fn pin_access_from(
        catalog: &ProviderCatalog,
        sources: &[CredentialSource],
        scope: &str,
        model_ref: &str,
    ) -> Result<InferenceAccess, String> {
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
        let version = u64::try_from(chosen.version)
            .map_err(|_| format!("credential {} has a negative version", chosen.id.0))?;
        let base_url = endpoint
            .base_url
            .clone()
            .ok_or_else(|| format!("endpoint {} has no base URL", endpoint.id.0))?;
        Ok(InferenceAccess::resolved_credential(
            chosen.id.0.clone(),
            version,
            scope,
            format!("{}@{}", offering.provider_id.0, provider.version),
            format!("{}@{}", offering.protocol_endpoint_id.0, endpoint.version),
            InferenceEndpoint {
                adapter_kind: endpoint.dialect.adapter_kind().to_string(),
                base_url,
                upstream_model: offering
                    .upstream_model
                    .clone()
                    .unwrap_or_else(|| offering.model_id.clone()),
            },
        ))
    }

    async fn pin_models_access(
        &self,
        scope: &str,
        models: &[ModelBinding],
    ) -> Result<InferenceAccess, String> {
        let model_refs = models
            .iter()
            .map(|binding| binding.model_ref.clone())
            .collect::<Vec<_>>();
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
                .list(scope)
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
                Self::pin_access_from(&catalog, &sources, scope, &model_ref)
            } else {
                self.fallback_access(&model_ref)
                    .ok_or_else(|| format!("model offering {model_ref} is not published"))
            };
            match access {
                Ok(access) => pinned.push((model_ref, access)),
                Err(error) => last_error = Some(error),
            }
        }
        InferenceAccess::candidate_set(pinned).ok_or_else(|| {
            last_error.unwrap_or_else(|| "run has no materializable model candidate".to_string())
        })
    }
}

impl CredentialInferenceMaterializer {
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials,
            secrets,
            fallback: None,
        }
    }

    /// Install the host's existing explicit fallback for an exactly matching
    /// host-executor snapshot reference.
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

    fn fallback_executor(
        &self,
        model_ref: &str,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.fallback
            .as_ref()
            .filter(|fallback| {
                fallback.model_ref == model_ref && access.is_host_executor_for(model_ref)
            })
            .map(|fallback| fallback.executor.clone())
    }

    async fn materialize_pinned(
        &self,
        model_ref: &str,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if access.scheme != "credential-source/v1" {
            return None;
        }
        let provider = access.provider_ref.as_deref()?.split_once('@')?.0;
        let scope = access.scope_id.as_deref()?;
        let credential = access.credential_access.as_ref()?;
        if credential.injection != CredentialInjectionKind::Reference
            || credential.usage != CredentialUsage::ProviderAdapter
            || credential.credential.id != access.reference
        {
            return None;
        }
        let expected_version = credential.credential.revision;
        let source = self
            .credentials
            .get(&CredentialSourceId(access.reference.clone()))
            .await
            .ok()?;
        if source.workspace_id != scope
            || source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok()? != expected_version
            || !can_consume(provider, &source)
        {
            return None;
        }
        let secret = awaken_credential_vault::materialize(&source, self.secrets.as_ref())
            .await
            .ok()?;
        let endpoint = access.endpoint.as_ref()?;
        if endpoint.upstream_model.is_empty() {
            return None;
        }
        let _ = model_ref;
        let executor = executor_from_materialized_access(
            &endpoint.adapter_kind,
            Some(&endpoint.base_url),
            Some(&secret),
        )
        .ok()?;
        Some(Arc::new(PinnedModelExecutor {
            inner: executor,
            upstream_model: endpoint.upstream_model.clone(),
        }))
    }

    async fn materialize(
        &self,
        model_ref: &str,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if let Some(executor) = self.fallback_executor(model_ref, access) {
            Some(executor)
        } else {
            self.materialize_pinned(model_ref, access).await
        }
    }
}

struct PinnedCandidateExecutor {
    provider: CredentialInferenceMaterializer,
    access: InferenceAccess,
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

impl InferenceAccessPublisher for CatalogInferenceAccessPublisher {
    fn resolve_access<'a>(
        &'a self,
        scope: &'a str,
        models: &'a [ModelBinding],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<InferenceAccess, String>> + Send + 'a>,
    > {
        Box::pin(async move { self.resolve_for_scope(scope, models).await })
    }
}

impl InferenceExecutorMaterializer for CredentialInferenceMaterializer {
    fn materialize_pinned(
        &self,
        model_ref: &str,
        access: &InferenceAccess,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if !access.candidates.is_empty() {
            return Some(Arc::new(PinnedCandidateExecutor {
                provider: self.clone(),
                access: access.clone(),
            }));
        }
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.materialize(model_ref, access))
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
    use awaken_runtime_contract::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };

    /// Author a catalog with one anthropic offering for `model`, and optionally a
    /// workspace credential `(provider, active)`. The secret is a fake — resolution
    /// and executor construction never call the network, so every branch is
    /// reachable offline.
    struct TestServices {
        publisher: CatalogInferenceAccessPublisher,
        materializer: CredentialInferenceMaterializer,
        catalog: Arc<InMemoryCatalogRepo>,
        credentials: Arc<InMemoryCredentialRepo>,
        secrets: Arc<InMemorySecretStore>,
    }

    async fn provider(model: &str, credential: Option<(&str, bool)>) -> TestServices {
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
        TestServices {
            publisher: CatalogInferenceAccessPublisher::new(catalog.clone(), creds.clone()),
            materializer: CredentialInferenceMaterializer::new(creds.clone(), secrets.clone()),
            catalog,
            credentials: creds,
            secrets,
        }
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

    async fn pin_activation(
        provider: &CatalogInferenceAccessPublisher,
        activation: &RunActivation,
    ) -> Result<InferenceAccess, String> {
        let models = if let Some(model_ref) = activation.model_ref_override.as_ref() {
            vec![ModelBinding::new("override", model_ref, "genai")]
        } else {
            activation
                .snapshot
                .resolved_spec
                .candidate_bindings()
                .into_iter()
                .cloned()
                .collect()
        };
        provider.resolve_for_scope("ws", &models).await
    }

    #[tokio::test]
    async fn resolves_a_configured_model_to_an_executor() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "unused");
        let access = pin_activation(&p.publisher, &activation).await.unwrap();
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &access)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn explicitly_installed_host_fallback_is_pinned_and_exact() {
        let fallback: Arc<dyn LlmExecutor> = Arc::new(crate::no_model::NoModelConfiguredExecutor);
        let mut p = provider("configured", None).await;
        p.publisher = p.publisher.with_fallback_model("embedded");
        p.materializer = p
            .materializer
            .with_fallback_executor("embedded", fallback.clone());
        let activation = activation_with_fallback("configured", "other")
            .with_model_ref_override(Some("embedded".to_string()));

        let pinned = pin_activation(&p.publisher, &activation).await.unwrap();
        assert_eq!(pinned.candidates.len(), 1);
        let exact = pinned.for_model("embedded").unwrap();
        assert!(exact.is_host_executor_for("embedded"));
        let materialized = p
            .materializer
            .materialize("embedded", &exact)
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&materialized, &fallback));
        assert!(p.materializer.materialize("other", &exact).await.is_none());
    }

    #[tokio::test]
    async fn an_unconfigured_model_falls_back_to_the_host_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            pin_activation(
                &p.publisher,
                &activation_with_fallback("no-such-model", "unused")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn no_credential_falls_back_to_the_host_default() {
        let p = provider("claude-x", None).await;
        assert!(
            pin_activation(
                &p.publisher,
                &activation_with_fallback("claude-x", "unused")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_falls_back() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            pin_activation(
                &p.publisher,
                &activation_with_fallback("claude-x", "unused")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_falls_back() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            pin_activation(
                &p.publisher,
                &activation_with_fallback("claude-x", "unused")
            )
            .await
            .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_credential_never_switches_to_a_new_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "unused");
        let pinned = pin_activation(&p.publisher, &activation).await.unwrap();
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
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &pinned)
                .await
                .is_some()
        );

        let pinned_id = awaken_credential_vault::CredentialSourceId(pinned.reference.clone());
        let mut old = p.credentials.get(&pinned_id).await.unwrap();
        old.status = CredentialStatus::Disabled;
        p.credentials.put(old).await.unwrap();
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &pinned)
                .await
                .is_none(),
            "revoking the pinned credential fails closed instead of selecting the new default"
        );
    }

    #[tokio::test]
    async fn publication_selects_credentials_only_from_the_supplied_scope() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let other = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-b".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: Some("ANTHROPIC_API_KEY_B".into()),
                secret: Some(RedactedString::new("sk-test-b")),
                oauth_command: None,
            },
            p.secrets.as_ref(),
            p.credentials.as_ref(),
        )
        .await
        .unwrap();
        let model = ModelBinding::new("anthropic", "claude-x", "genai");
        let access = p
            .publisher
            .resolve_for_scope("workspace-b", &[model])
            .await
            .unwrap();
        assert_eq!(access.reference, other.id.0);
        assert_eq!(access.scope_id.as_deref(), Some("workspace-b"));
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &access)
                .await
                .is_some()
        );

        let mut forged = access;
        forged.scope_id = Some("ws".into());
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &forged)
                .await
                .is_none(),
            "execution rejects a credential whose persisted owner differs from the snapshot scope"
        );
    }

    #[tokio::test]
    async fn runtime_never_weakens_the_published_injection_policy() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let mut access = p
            .publisher
            .resolve_for_scope("ws", &[ModelBinding::new("anthropic", "claude-x", "genai")])
            .await
            .unwrap();
        access.credential_access.as_mut().unwrap().injection = CredentialInjectionKind::Direct;

        assert!(
            p.materializer
                .materialize_pinned("claude-x", &access)
                .await
                .is_none(),
            "a reference-only materializer cannot downgrade a direct-only publication policy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_route_is_independent_of_a_later_catalog_update() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "unused");
        let pinned = pin_activation(&p.publisher, &activation).await.unwrap();
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
            p.materializer
                .materialize_pinned("claude-x", &pinned)
                .await
                .is_some(),
            "execution uses the publication-pinned endpoint without consulting the updated catalog"
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

        let pinned = pin_activation(&p.publisher, &activation_with_fallback("claude-x", "gpt-x"))
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
            pinned.candidates[1].access.provider_ref.as_deref(),
            Some("openai@3")
        );
        assert_eq!(
            pinned.candidates[1].access.route_ref.as_deref(),
            Some("ep-openai@7")
        );

        let primary = pinned.for_model("claude-x").unwrap();
        let primary_id = awaken_credential_vault::CredentialSourceId(primary.reference.clone());
        let mut primary_row = p.credentials.get(&primary_id).await.unwrap();
        primary_row.status = CredentialStatus::Disabled;
        p.credentials.put(primary_row).await.unwrap();
        assert!(
            p.materializer
                .materialize_pinned("claude-x", &primary)
                .await
                .is_none()
        );
        assert!(
            p.materializer
                .materialize_pinned("gpt-x", &pinned.for_model("gpt-x").unwrap())
                .await
                .is_some(),
            "the already-pinned fallback remains materializable"
        );
        assert!(pinned.for_model("new-global-default").is_none());
        let router = PinnedCandidateExecutor {
            provider: p.materializer,
            access: pinned,
        };
        assert!(router.executor_for("claude-x").await.is_err());
        assert!(router.executor_for("gpt-x").await.is_ok());
        assert!(router.executor_for("new-global-default").await.is_err());
    }
}
