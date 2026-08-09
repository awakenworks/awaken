//! Worker-side realization of publication-pinned Native provider candidates.

use std::sync::Arc;

use async_trait::async_trait;
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

#[derive(Clone)]
pub struct DirectInferenceMaterializer {
    credentials: PinnedCredentialMaterializer,
}

impl DirectInferenceMaterializer {
    #[must_use]
    pub fn new(credentials: PinnedCredentialMaterializer) -> Self {
        Self { credentials }
    }

    pub async fn materialize_candidate(
        &self,
        candidate: &ResolvedModelCandidate,
        context: &RuntimeRunContext,
    ) -> Result<Option<Arc<dyn LlmExecutor>>, String> {
        let ModelProvisioning::Provider { endpoint, .. } = &candidate.provisioning else {
            return Ok(None);
        };
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
    provider: DirectInferenceMaterializer,
    candidates: Vec<ResolvedModelCandidate>,
    realization: Option<awaken_runtime_contract::AttemptCredentialRealization>,
    ownership: Option<Arc<dyn awaken_runtime_contract::AttemptOwnershipVerifier>>,
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
        self.provider
            .materialize_candidate(
                candidate,
                &RuntimeRunContext {
                    credential_realization: self.realization.clone(),
                    ownership: self.ownership.clone(),
                    ..RuntimeRunContext::new()
                },
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

impl InferenceExecutorMaterializer for DirectInferenceMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
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
            provider: self.clone(),
            candidates,
            realization: context.credential_realization.clone(),
            ownership: context.ownership.clone(),
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
            provider: self.clone(),
            candidates: vec![candidate.clone()],
            realization: context.credential_realization.clone(),
            ownership: context.ownership.clone(),
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

    /// Cause/effect graph: C1 dialect is supported, C2 dialect matches adapter,
    /// C3 adapter is supported, C4 base URL exists, C5 credential exists.
    /// Effects are E1 exact executor construction or one fail-closed typed error.
    /// Decision rules exercised here: all causes true -> E1; !C1 -> unsupported
    /// dialect; C1+!C2 -> mismatch; C1+C2+!C3 -> unsupported adapter; missing C4
    /// or C5 -> the corresponding missing-material error. These rules cover the
    /// canonical factory; composition crates deliberately own no parallel map.
    #[test]
    fn provider_executor_factory_decision_table() {
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
                "supported {dialect}/{adapter} must construct"
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
                "cohere",
                Some("https://provider.invalid"),
                Some(&credential),
            ),
            Err(ResolvedExecutorError::UnsupportedAdapter(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint("", "openai", None, Some(&credential)),
            Err(ResolvedExecutorError::MissingBaseUrl(_))
        ));
        assert!(matches!(
            executor_from_materialized_endpoint(
                "",
                "openai",
                Some("https://provider.invalid"),
                None,
            ),
            Err(ResolvedExecutorError::MissingCredential)
        ));
    }
}
