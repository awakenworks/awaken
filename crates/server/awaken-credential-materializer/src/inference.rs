// @formal entrypoint config.inference_access_resolved_once_at_publication
// @formal entrypoint model_route.fallback_preserves_the_complete_publication_pin
// @formal entrypoint worker.revoked_credential_blocks_new_materialization
// @formal evidence model_binding.constructor_and_wire_share_one_admission
// @formal evidence model_operation.ordinal_never_wraps_or_reuses_a_coordinate
//! Runtime realization of immutable, publication-pinned model candidates.
//!
//! One materializer owns exact candidate validation, fallback routing, direct
//! provider credential realization, and (when enabled) brokered grant routing.
//! Configuration catalogs and credential selection never enter this boundary.

use std::sync::Arc;

use async_trait::async_trait;
#[cfg(feature = "authority")]
use awaken_credential_vault::{SecretStore, repo::CredentialRepo};
use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};
use awaken_runtime_contract::resolved::{
    Backend, ModelProvisioning, ProviderAccessKind, ResolvedModelCandidate,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::{ModelBinding, UnspecifiedReasoning};

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
    #[error("API dialect `{0}` cannot enforce the frozen reasoning policy")]
    UnsupportedReasoningPolicy(String),
}

pub fn executor_from_materialized_endpoint(
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    executor_from_materialized_endpoint_for_provider(
        adapter_kind,
        api_dialect,
        adapter_kind,
        base_url,
        credential,
        UnspecifiedReasoning::ProviderDefault,
    )
}

/// Provider-aware endpoint realization. The provider kind is independent of
/// the wire adapter so compatible gateways cannot accidentally inherit native
/// capabilities from the protocol they emulate.
pub fn executor_from_materialized_endpoint_for_provider(
    provider_kind: &str,
    api_dialect: &str,
    adapter_kind: &str,
    base_url: Option<&str>,
    credential: Option<&awaken_agent_contract::RedactedString>,
    unspecified_reasoning: UnspecifiedReasoning,
) -> Result<Arc<dyn LlmExecutor>, ResolvedExecutorError> {
    let expected_adapter = match api_dialect {
        "anthropic_messages" => "anthropic",
        "open_ai_chat" => "openai",
        "gemini" => "gemini",
        "vertex_gemini" => "vertex",
        "open_ai_responses" => "openai",
        other => return Err(ResolvedExecutorError::UnsupportedDialect(other.to_string())),
    };
    if expected_adapter != adapter_kind {
        return Err(ResolvedExecutorError::DialectAdapterMismatch {
            dialect: api_dialect.to_string(),
            adapter: adapter_kind.to_string(),
        });
    }
    let base_url =
        base_url.ok_or_else(|| ResolvedExecutorError::MissingBaseUrl(adapter_kind.to_string()))?;
    let credential = credential.ok_or(ResolvedExecutorError::MissingCredential)?;
    if api_dialect == "open_ai_responses" {
        if unspecified_reasoning != UnspecifiedReasoning::ProviderDefault {
            return Err(ResolvedExecutorError::UnsupportedReasoningPolicy(
                api_dialect.to_string(),
            ));
        }
        return awaken_provider_genai::OpenAiResponsesExecutor::from_materialized_endpoint(
            provider_kind,
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
    awaken_provider_genai::GenaiExecutor::from_materialized_provider_endpoint(
        provider_kind,
        adapter,
        base_url,
        credential.expose_secret(),
        unspecified_reasoning,
    )
    .map(|executor| Arc::new(executor) as Arc<dyn LlmExecutor>)
    .map_err(|error| ResolvedExecutorError::ExecutorBuild(error.to_string()))
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
            access_kind,
            endpoint,
            unspecified_reasoning,
            ..
        } = candidate.provisioning()
        else {
            return Ok(None);
        };

        if *access_kind == ProviderAccessKind::Brokered {
            #[cfg(feature = "brokered")]
            {
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
                    candidate.binding().identity().1,
                    endpoint,
                    *unspecified_reasoning,
                    local_run_correlation,
                    context.ownership.clone(),
                )
                .map(|executor| Some(Arc::new(executor) as Arc<dyn LlmExecutor>));
            }
            #[cfg(not(feature = "brokered"))]
            return Err("cloud_models_disabled: brokered model supply is disabled".into());
        }

        #[cfg(not(feature = "brokered"))]
        let _ = local_run_correlation;

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
        let inner = executor_from_materialized_endpoint_for_provider(
            provider_ref,
            &endpoint.api_dialect,
            &endpoint.adapter_kind,
            Some(&endpoint.base_url),
            secret.as_ref(),
            *unspecified_reasoning,
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
    fn effect_scope(
        &self,
        request: &ChatRequest,
    ) -> awaken_runtime_contract::model_operation::ModelEffectScope {
        self.inner.effect_scope(request)
    }

    async fn infer(&self, mut request: ChatRequest) -> Result<ChatResponse, LlmError> {
        request.model_binding = request.project_upstream_model(&self.upstream_model)?;
        self.inner.infer(request).await
    }

    async fn infer_streaming(
        &self,
        mut request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        request.model_binding = request.project_upstream_model(&self.upstream_model)?;
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
            .find(|candidate| candidate.binding() == requested)
            .ok_or_else(|| {
                LlmError::Binding(format!(
                    "model {} is outside the publication-pinned candidate set",
                    requested.identity().1
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
                    requested.identity().1
                ))
            })
    }
}

#[async_trait]
impl LlmExecutor for PinnedCandidateExecutor {
    fn effect_scope(
        &self,
        request: &ChatRequest,
    ) -> awaken_runtime_contract::model_operation::ModelEffectScope {
        let candidate = self
            .candidates
            .iter()
            .find(|candidate| candidate.binding() == &request.model_binding);
        match candidate.map(|candidate| candidate.provisioning()) {
            Some(awaken_runtime_contract::resolved::ModelProvisioning::Provider {
                provider_ref,
                endpoint,
                ..
            }) => awaken_provider_genai::provider_model_effect_scope(
                provider_ref,
                &endpoint.api_dialect,
                &endpoint.adapter_kind,
                request,
            ),
            _ => Default::default(),
        }
    }

    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.executor_for(&request.model_binding)
            .await
            .map_err(|error| {
                // Candidate resolution failed, so no adapter was entered and
                // no byte of this request reached a transport.
                error.with_execution_evidence(
                    awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                        awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(),
                    ),
                )
            })?
            .infer(request)
            .await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        self.executor_for(&request.model_binding)
            .await
            .map_err(|error| {
                // Candidate resolution failed, so no adapter was entered and
                // no byte of this request reached a transport.
                error.with_execution_evidence(
                    awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                        awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(),
                    ),
                )
            })?
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
        self.credentials.provider_adapter_capabilities()
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
            Backend::from_ref(exact.binding().identity().2),
            Backend::Native
        ) {
            return Ok(None);
        }
        let candidates = activation
            .snapshot
            .resolved_spec
            .attempt_candidates(activation.model_ref_override.as_deref())
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        for candidate in &candidates {
            if let ModelProvisioning::Provider {
                credential: Some(access),
                ..
            } = candidate.provisioning()
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
                candidate.provisioning(),
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

    #[tokio::test]
    async fn pinned_executor_preserves_inner_execution_evidence_for_both_modes() {
        use awaken_runtime_contract::model_operation::ModelExecutionEvidence;
        struct Reject {
            evidence: ModelExecutionEvidence,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        #[async_trait]
        impl LlmExecutor for Reject {
            async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(request.model_binding.identity(), ("p", "upstream", "b"));
                Err(LlmError::Provider("inner failure".into())
                    .with_execution_evidence(self.evidence))
            }
        }
        // C1 all three evidence values * C2 streaming/nonstreaming => exact
        // evidence and classification survive model-name pinning. A decorator
        // must never promote Unknown to Terminated merely because it returned.
        // C3 the pinned wire model is canonical / empty / padded. P1 canonical
        // => same provider/backend, exactly one inner call per mode, C1 retained.
        // P2 empty/padded => Binding + NotDispatched(RequestProjection), zero
        // inner calls in BOTH modes. The same Reject owns every positive/negative
        // row; projection cannot turn a caller's original binding into bad data.
        for evidence in [
            ModelExecutionEvidence::Unknown,
            ModelExecutionEvidence::NotDispatched(
                awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(),
            ),
            ModelExecutionEvidence::Terminated(
                awaken_runtime_contract::model_operation::TerminalReceipt::from_wire_status(Some(
                    "completed",
                ))
                .unwrap(),
            ),
        ] {
            for alias in ["upstream", "", " upstream", "upstream "] {
                let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let executor = PinnedModelExecutor {
                    inner: Arc::new(Reject {
                        evidence,
                        calls: calls.clone(),
                    }),
                    upstream_model: alias.into(),
                };
                let request = ChatRequest {
                    model_binding: ModelBinding::parse("p", "alias", "b").unwrap(),
                    inference: Default::default(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                };
                for streaming in [false, true] {
                    let error = if streaming {
                        executor
                            .infer_streaming(request.clone(), &crate::NoopDeltaSink)
                            .await
                    } else {
                        executor.infer(request.clone()).await
                    }
                    .unwrap_err();
                    if alias == "upstream" {
                        assert_eq!(error.code(), "provider_error", "P1");
                        assert_eq!(error.execution_evidence(), evidence, "P1");
                    } else {
                        assert!(matches!(error.classification(), LlmError::Binding(_)), "P2");
                        assert_eq!(error.execution_evidence(), ModelExecutionEvidence::NotDispatched(
                            awaken_runtime_contract::model_operation::NoDispatchProof::request_projection(),
                        ), "P2");
                    }
                    assert_eq!(request.model_binding.identity(), ("p", "alias", "b"));
                }
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    if alias == "upstream" { 2 } else { 0 },
                    "P1/P2 exact effects"
                );
            }
        }
    }

    #[test]
    fn provider_executor_factory_decision_table() {
        // Causes: C1 dialect is present and supported, C2 dialect matches the
        // pinned adapter, C3 adapter is supported, C4 base URL exists, and C5
        // credential exists. Effects: E1 exact construction; E2 every false
        // cause fails closed before provider I/O. Decision rules:
        // R1=C1+C2+C3+C4+C5=>E1; R2=!C1=>UnsupportedDialect; each other single
        // false cause yields its typed error. Empty historical dialect is not an
        // executable compatibility path; it must be migrated at publication.
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
                "",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedDialect(dialect)) if dialect.is_empty()
        ));
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
            Err(ResolvedExecutorError::UnsupportedDialect(dialect)) if dialect.is_empty()
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
        // C6 the immutable publication selected Disabled for DeepSeek Chat;
        // C7 the same policy reaches a Responses adapter that has no faithful
        // projection. E3 Chat accepts the policy for the provider-wire gate;
        // E4 Responses fails before any network I/O. Rules R3=C1..C5+C6=>E3;
        // R4=C1..C5+C7=>E4. The HTTP-byte assertion lives in the genai
        // compatibility suite and prevents an accepted but dropped override.
        assert!(
            executor_from_materialized_endpoint_for_provider(
                "deepseek",
                "open_ai_chat",
                "openai",
                Some("https://provider.invalid"),
                Some(&credential),
                UnspecifiedReasoning::Disabled,
            )
            .is_ok(),
            "R3"
        );
        assert!(
            matches!(
                executor_from_materialized_endpoint_for_provider(
                    "deepseek",
                    "open_ai_responses",
                    "openai",
                    Some("https://provider.invalid"),
                    Some(&credential),
                    UnspecifiedReasoning::Disabled,
                ),
                Err(ResolvedExecutorError::UnsupportedReasoningPolicy(_))
            ),
            "R4"
        );
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

    fn brokered_candidate(model: &str) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_brokered_provider(
            ModelBinding::new("openai", model, "genai"),
            "openai@1",
            "awaken-cloud:openai:open_ai_responses@7",
            "workspace-a",
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_responses".into(),
                base_url: "https://api.awakenworks.com".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent brokered provider candidate")
    }

    #[cfg(not(feature = "brokered"))]
    #[tokio::test]
    async fn brokered_access_fails_closed_when_the_adapter_is_absent() {
        // Cause/effect design: C1 the immutable candidate explicitly requests
        // Brokered access; C2 this binary has no brokered adapter. C1+C2 => E1
        // fail with cloud_models_disabled before direct credential lookup or
        // provider I/O. Route text is deliberately plain, so it has no causal
        // role. Decision rule N1=C1+C2=>E1, with/without authority (N2).
        let error = neutral_materializer()
            .materialize_exact(
                &brokered_candidate("model-a"),
                &RuntimeRunContext::new(),
                None,
            )
            .await
            .err()
            .expect("N1 brokered access without an adapter must fail closed");
        assert!(error.contains("cloud_models_disabled"), "N1/E1");
    }

    #[cfg(feature = "brokered")]
    fn direct_candidate_with_brokered_looking_route(model: &str) -> ResolvedModelCandidate {
        ResolvedModelCandidate::try_provider(
            ModelBinding::new("openai", model, "genai"),
            "openai@1",
            "brokered:misleading-route@1",
            "workspace-a",
            None,
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_responses".into(),
                base_url: "https://provider.invalid".into(),
                upstream_model: model.into(),
                processing_placement: None,
            },
        )
        .expect("coherent direct provider candidate")
    }

    fn neutral_materializer() -> CredentialInferenceMaterializer {
        struct NoCredentialAccess;
        #[async_trait]
        impl awaken_runtime_contract::CredentialMaterialResolver for NoCredentialAccess {
            async fn resolve_exact(
                &self,
                _: awaken_runtime_contract::CredentialMaterialRequest<'_>,
            ) -> Result<
                awaken_runtime_contract::ResolvedCredentialMaterial,
                awaken_runtime_contract::CredentialMaterialError,
            > {
                panic!("brokered inference must not read local or external Provider credentials")
            }
        }
        CredentialInferenceMaterializer::from_pinned(PinnedCredentialMaterializer::external_only(
            Arc::new(NoCredentialAccess),
        ))
    }

    #[test]
    fn pinned_candidate_preserves_qualified_effect_scope_for_direct_and_brokered_access() {
        use awaken_runtime_contract::model_operation::ModelEffectScope;
        use awaken_runtime_contract::resolved::{ProviderServerTool, ToolDescriptor};
        // Causes: C1 exact pinned binding; C2 qualified provider+wire; C3
        // Direct/Brokered placement; C4 provider-hosted tool absent/present.
        // Effects: E1 response-only classification survives either placement;
        // E2 wrong binding/provider or hosted execution stays opaque. Rules:
        // C1+C2+!C4=>E1 for both C3; every missing safety conjunct=>E2.
        // No credential resolution, grant or model call is needed to classify.
        for brokered in [false, true] {
            for provider in ["deepseek", "openai"] {
                let binding = ModelBinding::parse(provider, "model-a", "genai").unwrap();
                let endpoint = awaken_runtime_contract::InferenceEndpoint {
                    adapter_kind: "openai".into(),
                    api_dialect: "open_ai_chat".into(),
                    base_url: "https://gateway.invalid".into(),
                    upstream_model: "model-a".into(),
                    processing_placement: None,
                };
                let reference = format!("{provider}@1");
                let candidate = if brokered {
                    ResolvedModelCandidate::try_brokered_provider(
                        binding.clone(),
                        reference,
                        "route-a",
                        "workspace-a",
                        endpoint,
                    )
                } else {
                    ResolvedModelCandidate::try_provider(
                        binding.clone(),
                        reference,
                        "route-a",
                        "workspace-a",
                        None,
                        endpoint,
                    )
                }
                .unwrap();
                let router = PinnedCandidateExecutor {
                    materializer: neutral_materializer(),
                    candidates: vec![candidate],
                    realization: None,
                    ownership: None,
                    local_run_correlation: None,
                };
                let mut request = ChatRequest {
                    model_binding: binding,
                    inference: Default::default(),
                    messages: Vec::new(),
                    tools: Vec::new(),
                };
                assert_eq!(
                    router.effect_scope(&request),
                    if provider == "deepseek" {
                        ModelEffectScope::ResponseOnly
                    } else {
                        ModelEffectScope::Opaque
                    }
                );
                let mut hosted = ToolDescriptor::pinned(
                    "test",
                    "search",
                    "search",
                    serde_json::json!({"type":"object"}),
                );
                hosted.provider_server_tool = Some(ProviderServerTool::DeepSeekResponsesWebSearch);
                request.tools.push(hosted);
                assert_eq!(router.effect_scope(&request), ModelEffectScope::Opaque);
                request.tools.clear();
                request.model_binding = ModelBinding::parse("foreign", "other", "genai").unwrap();
                assert_eq!(router.effect_scope(&request), ModelEffectScope::Opaque);
            }
        }
    }

    #[cfg(feature = "brokered")]
    #[tokio::test]
    async fn published_reasoning_policy_survives_candidate_materialization() {
        use crate::brokered_inference::{
            BrokeredInferenceClient, BrokeredInferenceError, BrokeredInferenceLease,
            BrokeredInferenceRequest,
        };
        use awaken_runtime_contract::model_operation::{
            ModelExecutionEvidence, ModelOperation, ModelOperationContext, NoDispatchProof,
            with_model_operation_context,
        };

        // C1 the immutable brokered candidate freezes Disabled; C2 its
        // Responses dialect cannot project that policy; C3 a grant succeeds.
        // E1 the same policy reaches the provider factory and fails before
        // Provider I/O; E2 the grant is closed. Rule P1=C1+C2+C3=>E1+E2.
        // This seam catches policy loss in materialization; the genai HTTP
        // compatibility test covers the supported Chat policy on the wire.
        #[derive(Default)]
        struct Grant(std::sync::Mutex<usize>);
        #[async_trait]
        impl BrokeredInferenceClient for Grant {
            async fn create_grant(
                &self,
                _: BrokeredInferenceRequest,
            ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
                Ok(BrokeredInferenceLease {
                    grant_id: "grant-policy".into(),
                    gateway_base_url: "https://gateway.invalid".into(),
                    capability: RedactedString::new("fixture-capability"),
                    grant_expires_at: 1_800_000_000,
                })
            }

            async fn close_grant(&self, _: &str) -> Result<(), BrokeredInferenceError> {
                *self.0.lock().unwrap() += 1;
                Ok(())
            }

            async fn renew_grant(
                &self,
                _: &str,
            ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
                panic!("one request never renews its grant")
            }
        }

        let binding = ModelBinding::parse("deepseek", "deepseek-v4-pro", "genai").unwrap();
        let candidate = ResolvedModelCandidate::try_brokered_provider_with_reasoning(
            binding.clone(),
            "deepseek",
            "deepseek:open_ai_responses@1",
            "workspace-a",
            awaken_runtime_contract::InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_responses".into(),
                base_url: "https://gateway.invalid".into(),
                upstream_model: "deepseek-v4-pro".into(),
                processing_placement: None,
            },
            UnspecifiedReasoning::Disabled,
        )
        .unwrap();
        let grant = Arc::new(Grant::default());
        let executor = neutral_materializer()
            .with_brokered_client(grant.clone())
            .materialize_exact(
                &candidate,
                &RuntimeRunContext::new().with_ownership(Arc::new(CurrentOwnership)),
                Some("run-policy".into()),
            )
            .await
            .unwrap()
            .unwrap();
        let operation = ModelOperationContext::from_operation(
            &awaken_agent_contract::agent::thread::Id("thread-policy".into()),
            &awaken_agent_contract::agent::run::Id("run-policy".into()),
            &ModelOperation::next(None, binding.clone()).unwrap(),
        );
        let error = with_model_operation_context(
            Some(operation),
            executor.infer(ChatRequest {
                model_binding: binding,
                inference: Default::default(),
                messages: Vec::new(),
                tools: Vec::new(),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot enforce the frozen reasoning policy"),
            "P1/E1: {error}"
        );
        assert_eq!(
            error.execution_evidence(),
            ModelExecutionEvidence::NotDispatched(NoDispatchProof::executor_resolution()),
            "P1/E1"
        );
        assert_eq!(*grant.0.lock().unwrap(), 1, "P1/E2");
    }

    #[cfg(feature = "brokered")]
    #[tokio::test]
    async fn pinned_materialization_preserves_journal_identity_in_both_model_modes() {
        use crate::brokered_inference::{
            BrokeredInferenceClient, BrokeredInferenceError, BrokeredInferenceLease,
            BrokeredInferenceRequest,
        };
        use awaken_runtime_contract::model_operation::{
            ModelOperation, ModelOperationContext, with_model_operation_context,
        };
        // P1 each pinned model-port call rematerializes the candidate. P2 both
        // streaming and nonstreaming paths retain the ambient journal scope.
        // P3 a grant response is lost: repeating its same invocation keeps the
        // key; the next recorded invocation must differ. The fake broker records
        // the real grant request then refuses it, so no model/credential IO is
        // hidden in this cross-adapter regression.
        // P4 run the same assertions with/without the authority feature: both
        // must compile and use the neutral composition, with zero secret reads.
        #[derive(Default)]
        struct LostGrant(std::sync::Mutex<Vec<BrokeredInferenceRequest>>);
        #[async_trait]
        impl BrokeredInferenceClient for LostGrant {
            async fn create_grant(
                &self,
                request: BrokeredInferenceRequest,
            ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
                self.0.lock().unwrap().push(request);
                tokio::task::yield_now().await;
                Err(BrokeredInferenceError::TemporarilyUnavailable)
            }
            async fn close_grant(&self, _: &str) -> Result<(), BrokeredInferenceError> {
                panic!("no grant receipt exists")
            }
            async fn renew_grant(
                &self,
                _: &str,
            ) -> Result<BrokeredInferenceLease, BrokeredInferenceError> {
                panic!("no grant receipt exists")
            }
        }
        let client = Arc::new(LostGrant::default());
        let materializer = neutral_materializer().with_brokered_client(client.clone());
        let candidate = brokered_candidate("model-a");
        let router = PinnedCandidateExecutor {
            materializer,
            candidates: vec![candidate.clone()],
            realization: None,
            ownership: Some(Arc::new(CurrentOwnership)),
            local_run_correlation: Some("run-a".into()),
        };
        let request = ChatRequest {
            model_binding: candidate.binding().clone(),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        for ordinal in [1, 1, 2] {
            let mut record = ModelOperation::next(None, candidate.binding().clone()).unwrap();
            record.ordinal = std::num::NonZeroU64::new(ordinal).unwrap();
            let operation = ModelOperationContext::from_operation(
                &awaken_agent_contract::agent::thread::Id("thread-a".into()),
                &awaken_agent_contract::agent::run::Id("run-a".into()),
                &record,
            );
            for streaming in [false, true] {
                let error = with_model_operation_context(Some(operation.clone()), async {
                    if streaming {
                        router
                            .infer_streaming(request.clone(), &crate::NoopDeltaSink)
                            .await
                    } else {
                        router.infer(request.clone()).await
                    }
                })
                .await
                .unwrap_err();
                assert_eq!(
                    error.execution_evidence(),
                    awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution())
                );
            }
        }
        let requests = client.0.lock().unwrap();
        assert_eq!(requests.len(), 6);
        for row in 0..4 {
            assert_eq!(requests[0].idempotency_key, requests[row].idempotency_key);
        }
        assert_ne!(requests[0].idempotency_key, requests[4].idempotency_key);
        assert_eq!(requests[4].idempotency_key, requests[5].idempotency_key);
    }

    #[cfg(feature = "brokered")]
    #[tokio::test]
    async fn brokered_mode_and_candidate_routing_share_one_decision_table() {
        // Causes: C1 brokered supply is enabled; C2 an authenticated client is
        // installed; C3 the requested binding is in the publication-pinned set;
        // C4 the explicit access kind, independently of route text.
        // Effects: E1 return a lazy exact executor without acquiring a grant;
        // E2 report disabled; E3 require sign-in; E4 reject an unpinned binding;
        // E5 a direct candidate remains direct even if its route resembles the
        // retired brokered prefix.
        // Rules: B1=!C1 -> E2; B2=C1&&!C2 -> E3; B3=C1&&C2&&C3 -> E1;
        // B4=C1&&C2&&!C3 -> E4; B5=direct access+prefixed route -> E5.
        // Primary and fallback use plain route identities and exercise B3,
        // proving route text has no access-routing authority.
        // B6 authority present/absent x B1..B5: identical routing through the
        // neutral constructor; an accidental credential read panics instead of
        // succeeding against an otherwise unnecessary empty Vault fixture.
        let materializer = neutral_materializer();
        let primary = brokered_candidate("model-primary");
        let fallback = brokered_candidate("model-fallback");
        let context = RuntimeRunContext::new().with_ownership(Arc::new(CurrentOwnership));

        assert!(
            materializer
                .materialize_exact(
                    &direct_candidate_with_brokered_looking_route("model-direct"),
                    &context,
                    None,
                )
                .await
                .err()
                .expect("anonymous direct route reaches direct materialization")
                .contains("resolved inference carries no credential"),
            "B5/E5"
        );

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
        assert!(
            router.executor_for(primary.binding()).await.is_ok(),
            "B3/E1"
        );
        assert!(
            router.executor_for(fallback.binding()).await.is_ok(),
            "B3/E1 fallback"
        );
        assert!(
            router
                .executor_for(&ModelBinding::new("openai", "outside", "genai"))
                .await
                .is_err(),
            "B4/E4"
        );
        // B4 at the actual model port is known pre-dispatch in both modes;
        // NeverGrantClient would panic if candidate validation leaked I/O.
        let request = ChatRequest {
            model_binding: ModelBinding::new("openai", "outside", "genai"),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        for streaming in [false, true] {
            let error = if streaming {
                router
                    .infer_streaming(request.clone(), &crate::NoopDeltaSink)
                    .await
            } else {
                router.infer(request.clone()).await
            }
            .unwrap_err();
            assert_eq!(
                error.execution_evidence(),
                awaken_runtime_contract::model_operation::ModelExecutionEvidence::NotDispatched(
                    awaken_runtime_contract::model_operation::NoDispatchProof::executor_resolution(
                    )
                )
            );
        }
    }
}
