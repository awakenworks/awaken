//! E2E fixture: a worker that materializes one opaque credential reference into
//! an executor. Endpoint selection is already fixed; only credential injection is
//! exercised, independent of deployment topology.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_credential_materializer::InferenceExecutorMaterializer;
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
    credential: awaken_agent_contract::credential::CredentialRef,
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
    ) -> awaken_credential_contract::CredentialRealizationCapabilities {
        awaken_credential_contract::CredentialRealizationCapabilities {
            holders: BTreeSet::from([awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN,
            )]),
            material_sources: BTreeSet::from([
                awaken_agent_contract::credential::CredentialMaterialSource::WorkerReference,
            ]),
            realization_kinds: BTreeSet::from([
                awaken_credential_contract::CredentialRealizationKind::WorkerProviderAdapter,
            ]),
            recipient_bound_envelopes: false,
            envelope_required: false,
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
        } = candidate.provisioning()
        else {
            return None;
        };
        if credential.material_source
            != awaken_agent_contract::credential::CredentialMaterialSource::WorkerReference
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
impl awaken_credential_contract::CredentialMaterialResolver for ReferenceMaterializer {
    fn material_capabilities(
        &self,
    ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability> {
        let mut profiles = std::collections::BTreeSet::new();
        for holder in [
            awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Worker,
                "awaken.worker",
            ),
            awaken_agent_contract::credential::PlaintextHolder::new(
                awaken_agent_contract::credential::PlaintextBoundary::Workload,
                "awaken.workload.acp",
            ),
        ] {
            profiles.insert(awaken_credential_contract::CredentialMaterialCapability {
                holder,
                source:
                    awaken_agent_contract::credential::CredentialMaterialSource::WorkerReference,
                envelope: false,
            });
        }
        profiles
    }

    async fn resolve_exact(
        &self,
        _request: awaken_credential_contract::CredentialMaterialRequest<'_>,
    ) -> Result<
        awaken_credential_contract::ResolvedCredentialMaterial,
        awaken_credential_contract::CredentialMaterialError,
    > {
        Err(awaken_credential_contract::CredentialMaterialError::Unavailable)
    }
}

#[async_trait]
impl awaken_credential_contract::CredentialObservationSource for ReferenceMaterializer {
    async fn credential_observations(
        &self,
    ) -> Result<
        BTreeSet<awaken_credential_contract::CredentialObservation>,
        awaken_credential_contract::CredentialMaterialError,
    > {
        let state = self.state();
        if state == "probe_failed" {
            return Err(awaken_credential_contract::CredentialMaterialError::ProbeFailed);
        }
        let state = match state.as_str() {
            "available" | "available_then_login_required" => {
                awaken_credential_contract::CredentialObservationState::Available
            }
            "login_required" => {
                awaken_credential_contract::CredentialObservationState::LoginRequired
            }
            "expired" => awaken_credential_contract::CredentialObservationState::Expired,
            "disabled" => awaken_credential_contract::CredentialObservationState::Disabled,
            "probe_failed_one" => {
                awaken_credential_contract::CredentialObservationState::ProbeFailed
            }
            _ => awaken_credential_contract::CredentialObservationState::Invalid,
        };
        Ok(BTreeSet::from([
            awaken_credential_contract::CredentialObservation {
                credential: self.credential.clone(),
                state,
                observed_at_ms: 1,
                reason_code: (state
                    != awaken_credential_contract::CredentialObservationState::Available)
                    .then(|| format!("fixture_{state:?}")),
            },
        ]))
    }
}

#[async_trait]
impl awaken_credential_contract::WorkerLocalReferenceRevalidator for ReferenceMaterializer {
    async fn revalidate_worker_reference(
        &self,
        credential: &awaken_agent_contract::credential::CredentialRef,
    ) -> Result<
        awaken_credential_contract::CredentialObservation,
        awaken_credential_contract::CredentialMaterialError,
    > {
        if credential != &self.credential {
            return Err(awaken_credential_contract::CredentialMaterialError::RevisionMismatch);
        }
        if self.state() == "available_then_login_required" {
            return Err(awaken_credential_contract::CredentialMaterialError::LoginRequired);
        }
        let observation =
            awaken_credential_contract::CredentialObservationSource::credential_observations(self)
                .await?
                .into_iter()
                .next()
                .ok_or(awaken_credential_contract::CredentialMaterialError::Unavailable)?;
        if observation.state == awaken_credential_contract::CredentialObservationState::Available {
            Ok(observation)
        } else {
            Err(awaken_credential_contract::CredentialMaterialError::Unavailable)
        }
    }
}

fn fixture_deployment() -> Result<awaken_session_executor::DeploymentConfig, String> {
    let mut deployment = awaken_session_executor::DeploymentConfig::ephemeral();
    if let Ok(tier) = std::env::var("AWAKEN_TEST_SANDBOX_TIER") {
        deployment.sandbox_tier = match tier.as_str() {
            "local" => awaken_session_executor::SandboxTier::Local,
            #[cfg(feature = "container-docker")]
            "docker" => awaken_session_executor::SandboxTier::Docker,
            _ => return Err(format!("unsupported E2E sandbox tier `{tier}`")),
        };
    }
    deployment.container_image = std::env::var("AWAKEN_TEST_CONTAINER_IMAGE").ok();
    Ok(deployment)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    awaken_observability::init(&Default::default());
    let upstream = std::env::var("AWAKEN_UPSTREAM_URL")?;
    let credential_id = std::env::var("AWAKEN_TEST_CREDENTIAL_ID")?;
    let credential_revision = std::env::var("AWAKEN_TEST_CREDENTIAL_REVISION")?.parse()?;
    let materializer = Arc::new(ReferenceMaterializer {
        credential: awaken_agent_contract::credential::CredentialRef {
            id: credential_id,
            revision: credential_revision,
        },
        state_file: std::env::var_os("AWAKEN_TEST_CREDENTIAL_STATE_FILE")
            .map(std::path::PathBuf::from),
    });
    let resource = std::env::var("AWAKEN_TEST_RESOURCE_DATABASE_URL").ok();
    let admin = std::env::var("AWAKEN_TEST_ADMIN_DATABASE_URL").ok();
    let storage = std::env::var("AWAKEN_TEST_WORKER_STORAGE_DIR").ok();
    // This example process's one wall clock.
    let clock = awaken_time::system_clock();
    match (resource, admin, storage) {
        (None, None, None) => {
            let deployment = fixture_deployment()?;
            let credentials =
                awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                    materializer.clone(),
                    clock.clone(),
                );
            let mut builder = awaken_worker::WorkerNodeBuilder::new(
                awaken_worker_transport_security::WorkerUpstream::new(upstream),
                clock,
            )
            .with_deployment_config(deployment)
            .with_standard_manifest_config(
                awaken_worker::StandardManifestConfig::default().with_max_concurrent(3),
            )
            .with_hand_executor_factory(awaken_worker::relay_hand_executor_factory())
            .with_inference_materializer(materializer.clone())
            .with_credential_materializer(credentials)
            .with_worker_local_credential_resolver(materializer)
            .with_standard_manifest(Default::default());
            if let Ok(address) = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN") {
                builder = builder.with_admin_listen(address);
            }
            builder
                .prepare_session_environment_from_deployment()
                .await?
                .build()?
                .run_until_shutdown()
                .await
        }
        (Some(_resource_url), Some(_admin_url), Some(storage_dir)) => {
            let mut deployment = fixture_deployment()?;
            deployment.durable = true;
            deployment.storage_dir = Some(storage_dir.into());
            let credentials =
                awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                    materializer.clone(),
                    clock.clone(),
                );
            // Worker identity decision table: an explicit E2E identity owns its
            // own registry slot; an absent identity retains the production
            // adapter default. Ignoring the explicit identity would alias an
            // AllInOne worker and leave eligible dispatches permanently pending.
            let mut upstream = awaken_worker_transport_security::WorkerUpstream::new(upstream);
            if let Ok(worker_id) = std::env::var("AWAKEN_WORKER_ID") {
                upstream = upstream.with_worker_id(worker_id);
            }
            let mut builder = awaken_worker::WorkerNodeBuilder::new(upstream, clock)
                .with_deployment_config(deployment)
                .with_standard_manifest_config(
                    awaken_worker::StandardManifestConfig::default().with_max_concurrent(3),
                )
                .with_hand_executor_factory(awaken_worker::relay_hand_executor_factory())
                .with_remote_session_resource_support()
                .with_inference_materializer(materializer.clone())
                .with_credential_materializer(credentials)
                .with_worker_local_credential_resolver(materializer)
                .with_standard_manifest(Default::default());
            if let Ok(address) = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN") {
                builder = builder.with_admin_listen(address);
            }
            builder
                .prepare_session_environment_from_deployment()
                .await?
                .build()?
                .run_until_shutdown()
                .await
        }
        _ => Err("resource Worker fixture requires resource, admin, and storage together".into()),
    }
}
