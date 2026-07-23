//! Runtime realization of immutable published model candidates. Configuration
//! publication has already selected the complete route and credential reference;
//! execution only realizes that exact pin and cannot enumerate the catalog,
//! choose another credential, or distinguish a local endpoint from a gateway.
//!
//! `InferenceExecutorMaterializer` is sync but exact stores are async; it bridges via
//! `block_in_place` + the ambient runtime handle. Durable admission pins the
//! non-secret provider/endpoint/credential ids, and execution fails closed if
//! any of those facts changed or the credential was disabled.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_credential_vault::SecretStore;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_runtime_contract::ModelBinding;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_host::InferenceExecutorMaterializer;

use crate::executor_from_materialized_access;

/// Runtime adapter that materializes only the access already pinned in an
/// executable snapshot. It cannot enumerate the model catalog or select a
/// different credential.
#[derive(Clone)]
pub struct CredentialInferenceMaterializer {
    credentials: awaken_runtime_host::PinnedCredentialMaterializer,
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

impl CredentialInferenceMaterializer {
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            credentials: awaken_runtime_host::PinnedCredentialMaterializer::new(
                credentials,
                secrets,
            ),
        }
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

    /// Realize one complete publication candidate. This is the worker/provisioning
    /// boundary: callers cannot supply independent provider, endpoint, or
    /// credential choices.
    pub async fn materialize_candidate(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.materialize_pinned(candidate).await
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
        self.provider
            .materialize_candidate(candidate)
            .await
            .ok_or_else(|| {
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

impl InferenceExecutorMaterializer for CredentialInferenceMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[awaken_runtime_host::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY]
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
            tokio::runtime::Handle::current().block_on(self.materialize_candidate(candidate))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_config_store::ModelSelection;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, enter_credential};
    use awaken_credential_vault::{
        CredentialCreateParams, CredentialKind, CredentialStatus, InMemorySecretStore,
    };
    use awaken_model_catalog::repo::{CatalogRepo, InMemoryCatalogRepo};
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };
    use awaken_runtime_contract::RunActivation;
    use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
    use awaken_runtime_contract::snapshot::{
        AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
    };
    use awaken_runtime_host::{ModelPublicationResolver, ResolvedPublicationModels};

    use crate::model_resolver::CatalogModelPublicationResolver;

    /// Author a catalog with one anthropic offering for `model`, and optionally a
    /// workspace credential `(provider, active)`. The secret is a fake — resolution
    /// and executor construction never call the network, so every branch is
    /// reachable offline.
    struct TestServices {
        resolver: CatalogModelPublicationResolver,
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
                source: Default::default(),
                status: Default::default(),
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
            resolver: CatalogModelPublicationResolver::from_repo(catalog.clone(), creds.clone()),
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

    async fn resolve_activation(
        resolver: &CatalogModelPublicationResolver,
        activation: &RunActivation,
    ) -> Result<ResolvedPublicationModels, awaken_runtime_host::PublicationResolutionError> {
        let (primary, fallbacks) = if let Some(model_ref) = activation.model_ref_override.as_ref() {
            (
                activation
                    .snapshot
                    .resolved_spec
                    .candidate_for_model(model_ref)
                    .ok_or_else(|| {
                        awaken_runtime_host::PublicationResolutionError::Invalid(format!(
                            "model {model_ref} is outside the published candidate set"
                        ))
                    })?
                    .binding
                    .clone(),
                Vec::new(),
            )
        } else {
            (
                activation
                    .snapshot
                    .resolved_spec
                    .model_binding
                    .binding
                    .clone(),
                activation
                    .snapshot
                    .resolved_spec
                    .model_candidates
                    .iter()
                    .map(|candidate| candidate.binding.clone())
                    .collect(),
            )
        };
        resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("ws"),
                &ModelSelection::Pinned(primary),
                &fallbacks,
            )
            .await
    }

    #[tokio::test]
    async fn resolves_a_configured_model_to_an_executor() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let models = resolve_activation(&p.resolver, &activation).await.unwrap();
        assert!(
            p.materializer
                .materialize_pinned(&models.primary)
                .await
                .is_some()
        );
    }

    #[tokio::test]
    async fn publication_rejects_a_pool_when_any_candidate_cannot_be_pinned() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let error = resolve_activation(
            &p.resolver,
            &activation_with_fallback("claude-x", "missing-fallback"),
        )
        .await
        .expect_err("a partial candidate pool must not be published");

        assert!(error.to_string().contains("missing-fallback"));
    }

    #[tokio::test]
    async fn credential_materializer_rejects_host_executor_candidates() {
        let p = provider("configured", None).await;
        let candidate = awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
            ModelBinding::new("host", "embedded", "native"),
        );
        assert!(
            p.materializer
                .materialize_candidate(&candidate)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unconfigured_model_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("no-such-model", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_model_without_a_credential_is_rejected() {
        let p = provider("claude-x", None).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_non_active_credential_is_rejected() {
        let p = provider("claude-x", Some(("anthropic", false))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_credential_for_another_provider_is_rejected() {
        let p = provider("claude-x", Some(("openai", true))).await;
        assert!(
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", ""))
                .await
                .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_credential_never_switches_to_a_new_default() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let pinned = resolve_activation(&p.resolver, &activation).await.unwrap();
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            credential: Some(credential),
            ..
        } = &pinned.primary.provisioning
        else {
            panic!("publication must carry a complete provider candidate")
        };
        assert_eq!(provider_ref, "anthropic@1");
        assert_eq!(route_ref, "ep1@1");
        let pinned_id = credential.credential.id.clone();

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
        assert_ne!(pinned_id, second.id.0);
        let candidate = pinned.primary.clone();
        assert!(
            p.materializer
                .materialize_pinned(&candidate)
                .await
                .is_some()
        );

        let mut old = p
            .credentials
            .get(&awaken_credential_vault::CredentialSourceId(pinned_id))
            .await
            .unwrap();
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
        let resolved = p
            .resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("workspace-b"),
                &ModelSelection::Pinned(model),
                &[],
            )
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            scope_id,
            credential: Some(credential),
            ..
        } = &resolved.primary.provisioning
        else {
            panic!("publication must carry a complete provider candidate")
        };
        assert_eq!(credential.credential.id, other.id.0);
        assert_eq!(scope_id.as_str(), "workspace-b");
        assert!(
            p.materializer
                .materialize_pinned(&resolved.primary)
                .await
                .is_some()
        );

        let mut forged = resolved.primary;
        let ModelProvisioning::Provider { scope_id, .. } = &mut forged.provisioning else {
            unreachable!()
        };
        *scope_id = "ws".into();
        assert!(
            p.materializer.materialize_pinned(&forged).await.is_none(),
            "execution rejects a credential whose persisted owner differs from the snapshot scope"
        );
    }

    #[tokio::test]
    async fn runtime_never_weakens_the_published_injection_policy() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let mut resolved = p
            .resolver
            .resolve_models(
                &awaken_tenancy::ScopeId::from("ws"),
                &ModelSelection::Pinned(ModelBinding::new("anthropic", "claude-x", "genai")),
                &[],
            )
            .await
            .unwrap();
        let ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        } = &mut resolved.primary.provisioning
        else {
            panic!("publication must carry a credential pin")
        };
        credential.injection = awaken_runtime_contract::CredentialInjectionKind::Direct;

        assert!(
            p.materializer
                .materialize_pinned(&resolved.primary)
                .await
                .is_none(),
            "a reference-only materializer cannot downgrade a direct-only publication policy"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pinned_route_is_independent_of_a_later_catalog_update() {
        let p = provider("claude-x", Some(("anthropic", true))).await;
        let activation = activation_with_fallback("claude-x", "");
        let pinned = resolve_activation(&p.resolver, &activation).await.unwrap();
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
                .materialize_pinned(&pinned.primary)
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
                source: Default::default(),
                status: Default::default(),
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

        let pinned =
            resolve_activation(&p.resolver, &activation_with_fallback("claude-x", "gpt-x"))
                .await
                .unwrap();
        assert_eq!(
            std::iter::once(&pinned.primary)
                .chain(pinned.candidates.iter())
                .map(|candidate| candidate.binding.model_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-x", "gpt-x"]
        );
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            ..
        } = &pinned.candidates[0].provisioning
        else {
            panic!("fallback must be provider-backed")
        };
        assert_eq!(provider_ref, "openai@3");
        assert_eq!(route_ref, "ep-openai@7");

        let ModelProvisioning::Provider {
            credential: Some(primary_credential),
            ..
        } = &pinned.primary.provisioning
        else {
            panic!("primary must carry its credential pin")
        };
        let primary_id =
            awaken_credential_vault::CredentialSourceId(primary_credential.credential.id.clone());
        let mut primary_row = p.credentials.get(&primary_id).await.unwrap();
        primary_row.status = CredentialStatus::Disabled;
        p.credentials.put(primary_row).await.unwrap();
        let primary_candidate = pinned.primary;
        let fallback_candidate = pinned.candidates[0].clone();
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
