//! Runtime realization of immutable, publication-pinned model candidates.
//!
//! One materializer owns exact candidate validation, fallback routing, direct
//! provider credential realization, and (when enabled) brokered grant routing.
//! Configuration catalogs and credential selection never enter this boundary.

use std::sync::Arc;

use async_trait::async_trait;
#[cfg(feature = "authority")]
use awaken_credential_vault::{SecretStore, repo::CredentialRepo};
use awaken_runtime_contract::ModelBinding;
use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::resolved::{Backend, ModelProvisioning, ResolvedModelCandidate};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;

use crate::PinnedCredentialMaterializer;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolvedExecutorError {
    #[error("resolved inference for adapter `{0}` carries no base URL")]
    MissingBaseUrl(String),
    #[error("resolved inference carries no credential")]
    MissingCredential,
    #[error("no provider executor in this build serves adapter `{0}`")]
    UnsupportedAdapter(String),
    #[error("no provider executor in this build serves API dialect `{0}`")]
    UnsupportedDialect(String),
    #[error("API dialect `{dialect}` is incompatible with adapter `{adapter}`")]
    DialectAdapterMismatch { dialect: String, adapter: String },
    #[error("provider executor could not be constructed: {0}")]
    ExecutorBuild(String),
}

pub fn executor_from_materialized_endpoint(
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    let expected_adapter = match api_dialect {
        "" => None,
        "anthropic_messages" => Some("anthropic"),
        "open_ai_chat" => Some("openai"),
        "gemini" => Some("gemini"),
        "vertex_gemini" => Some("vertex"),
        "open_ai_responses" => Some("openai"),
        other => return Err(ResolvedExecutorError::UnsupportedDialect(other.to_string())),
    };
    if expected_adapter.is_some_and(|expected| expected != adapter_kind) {
        return Err(ResolvedExecutorError::DialectAdapterMismatch {
            dialect: api_dialect.to_string(),
            adapter: adapter_kind.to_string(),
        });
    }
    let base_url =
        base_url.ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
    let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
    if api_dialect == "open_ai_responses" {
        return awaken_provider_genai::OpenAiResponsesExecutor::new(
            base_url,
            credential.expose_secret(),
        )
        .map(|executor| Arc::new(executor) as Arc<dyn LlmExecutor>)
        .map_err(|error| ResolvedExecutorError::ExecutorBuild(error.to_string()));
    }
    let adapter = match adapter_kind {
        "anthropic" => awaken_provider_genai::AdapterKind::Anthropic,
        "gemini" => awaken_provider_genai::AdapterKind::Gemini,
        "vertex" => awaken_provider_genai::AdapterKind::Vertex,
        "openai" => awaken_provider_genai::AdapterKind::OpenAI,
        other => return Err(ResolvedExecutorError::UnsupportedAdapter(other.to_string())),
    };
    Ok(Arc::new(
        awaken_provider_genai::GenaiExecutor::from_resolved(
            adapter,
            Some(base_url.to_string()),
            credential.expose_secret(),
        ),
    ))
}

/// Worker/host adapter for one immutable candidate set. Direct and brokered
/// supply differ only at the final realization edge; all candidate fencing and
/// fallback routing is shared here.
#[derive(Clone)]
pub struct CredentialInferenceMaterializer {
    credentials: PinnedCredentialMaterializer,
    #[cfg(feature = "brokered")]
    brokered: Option<Arc<dyn crate::brokered_inference::BrokeredInferenceClient>>,
    #[cfg(feature = "brokered")]
    brokered_mode_enabled: bool,
}

impl CredentialInferenceMaterializer {
    #[cfg(feature = "authority")]
    #[must_use]
    pub fn new(credentials: Arc<dyn CredentialRepo>, secrets: Arc<dyn SecretStore>) -> Self {
        Self::from_pinned(PinnedCredentialMaterializer::new(credentials, secrets))
    }

    #[must_use]
    pub fn from_pinned(credentials: PinnedCredentialMaterializer) -> Self {
        Self {
            credentials,
            #[cfg(feature = "brokered")]
            brokered: None,
            #[cfg(feature = "brokered")]
            brokered_mode_enabled: false,
        }
    }

    #[cfg(feature = "brokered")]
    #[must_use]
    pub fn with_brokered_mode(mut self, enabled: bool) -> Self {
        self.brokered_mode_enabled = enabled;
        self
    }

    #[cfg(feature = "brokered")]
    #[must_use]
    pub fn with_brokered_client(
        mut self,
        client: Arc<dyn crate::brokered_inference::BrokeredInferenceClient>,
    ) -> Self {
        self.brokered = Some(client);
        self.brokered_mode_enabled = true;
        self
    }

    async fn materialize_exact(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
        local_run_correlation: Option<String>,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            endpoint,
            ..
        } = &candidate.provisioning
        else {
            return Ok(None);
        };

        #[cfg(feature = "brokered")]
        if route_ref.starts_with(crate::brokered_inference::BROKERED_ROUTE_PREFIX) {
            if !self.brokered_mode_enabled {
                return Err("cloud_models_disabled: brokered model supply is disabled".into());
            }
            let client = self.brokered.clone().ok_or_else(|| {
                "cloud_sign_in_required: brokered inference needs an authenticated Awaken Cloud identity"
                    .to_string()
            })?;
            return crate::brokered_inference::BrokeredCandidateExecutor::new(
                client,
                provider_ref,
                &candidate.binding.model_ref,
                &endpoint.api_dialect,
                &endpoint.adapter_kind,
                local_run_correlation,
                context.ownership.clone(),
            )
            .map(|executor| Some(Arc::new(executor) as Arc<dyn LlmExecutor>));
        }

        #[cfg(not(feature = "brokered"))]
        let _ = (provider_ref, route_ref, local_run_correlation);

        let secret = self
            .credentials
            .materialize_claimed_provider(
                candidate,
                context,
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            )
            .await?;
        if endpoint.upstream_model.is_empty() {
            return Ok(None);
        }
        let inner = executor_from_materialized_endpoint(
            &endpoint.api_dialect,
            &endpoint.adapter_kind,
            Some(&endpoint.base_url),
            secret.as_ref(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(Arc::new(PinnedModelExecutor {
            inner,
            upstream_model: endpoint.upstream_model.clone(),
        })))
    }

    /// Realize one complete publication candidate through the exact attempt
    /// authority. The public helper intentionally collapses failure to absence;
    /// the runtime trait path below preserves diagnostic errors.
    pub async fn materialize_candidate(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        self.materialize_exact(candidate, context, None)
            .await
            .ok()
            .flatten()
    }
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

struct PinnedCandidateExecutor {
    materializer: CredentialInferenceMaterializer,
    candidates: Vec<ResolvedModelCandidate>,
    realization: Option<awaken_runtime_contract::AttemptCredentialRealization>,
    ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
    local_run_correlation: Option<String>,
}

impl PinnedCandidateExecutor {
    async fn executor_for(
        &self,
        requested: &ModelBinding,
    ) -> Result<Arc<dyn LlmExecutor>, LlmError> {
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| &candidate.binding == requested)
            .ok_or_else(|| {
                LlmError::Binding(format!(
                    "model {} is outside the publication-pinned candidate set",
                    requested.model_ref
                ))
            })?;
        self.materializer
            .materialize_exact(
                candidate,
                &RuntimeRunContext {
                    credential_realization: self.realization.clone(),
                    ownership: self.ownership.clone(),
                    ..RuntimeRunContext::new()
                },
                self.local_run_correlation.clone(),
            )
            .await
            .map_err(LlmError::Binding)?
            .ok_or_else(|| {
                LlmError::Binding(format!(
                    "publication-pinned model candidate is unavailable for {}",
                    requested.model_ref
                ))
            })
    }
}

#[async_trait]
impl LlmExecutor for PinnedCandidateExecutor {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.executor_for(&request.model_binding)
            .await?
            .infer(request)
            .await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        self.executor_for(&request.model_binding)
            .await?
            .infer_streaming(request, sink)
            .await
    }
}

impl InferenceExecutorMaterializer for CredentialInferenceMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        #[cfg(feature = "brokered")]
        if self.brokered.is_some() {
            return &[
                awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY,
                crate::brokered_inference::BROKERED_INFERENCE_ACCESS_CAPABILITY,
            ];
        }
        &[awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY]
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        let (material_sources, recipient_bound_envelopes) =
            self.credentials.material_source_capabilities();
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: [awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]
            .into_iter()
            .collect(),
            material_sources,
            realization_kinds: [
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]
            .into_iter()
            .collect(),
            recipient_bound_envelopes,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        }
    }

    fn materialize(
        &self,
        activation: &awaken_runtime_contract::activation::RunActivation,
        context: &RuntimeRunContext,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let exact = activation
            .snapshot
            .resolved_spec
            .candidate_for_model(activation.effective_model_ref())
            .ok_or_else(|| {
                "effective model is outside the publication candidate set".to_string()
            })?;
        if !matches!(
            Backend::from_ref(&exact.binding.backend_ref),
            Backend::Native
        ) {
            return Ok(None);
        }
        let candidates = activation
            .snapshot
            .resolved_spec
            .execution_candidates(activation.model_ref_override.as_deref())
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        for candidate in &candidates {
            if let ModelProvisioning::Provider {
                credential: Some(access),
                ..
            } = &candidate.provisioning
            {
                let binding = context
                    .credential_realization
                    .as_ref()
                    .ok_or_else(|| "credential-bearing inference has no claim binding".to_string())?
                    .binding_for(candidate)
                    .map_err(|error| error.to_string())?
                    .ok_or_else(|| {
                        "candidate has no exact attempt credential binding".to_string()
                    })?;
                if binding.credential != access.credential
                    || binding.selected_realization_kind
                        != awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter
                {
                    return Err(
                        "attempt credential binding differs from Native materializer".into(),
                    );
                }
            }
        }
        Ok(Some(Arc::new(PinnedCandidateExecutor {
            materializer: self.clone(),
            candidates,
            realization: context.credential_realization.clone(),
            ownership: context.ownership.clone(),
            local_run_correlation: Some(activation.run_id.0.clone()),
        })))
    }

    fn materialize_pinned(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let binding = context
            .credential_realization
            .as_ref()
            .and_then(|realization| realization.binding_for(candidate).ok().flatten());
        Some(Arc::new(PinnedCandidateExecutor {
            materializer: self.clone(),
            candidates: vec![candidate.clone()],
            realization: context.credential_realization.clone(),
            ownership: context.ownership.clone(),
            local_run_correlation: None,
        }) as Arc<dyn LlmExecutor>)
        .filter(|_| {
            !matches!(
                &candidate.provisioning,
                ModelProvisioning::Provider {
                    credential: Some(_),
                    ..
                }
            ) || binding.is_some()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;

    #[test]
    fn provider_executor_factory_decision_table() {
        // Causes: C1 dialect is supported, C2 dialect matches adapter, C3
        // adapter is supported, C4 base URL exists, C5 credential exists.
        // Effects: E1 exact construction or the corresponding fail-closed typed
        // error. The table enumerates all true plus each single false cause;
        // composition crates deliberately own no parallel dialect map.
        let credential = RedactedString::new("sk-test");
        for (dialect, adapter) in [
            ("anthropic_messages", "anthropic"),
            ("open_ai_chat", "openai"),
            ("open_ai_responses", "openai"),
            ("gemini", "gemini"),
            ("vertex_gemini", "vertex"),
        ] {
            assert!(
                executor_from_materialized_endpoint(
                    dialect,
                    adapter,
                    Some("https://provider.invalid"),
                    Some(&credential),
                )
                .is_ok(),
                "all causes true for {dialect}/{adapter} -> E1"
            );
        }
        assert!(matches!(
            executor_from_materialized_endpoint(
                "unknown",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedDialect(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "anthropic_messages",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::DialectAdapterMismatch { .. })
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "",
                "unknown",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedAdapter(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint("open_ai_chat", "openai", None, Some(&credential)),
            Err(ResolvedExecutorError::MissingBaseUrl(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "open_ai_chat",
                "openai",
                Some("https://provider.invalid"),
                None,
            ),
            Err(ResolvedExecutorError::MissingCredential)
        ));
    }

    #[cfg(feature = "brokered")]
    struct CurrentOwnership;

    #[cfg(feature = "brokered")]
    #[async_trait::async_trait]
    impl awaken_runtime_contract::AttemptOwnershipVerifier for CurrentOwnership {
        async fn verify_current(
            &self,
        ) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
            Ok(())
        }
    }

    #[cfg(feature = "brokered")]
    struct NeverGrantClient;

    #[cfg(feature = "brokered")]
    #[async_trait::async_trait]
    impl crate::brokered_inference::BrokeredInferenceClient for NeverGrantClient {
        async fn create_grant(
            &self,
            _request: crate::brokered_inference::BrokeredInferenceRequest,
        ) -> Result<
            crate::brokered_inference::BrokeredInferenceLease,
            crate::brokered_inference::BrokeredInferenceError,
        > {
            panic!("candidate selection must not acquire a grant")
        }

        async fn close_grant(
            &self,
            _grant_id: &str,
        ) -> Result<(), crate::brokered_inference::BrokeredInferenceError> {
            panic!("no grant was acquired")
        }

        async fn renew_grant(
            &self,
            _grant_id: &str,
        ) -> Result<
            crate::brokered_inference::BrokeredInferenceLease,
            crate::brokered_inference::BrokeredInferenceError,
        > {
            panic!("no grant was acquired")
        }
    }

    #[cfg(feature = "brokered")]
    fn brokered_candidate(model: &str) -> ResolvedModelCandidate {
        ResolvedModelCandidate::provider(
            ModelBinding::new("openai", model, "genai"),
            "openai@1",
            "brokered:awaken-cloud:openai:open_ai_responses@7",
            "workspace-a",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_responses".into(),
                base_url: "https://api.awakenworks.com".into(),
                upstream_model: model.into(),
            },
        )
    }

    #[cfg(feature = "brokered")]
    #[tokio::test]
    async fn brokered_mode_and_candidate_routing_share_one_decision_table() {
        // Causes: C1 brokered supply is enabled; C2 an authenticated client is
        // installed; C3 the requested binding is in the publication-pinned set.
        // Effects: E1 return a lazy exact executor without acquiring a grant;
        // E2 report disabled; E3 require sign-in; E4 reject an unpinned binding.
        // Rules: B1=!C1 -> E2; B2=C1&&!C2 -> E3; B3=C1&&C2&&C3 -> E1;
        // B4=C1&&C2&&!C3 -> E4. Primary and fallback both exercise B3.
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let materializer = CredentialInferenceMaterializer::new(credentials, secrets);
        let primary = brokered_candidate("model-primary");
        let fallback = brokered_candidate("model-fallback");
        let context = RuntimeRunContext::new().with_ownership(Arc::new(CurrentOwnership));

        assert!(
            materializer
                .materialize_exact(&primary, &context, None)
                .await
                .err()
                .expect("disabled brokered route must fail")
                .contains("cloud_models_disabled"),
            "B1/E2"
        );
        assert!(
            materializer
                .clone()
                .with_brokered_mode(true)
                .materialize_exact(&primary, &context, None)
                .await
                .err()
                .expect("enabled brokered route without a client must fail")
                .contains("cloud_sign_in_required"),
            "B2/E3"
        );

        let materializer = materializer.with_brokered_client(Arc::new(NeverGrantClient));
        assert!(
            materializer
                .supported_access_schemes()
                .contains(&crate::brokered_inference::BROKERED_INFERENCE_ACCESS_CAPABILITY)
        );
        let router = PinnedCandidateExecutor {
            materializer,
            candidates: vec![primary.clone(), fallback.clone()],
            realization: None,
            ownership: context.ownership,
            local_run_correlation: Some("run-a".into()),
        };
        assert!(router.executor_for(&primary.binding).await.is_ok(), "B3/E1");
        assert!(
            router.executor_for(&fallback.binding).await.is_ok(),
            "B3/E1 fallback"
        );
        assert!(
            router
                .executor_for(&ModelBinding::new("openai", "outside", "genai"))
                .await
                .is_err(),
            "B4/E4"
        );
    }
}
