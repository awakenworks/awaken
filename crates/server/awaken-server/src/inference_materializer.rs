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
use awaken_credential_vault::{CredentialSource, CredentialStatus, SecretStore};
use awaken_model_catalog::ProviderCatalog;
use awaken_model_catalog::repo::CatalogRepo;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::{InferenceAccess, InferenceEndpoint, ModelBinding};
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
    credentials: awaken_runtime_host::PinnedCredentialMaterializer,
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
        let mut pinned = Vec::with_capacity(model_refs.len());
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
            pinned.push((model_ref, access?));
        }
        InferenceAccess::candidate_set(pinned)
            .ok_or_else(|| "publication has no materializable model candidate".to_string())
    }
}

impl CredentialInferenceMaterializer {
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials: awaken_runtime_host::PinnedCredentialMaterializer::new(
                credentials,
                secrets,
            ),
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
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.fallback
            .as_ref()
            .filter(|fallback| {
                fallback.model_ref == candidate.binding.model_ref
                    && matches!(candidate.provisioning, ModelProvisioning::HostExecutor)
            })
            .map(|fallback| fallback.executor.clone())
    }

    async fn materialize_pinned(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let ModelProvisioning::Provider { endpoint, .. } = &candidate.provisioning else {
            return None;
        };
        let secret = self
            .credentials
            .materialize_provider(candidate)
            .await
            .ok()?;
        if endpoint.upstream_model.is_empty() {
            return None;
        }
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
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>> {
        if let Some(executor) = self.fallback_executor(candidate) {
            Some(executor)
        } else {
            self.materialize_pinned(candidate).await
        }
    }
}

struct PinnedCandidateExecutor {
    provider: CredentialInferenceMaterializer,
    candidates: Vec<ResolvedModelCandidate>,
}

impl PinnedCandidateExecutor {
    async fn executor_for(&self, binding: &ModelBinding) -> Result<Arc<dyn LlmExecutor>, LlmError> {
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| &candidate.binding == binding)
            .ok_or_else(|| {
                LlmError::Binding(format!(
                    "model {} is outside the publication-pinned candidate set",
                    binding.model_ref
                ))
            })?;
        self.provider.materialize(candidate).await.ok_or_else(|| {
            LlmError::Binding(format!(
                "publication-pinned model candidate is unavailable for {}",
                binding.model_ref
            ))
        })
    }
}

#[async_trait]
impl LlmExecutor for PinnedCandidateExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let executor = self.executor_for(&request.model_binding).await?;
        executor.infer(request).await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        let executor = self.executor_for(&request.model_binding).await?;
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
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &["credential-source/v1"]
    }

    fn materialize(
        &self,
        activation: &awaken_runtime_contract::activation::RunActivation,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let candidates = std::iter::once(&activation.snapshot.resolved_spec.model_binding)
            .chain(activation.snapshot.resolved_spec.model_candidates.iter())
            .cloned()
            .collect::<Vec<_>>();
        candidates
            .iter()
            .any(|candidate| candidate.binding.model_ref == activation.effective_model_ref())
            .then(|| {
                Arc::new(PinnedCandidateExecutor {
                    provider: self.clone(),
                    candidates,
                }) as Arc<dyn LlmExecutor>
            })
    }

    fn materialize_pinned(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.materialize(candidate))
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
                    model_candidates: (!fallback.is_empty())
                        .then(|| {
                            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                                ModelBinding::new("openai", fallback, "genai"),
                            )
                        })
                        .into_iter()
                        .collect(),
                    catalog_fingerprint: CatalogFingerprint("catalog".into()),
                    instructions: String::new(),
                    max_steps: 2,
                    delegation_limits: Default::default(),
                    model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                        ModelBinding::new("anthropic", primary, "genai"),
                    ),
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
            vec![
                activation
                    .snapshot
                    .resolved_spec
                    .candidate_for_model(model_ref)
                    .ok_or_else(|| {
                        format!("model {model_ref} is outside the published candidate set")
                    })?
                    .binding
                    .clone(),
            ]
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

    fn published_candidate(model_ref: &str, access: &InferenceAccess) -> ResolvedModelCandidate {
        let exact = access
            .for_model(model_ref)
            .expect("test access contains the requested candidate");
        let binding = ModelBinding::new("test-identity", model_ref, "genai");
        if exact.is_host_executor_for(model_ref) {
            return ResolvedModelCandidate::host(binding);
        }
        ResolvedModelCandidate {
            binding,
            provisioning: ModelProvisioning::Provider {
                provider_ref: exact.provider_ref.expect("provider pin"),
                route_ref: exact.route_ref.expect("route pin"),
                scope_id: exact.scope_id.expect("scope pin"),
                credential: exact.credential_access.map(Box::new),
                endpoint: Box::new(exact.endpoint.expect("endpoint pin")),
            },
        }
    }

    #[tokio::test]
    async fn resolves_a_configured_model_to_an_executor() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let access = pin_activation(&p.publisher, &activation).await.unwrap();
        let candidate = published_candidate("claude-x", &access);
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn publication_rejects_a_pool_when_any_candidate_cannot_be_pinned() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let error = pin_activation(
            &p.publisher,
            &activation_with_fallback("claude-x", "missing-fallback"),
        )
        .await
        .expect_err("a partial candidate pool must not be published");

        assert!(error.contains("missing-fallback"));
    }

    #[tokio::test]
    async fn runtime_failover_materializes_only_the_next_published_candidate() {
        use awaken_agent_contract::agent::run::{EndCause, RunState};
        use awaken_runtime::{LlmRetryPolicy, Runtime};
        use awaken_runtime_contract::execution::RunExecutor;
        use awaken_runtime_contract::llm::{AssistantOutput, ChatResponse};
        use awaken_runtime_contract::runtime_context::RuntimeRunContext;

        struct Fixed;
        #[async_trait]
        impl LlmExecutor for Fixed {
            async fn infer(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
                Ok(ChatResponse {
                    output: AssistantOutput::text("fallback"),
                    usage: None,
                    stop_reason: None,
                })
            }
        }

        let p = provider("unused", None).await;
        let materializer = p
            .materializer
            .with_fallback_executor("fallback", Arc::new(Fixed));
        let activation = activation_with_fallback("unavailable-primary", "fallback");
        let candidates = std::iter::once(&activation.snapshot.resolved_spec.model_binding)
            .chain(activation.snapshot.resolved_spec.model_candidates.iter())
            .cloned()
            .collect();
        let runtime = Runtime::new()
            .with_llm(Arc::new(PinnedCandidateExecutor {
                provider: materializer,
                candidates,
            }))
            .with_retry_policy(LlmRetryPolicy {
                max_retries: 0,
                backoff_base_ms: 0,
                overloaded_backoff_base_ms: 0,
            });

        let state = runtime
            .execute(activation, RuntimeRunContext::new())
            .await
            .expect("published fallback runs");
        assert!(matches!(state, RunState::Ended(EndCause::NaturalEnd)));
    }

    #[tokio::test]
    async fn explicitly_installed_host_fallback_is_pinned_and_exact() {
        let fallback: Arc<dyn LlmExecutor> = Arc::new(crate::no_model::NoModelConfiguredExecutor);
        let mut p = provider("configured", None).await;
        p.publisher = p.publisher.with_fallback_model("embedded");
        p.materializer = p
            .materializer
            .with_fallback_executor("embedded", fallback.clone());
        let mut activation = activation_with_fallback("configured", "other");
        activation.snapshot.resolved_spec.model_candidates.push(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(ModelBinding::new(
                "host", "embedded", "genai",
            )),
        );
        let activation = activation.with_model_ref_override(Some("embedded".to_string()));

        let pinned = pin_activation(&p.publisher, &activation).await.unwrap();
        assert_eq!(pinned.candidates.len(), 1);
        let exact = pinned.for_model("embedded").unwrap();
        assert!(exact.is_host_executor_for("embedded"));
        let exact = published_candidate("embedded", &pinned);
        let materialized = p.materializer.materialize(&exact).await.unwrap();
        assert!(Arc::ptr_eq(&materialized, &fallback));
        let mut other = exact;
        other.binding.model_ref = "other".into();
        assert!(p.materializer.materialize(&other).await.is_none());
    }

    #[tokio::test]
    async fn an_unconfigured_model_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            pin_activation(&p.publisher, &activation_with_fallback("no-such-model", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_model_without_a_credential_is_rejected() {
        let p = provider("claude-x", None).await;
        assert!(
            pin_activation(&p.publisher, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            pin_activation(&p.publisher, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_is_rejected() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            pin_activation(&p.publisher, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_credential_never_switches_to_a_new_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
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
        let candidate = published_candidate("claude-x", &pinned);
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
                .await
                .is_some()
        );

        let pinned_id = awaken_credential_vault::CredentialSourceId(pinned.reference.clone());
        let mut old = p.credentials.get(&pinned_id).await.unwrap();
        old.status = CredentialStatus::Disabled;
        p.credentials.put(old).await.unwrap();
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
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
        let candidate = published_candidate("claude-x", &access);
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
                .await
                .is_some()
        );

        let mut forged = access;
        forged
            .candidates
            .first_mut()
            .expect("publication candidate")
            .access
            .scope_id = Some("ws".into());
        let forged = published_candidate("claude-x", &forged);
        assert!(
            p.materializer.materialize_pinned(&forged).await.is_none(),
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
        access
            .candidates
            .first_mut()
            .expect("publication candidate")
            .access
            .credential_access
            .as_mut()
            .expect("credential pin")
            .injection = awaken_runtime_contract::CredentialInjectionKind::Direct;
        let candidate = published_candidate("claude-x", &access);

        assert!(
            p.materializer
                .materialize_pinned(&candidate)
                .await
                .is_none(),
            "a reference-only materializer cannot downgrade a direct-only publication policy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_route_is_independent_of_a_later_catalog_update() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
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
        let candidate = published_candidate("claude-x", &pinned);
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
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
        let primary_candidate = published_candidate("claude-x", &pinned);
        let fallback_candidate = published_candidate("gpt-x", &pinned);
        assert!(
            p.materializer
                .materialize_pinned(&primary_candidate)
                .await
                .is_none()
        );
        assert!(
            p.materializer
                .materialize_pinned(&fallback_candidate)
                .await
                .is_some(),
            "the already-pinned fallback remains materializable"
        );
        assert!(pinned.for_model("new-global-default").is_none());
        let router = PinnedCandidateExecutor {
            provider: p.materializer,
            candidates: vec![primary_candidate.clone(), fallback_candidate.clone()],
        };
        assert!(
            router
                .executor_for(&primary_candidate.binding)
                .await
                .is_err()
        );
        assert!(
            router
                .executor_for(&fallback_candidate.binding)
                .await
                .is_ok()
        );
        assert!(
            router
                .executor_for(&ModelBinding::new(
                    "test-identity",
                    "new-global-default",
                    "genai"
                ))
                .await
                .is_err()
        );
    }
}
