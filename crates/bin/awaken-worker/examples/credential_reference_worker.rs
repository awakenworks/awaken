//! E2E fixture: a worker that materializes one opaque credential reference into
//! an executor. Endpoint selection is already fixed; only credential injection is
//! exercised, independent of deployment topology.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};
use awaken_server::InferenceExecutorMaterializer;

struct GrantExecutor {
    reference: String,
}

#[async_trait]
impl LlmExecutor for GrantExecutor {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("credential-reference:{}", self.reference)),
            usage: None,
            stop_reason: None,
        })
    }
}

struct ReferenceMaterializer {
    credential: awaken_runtime_contract::CredentialRef,
}

impl InferenceExecutorMaterializer for ReferenceMaterializer {
    fn supported_access_schemes(&self) -> &'static [&'static str] {
        &[awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY]
    }

    fn credential_realization_capabilities(
        &self,
    ) -> awaken_runtime_contract::CredentialRealizationCapabilities {
        awaken_runtime_contract::CredentialRealizationCapabilities {
            holders: BTreeSet::from([awaken_runtime_contract::PlaintextHolder::new(
                awaken_runtime_contract::PlaintextBoundary::Worker,
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]),
            material_sources: BTreeSet::from([
                awaken_runtime_contract::CredentialMaterialSource::WorkerReference,
            ]),
            realization_kinds: BTreeSet::from([
                awaken_runtime_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]),
            recipient_bound_envelopes: false,
        }
    }

    fn materialize_pinned(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        _context: &awaken_runtime_contract::RuntimeRunContext,
    ) -> Option<Arc<dyn LlmExecutor>> {
        let awaken_runtime_contract::resolved::ModelProvisioning::Provider {
            credential: Some(credential),
            ..
        } = &candidate.provisioning
        else {
            return None;
        };
        if credential.material_source
            != awaken_runtime_contract::CredentialMaterialSource::WorkerReference
            || credential.credential != self.credential
        {
            return None;
        }
        Some({
            Arc::new(GrantExecutor {
                reference: credential.credential.id.clone(),
            }) as Arc<dyn LlmExecutor>
        })
    }
}

#[async_trait]
impl awaken_runtime_contract::CredentialMaterialResolver for ReferenceMaterializer {
    fn supported_material_sources(
        &self,
    ) -> BTreeSet<awaken_runtime_contract::CredentialMaterialSource> {
        BTreeSet::from([awaken_runtime_contract::CredentialMaterialSource::WorkerReference])
    }

    async fn credential_observations(
        &self,
    ) -> Result<
        BTreeSet<awaken_runtime_contract::CredentialObservation>,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        Ok(BTreeSet::from([
            awaken_runtime_contract::CredentialObservation::available(self.credential.clone(), 1),
        ]))
    }

    async fn resolve_exact(
        &self,
        _request: awaken_runtime_contract::CredentialMaterialRequest<'_>,
    ) -> Result<
        awaken_runtime_contract::ResolvedCredentialMaterial,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        Err(awaken_runtime_contract::CredentialMaterialError::Unavailable)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init();
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let credential_id = std::env::var("AWAKEN_TEST_CREDENTIAL_ID")?;
    let credential_revision = std::env::var("AWAKEN_TEST_CREDENTIAL_REVISION")?.parse()?;
    let materializer = Arc::new(ReferenceMaterializer {
        credential: awaken_runtime_contract::CredentialRef {
            id: credential_id,
            revision: credential_revision,
        },
    });
    awaken_worker::run_with_inference_and_credential_resolver(
        &upstream,
        materializer.clone(),
        materializer,
    )
    .await
}
