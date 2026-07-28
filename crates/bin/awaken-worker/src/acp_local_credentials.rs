//! Worker-local credential liveness for discovered ACP agents.
//!
//! This adapter belongs to the Worker composition boundary: it projects
//! secret-free ACP discovery observations onto the existing credential
//! liveness port. It never opens, returns, or materializes CLI credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_credential_vault::{CredentialKind, CredentialSource, CredentialStatus};
use awaken_run_executor_acp::{
    AcpCli, AcpDetectionState, AcpDiscovery, AcpHostObservation, acp_cli,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
    CredentialObservation, CredentialObservationState, CredentialRef, ResolvedCredentialMaterial,
};

#[derive(Clone)]
struct LocalBinding {
    credential: CredentialRef,
    cli: &'static AcpCli,
    status: CredentialStatus,
}

/// One resolver for all `acp:*` WorkerLocal sources installed on this Worker.
pub struct AcpLocalCredentialResolver {
    discovery: Arc<dyn AcpDiscovery>,
    bindings: BTreeMap<String, LocalBinding>,
}

impl AcpLocalCredentialResolver {
    /// Adapt already-persisted secret-free sources. Non-WorkerLocal sources and
    /// WorkerLocal drivers outside the `acp:*` bounded context are ignored.
    pub fn from_sources(
        discovery: Arc<dyn AcpDiscovery>,
        sources: impl IntoIterator<Item = CredentialSource>,
    ) -> Result<Self, String> {
        let mut bindings = BTreeMap::new();
        for source in sources {
            if source.kind != CredentialKind::WorkerLocal {
                continue;
            }
            let binding = source.worker_local_binding.as_ref().ok_or_else(|| {
                format!("worker-local source {} has no stable binding", source.id.0)
            })?;
            let Backend::Acp { cli } = Backend::from_ref(&binding.driver_id) else {
                continue;
            };
            let cli = acp_cli(&cli)
                .ok_or_else(|| format!("worker-local source names unknown ACP CLI `{cli}`"))?;
            let revision = u64::try_from(source.version)
                .ok()
                .filter(|revision| *revision > 0)
                .ok_or_else(|| {
                    format!("worker-local source {} has invalid revision", source.id.0)
                })?;
            let credential = CredentialRef {
                id: source.id.0,
                revision,
            };
            if bindings
                .insert(
                    credential.id.clone(),
                    LocalBinding {
                        credential,
                        cli,
                        status: source.status,
                    },
                )
                .is_some()
            {
                return Err("duplicate WorkerLocal credential id".to_string());
            }
        }
        Ok(Self {
            discovery,
            bindings,
        })
    }

    async fn observe(&self, binding: &LocalBinding) -> CredentialObservation {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        if binding.status != CredentialStatus::Active {
            return CredentialObservation {
                credential: binding.credential.clone(),
                state: CredentialObservationState::Disabled,
                observed_at_ms: now,
                reason_code: Some("worker_local_source_disabled".to_string()),
            };
        }
        let host = self.discovery.discover(binding.cli).await;
        let (state, reason_code) = classify_host_observation(&host);
        CredentialObservation {
            credential: binding.credential.clone(),
            state,
            observed_at_ms: now,
            reason_code,
        }
    }
}

fn classify_host_observation(
    observation: &AcpHostObservation,
) -> (CredentialObservationState, Option<String>) {
    match observation.detection {
        AcpDetectionState::Detected => (
            observation
                .credential_state
                .unwrap_or(CredentialObservationState::ProbeFailed),
            observation.reason_code.clone(),
        ),
        AcpDetectionState::Missing => (
            CredentialObservationState::Invalid,
            observation
                .reason_code
                .clone()
                .or_else(|| Some("acp_agent_missing".to_string())),
        ),
        AcpDetectionState::ProbeFailed => (
            CredentialObservationState::ProbeFailed,
            observation
                .reason_code
                .clone()
                .or_else(|| Some("acp_discovery_probe_failed".to_string())),
        ),
    }
}

fn require_available(
    observation: CredentialObservation,
) -> Result<CredentialObservation, CredentialMaterialError> {
    match observation.state {
        CredentialObservationState::Available => Ok(observation),
        CredentialObservationState::LoginRequired => Err(CredentialMaterialError::LoginRequired),
        CredentialObservationState::Expired => Err(CredentialMaterialError::Expired),
        CredentialObservationState::Invalid => Err(CredentialMaterialError::Invalid),
        CredentialObservationState::Disabled => Err(CredentialMaterialError::Disabled),
        CredentialObservationState::ProbeFailed => Err(CredentialMaterialError::ProbeFailed),
    }
}

#[async_trait]
impl CredentialMaterialResolver for AcpLocalCredentialResolver {
    async fn credential_observations(
        &self,
    ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
        let mut observations = BTreeSet::new();
        for binding in self.bindings.values() {
            observations.insert(self.observe(binding).await);
        }
        Ok(observations)
    }

    async fn revalidate_worker_reference(
        &self,
        credential: &CredentialRef,
    ) -> Result<CredentialObservation, CredentialMaterialError> {
        let binding = self
            .bindings
            .get(&credential.id)
            .filter(|binding| binding.credential.revision == credential.revision)
            .ok_or(CredentialMaterialError::Unavailable)?;
        require_available(self.observe(binding).await)
    }

    async fn resolve_exact(
        &self,
        _request: CredentialMaterialRequest<'_>,
    ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
        Err(CredentialMaterialError::MaterialKindMismatch)
    }
}
