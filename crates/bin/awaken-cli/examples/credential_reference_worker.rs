//! E2E fixture: a worker that materializes one opaque credential reference into
//! an executor. Endpoint selection is already fixed; only credential injection is
//! exercised, independent of deployment topology.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::inference::InferenceExecutorMaterializer;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult,
};

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
    state_file: Option<std::path::PathBuf>,
}

impl ReferenceMaterializer {
    fn state(&self) -> String {
        self.state_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|state| state.trim().to_string())
            .unwrap_or_else(|| "available".to_string())
    }
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
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
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

#[async_trait]
impl awaken_runtime_contract::CredentialObservationSource for ReferenceMaterializer {
    async fn credential_observations(
        &self,
    ) -> Result<
        BTreeSet<awaken_runtime_contract::CredentialObservation>,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        let state = self.state();
        if state == "probe_failed" {
            return Err(awaken_runtime_contract::CredentialMaterialError::ProbeFailed);
        }
        let state = match state.as_str() {
            "available" | "available_then_login_required" => {
                awaken_runtime_contract::CredentialObservationState::Available
            }
            "login_required" => awaken_runtime_contract::CredentialObservationState::LoginRequired,
            "expired" => awaken_runtime_contract::CredentialObservationState::Expired,
            "disabled" => awaken_runtime_contract::CredentialObservationState::Disabled,
            "probe_failed_one" => awaken_runtime_contract::CredentialObservationState::ProbeFailed,
            _ => awaken_runtime_contract::CredentialObservationState::Invalid,
        };
        Ok(BTreeSet::from([
            awaken_runtime_contract::CredentialObservation {
                credential: self.credential.clone(),
                state,
                observed_at_ms: 1,
                reason_code: (state
                    != awaken_runtime_contract::CredentialObservationState::Available)
                    .then(|| format!("fixture_{state:?}")),
            },
        ]))
    }
}

#[async_trait]
impl awaken_runtime_contract::WorkerLocalReferenceRevalidator for ReferenceMaterializer {
    async fn revalidate_worker_reference(
        &self,
        credential: &awaken_runtime_contract::CredentialRef,
    ) -> Result<
        awaken_runtime_contract::CredentialObservation,
        awaken_runtime_contract::CredentialMaterialError,
    > {
        if credential != &self.credential {
            return Err(awaken_runtime_contract::CredentialMaterialError::RevisionMismatch);
        }
        if self.state() == "available_then_login_required" {
            return Err(awaken_runtime_contract::CredentialMaterialError::LoginRequired);
        }
        let observation =
            awaken_runtime_contract::CredentialObservationSource::credential_observations(self)
                .await?
                .into_iter()
                .next()
                .ok_or(awaken_runtime_contract::CredentialMaterialError::Unavailable)?;
        if observation.state == awaken_runtime_contract::CredentialObservationState::Available {
            Ok(observation)
        } else {
            Err(awaken_runtime_contract::CredentialMaterialError::Unavailable)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init(&Default::default());
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let credential_id = std::env::var("AWAKEN_TEST_CREDENTIAL_ID")?;
    let credential_revision = std::env::var("AWAKEN_TEST_CREDENTIAL_REVISION")?.parse()?;
    let materializer = Arc::new(ReferenceMaterializer {
        credential: awaken_runtime_contract::CredentialRef {
            id: credential_id,
            revision: credential_revision,
        },
        state_file: std::env::var_os("AWAKEN_TEST_CREDENTIAL_STATE_FILE")
            .map(std::path::PathBuf::from),
    });
    let resource = std::env::var("AWAKEN_TEST_RESOURCE_DATABASE_URL").ok();
    let admin = std::env::var("AWAKEN_TEST_ADMIN_DATABASE_URL").ok();
    let storage = std::env::var("AWAKEN_TEST_WORKER_STORAGE_DIR").ok();
    match (resource, admin, storage) {
        (None, None, None) => {
            let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
            if let Ok(tier) = std::env::var("AWAKEN_TEST_SANDBOX_TIER") {
                deployment.sandbox_tier = match tier.as_str() {
                    "local" => awaken_runtime_host::SandboxTier::Local,
                    _ => return Err(format!("unsupported E2E sandbox tier `{tier}`").into()),
                };
            }
            let credentials =
                awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                    materializer.clone(),
                );
            let mut builder = awaken_worker::WorkerNodeBuilder::new(
                awaken_worker_transport_security::WorkerUpstream::new(upstream),
            )
            .with_deployment_config(deployment)
            .with_inference_materializer(materializer.clone())
            .with_credential_materializer(credentials)
            .with_worker_local_credential_resolver(materializer)
            .with_standard_manifest(Default::default());
            if let Ok(address) = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN") {
                builder = builder.with_admin_listen(address);
            }
            builder.build()?.run_until_shutdown().await
        }
        (Some(_resource_url), Some(_admin_url), Some(storage_dir)) => {
            let mut deployment = awaken_runtime_host::DeploymentConfig::ephemeral();
            deployment.durable = true;
            deployment.storage_dir = Some(storage_dir.into());
            let credentials =
                awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                    materializer.clone(),
                );
            // Worker identity decision table: an explicit E2E identity owns its
            // own registry slot; an absent identity retains the production
            // adapter default. Ignoring the explicit identity would alias an
            // AllInOne worker and leave eligible dispatches permanently pending.
            let mut upstream = awaken_worker_transport_security::WorkerUpstream::new(upstream);
            if let Ok(worker_id) = std::env::var("AWAKEN_WORKER_ID") {
                upstream = upstream.with_worker_id(worker_id);
            }
            let mut builder = awaken_worker::WorkerNodeBuilder::new(upstream)
                .with_deployment_config(deployment)
                .with_registered_memory_mounter_factory(
                    awaken_cli::registered_memory_mounter_factory(),
                )
                .with_inference_materializer(materializer.clone())
                .with_credential_materializer(credentials)
                .with_worker_local_credential_resolver(materializer)
                .with_standard_manifest(Default::default());
            if let Ok(address) = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN") {
                builder = builder.with_admin_listen(address);
            }
            builder.build()?.run_until_shutdown().await
        }
        _ => Err("resource Worker fixture requires resource, admin, and storage together".into()),
    }
}
