//! Composite Worker-local credential resolver for every discovered ACP agent.
//!
//! It adapts the canonical ACP discovery observations to the existing credential
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_credential_vault::WorkerLocalBinding;
    use awaken_credential_vault::repo::{InMemoryCredentialRepo, ensure_worker_local};
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialBinding,
        CredentialMaterialSource, CredentialUsage, ModelExposurePolicy, PlaintextBoundary,
        PlaintextHolder,
    };

    use super::*;

    struct FixedDiscovery {
        observations: BTreeMap<String, AcpHostObservation>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AcpDiscovery for FixedDiscovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            self.calls.lock().unwrap().push(cli.id.to_string());
            self.observations.get(cli.id).cloned().unwrap()
        }
    }

    fn host(
        id: &str,
        detection: AcpDetectionState,
        credential_state: Option<CredentialObservationState>,
    ) -> AcpHostObservation {
        AcpHostObservation {
            cli_id: id.to_string(),
            display_name: id.to_string(),
            detection,
            version: Some("1".into()),
            credential_state,
            reason_code: Some(format!("fixture_{id}")),
        }
    }

    async fn source(repo: &InMemoryCredentialRepo, cli: &str, subject: &str) -> CredentialSource {
        ensure_worker_local(
            repo,
            "ws",
            WorkerLocalBinding::new(format!("acp:{cli}"), subject),
            None,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn discovery_states_project_into_the_existing_worker_liveness_states() {
        // Cause graph: exact WorkerLocal binding -> one profile observation ->
        // existing CredentialObservationState; no credential material edge exists.
        //
        // Decision table:
        // D1 detected/available       -> Available
        // D2 detected/login required  -> LoginRequired
        // D3 missing                  -> Invalid
        // D4 discovery failed         -> ProbeFailed
        // D5 disabled source          -> Disabled, no process probe
        let repo = InMemoryCredentialRepo::new();
        let codex = source(&repo, "codex", "default").await;
        let claude = source(&repo, "claude", "default").await;
        let gemini = source(&repo, "gemini", "default").await;
        let opencode = source(&repo, "opencode", "default").await;
        let mut disabled = source(&repo, "codex", "disabled").await;
        disabled.status = CredentialStatus::Disabled;

        let discovery = Arc::new(FixedDiscovery {
            observations: BTreeMap::from([
                (
                    "codex".into(),
                    host(
                        "codex",
                        AcpDetectionState::Detected,
                        Some(CredentialObservationState::Available),
                    ),
                ),
                (
                    "claude".into(),
                    host(
                        "claude",
                        AcpDetectionState::Detected,
                        Some(CredentialObservationState::LoginRequired),
                    ),
                ),
                (
                    "gemini".into(),
                    host("gemini", AcpDetectionState::Missing, None),
                ),
                (
                    "opencode".into(),
                    host("opencode", AcpDetectionState::ProbeFailed, None),
                ),
            ]),
            calls: Mutex::new(Vec::new()),
        });
        let resolver = AcpLocalCredentialResolver::from_sources(
            discovery.clone(),
            [codex, claude, gemini, opencode, disabled],
        )
        .unwrap();
        let observations = resolver.credential_observations().await.unwrap();
        let states: BTreeSet<_> = observations
            .iter()
            .map(|observation| observation.state)
            .collect();
        assert_eq!(
            states,
            BTreeSet::from([
                CredentialObservationState::Available,
                CredentialObservationState::LoginRequired,
                CredentialObservationState::Invalid,
                CredentialObservationState::Disabled,
                CredentialObservationState::ProbeFailed,
            ])
        );
        assert_eq!(discovery.calls.lock().unwrap().len(), 4, "D5");
    }

    #[tokio::test]
    async fn exact_revalidation_probes_only_the_pinned_cli_and_never_resolves_material() {
        // Cause graph: exact id+revision -> exact CLI liveness probe; stale pin
        // stops before I/O; a material request has no outgoing plaintext edge.
        //
        // Decision table:
        // R1 exact + available -> Available, one profile probe
        // R2 stale revision    -> Unavailable, no additional probe
        // R3 material request  -> MaterialKindMismatch
        let repo = InMemoryCredentialRepo::new();
        let codex = source(&repo, "codex", "default").await;
        let credential = CredentialRef {
            id: codex.id.0.clone(),
            revision: codex.version as u64,
        };
        let discovery = Arc::new(FixedDiscovery {
            observations: BTreeMap::from([(
                "codex".into(),
                host(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::Available),
                ),
            )]),
            calls: Mutex::new(Vec::new()),
        });
        let resolver =
            AcpLocalCredentialResolver::from_sources(discovery.clone(), [codex]).unwrap();

        assert!(
            resolver
                .revalidate_worker_reference(&credential)
                .await
                .is_ok()
        );
        assert_eq!(&*discovery.calls.lock().unwrap(), &["codex"]);
        let mut stale = credential.clone();
        stale.revision += 1;
        assert_eq!(
            resolver.revalidate_worker_reference(&stale).await,
            Err(CredentialMaterialError::Unavailable)
        );
        assert_eq!(&*discovery.calls.lock().unwrap(), &["codex"]);

        let holder = PlaintextHolder::new(PlaintextBoundary::Workload, "self-hosted-acp");
        let access = CredentialAccess::new(
            credential,
            CredentialMaterialSource::WorkerReference,
            CredentialUsage::EnvironmentVariable {
                name: "NEVER_INJECTED".into(),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
        );
        let binding = CredentialMaterialBinding::for_target("ws", &"acp:codex", &access.usage);
        assert!(
            matches!(
                resolver
                    .resolve_exact(CredentialMaterialRequest {
                        access: &access,
                        selected_holder: &holder,
                        binding: &binding,
                    })
                    .await,
                Err(CredentialMaterialError::MaterialKindMismatch)
            ),
            "R3"
        );
        assert!(resolver.supported_material_sources().is_empty(), "R3");
    }

    #[tokio::test]
    async fn constructor_is_generic_but_fails_closed_for_invalid_acp_bindings() {
        let repo = InMemoryCredentialRepo::new();
        let mut missing = source(&repo, "codex", "default").await;
        missing.worker_local_binding = None;
        let discovery: Arc<dyn AcpDiscovery> = Arc::new(FixedDiscovery {
            observations: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
        });
        assert!(AcpLocalCredentialResolver::from_sources(discovery.clone(), [missing]).is_err());

        let unrelated = ensure_worker_local(
            &repo,
            "ws",
            WorkerLocalBinding::new("git:ssh", "default"),
            None,
        )
        .await
        .unwrap();
        let resolver = AcpLocalCredentialResolver::from_sources(discovery, [unrelated]).unwrap();
        assert!(resolver.credential_observations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn existing_worker_builder_installs_the_composite_resolver_once() {
        // Cause graph: canonical credential stores + the one external resolver
        // seam -> WorkerNode -> heartbeat observations and launch-time resolver.
        //
        // Decision table:
        // B1 stores + ACP resolver -> build succeeds, Available is observable
        // B2 liveness-only resolver -> no WorkerReference material capability
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let source = source(&credentials, "codex", "default").await;
        let discovery = Arc::new(FixedDiscovery {
            observations: BTreeMap::from([(
                "codex".into(),
                host(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::Available),
                ),
            )]),
            calls: Mutex::new(Vec::new()),
        });
        let resolver =
            Arc::new(AcpLocalCredentialResolver::from_sources(discovery, [source]).unwrap());
        assert_eq!(
            resolver
                .credential_observations()
                .await
                .expect("B1 observation")
                .iter()
                .next()
                .map(|observation| observation.state),
            Some(CredentialObservationState::Available),
            "B1"
        );
        let worker = awaken_worker::WorkerNodeBuilder::new(
            awaken_runtime_host::WorkerUpstream::new("http://control"),
        )
        .with_credential_stores(
            credentials,
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        )
        .with_external_credential_resolver(resolver.clone())
        .with_standard_manifest(Default::default())
        .build()
        .expect("B1");

        let evidence =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &worker.manifest().capabilities,
            )
            .expect("B2 capability evidence");
        assert!(
            !evidence
                .material_sources
                .contains(&CredentialMaterialSource::WorkerReference),
            "B2"
        );
    }
}
