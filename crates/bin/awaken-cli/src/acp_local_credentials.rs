//! Composite Worker-local credential resolver for every discovered ACP agent.
//!
//! It adapts the canonical ACP discovery observations to the existing credential
//! liveness port. It never opens, returns, or materializes CLI credentials.

use std::collections::BTreeSet;
use std::sync::Arc;

pub use awaken_acp_application::AcpLocalCredentialResolver;
use awaken_acp_application::{
    AcpCapabilityNegotiator, AcpDiscovery, AcpHostDiscovery, AcpHostObservation,
    AcpWrapperInstaller, HostAcpCapabilityNegotiator, LocalAcpPreparation, NpmWrapperInstaller,
    prepare_host_acp_with,
};
use awaken_run_executor_acp::acp_cli;
use awaken_runtime_contract::CredentialObservationState;

/// One startup composition result for a trusted local ACP Worker. The durable
/// sources remain in the credential repository; this value carries only the
/// already-composed ports needed to build the existing WorkerNode.
pub struct PreparedLocalAcp {
    resolver: Arc<AcpLocalCredentialResolver>,
    stores: awaken_control::InferenceMaterializationStores,
    resources: Option<awaken_worker::WorkerResourcePlane>,
}

fn uses_trusted_local_identity(deployment: &crate::config::ResolvedDeployment) -> bool {
    deployment.mode == crate::config::OperatingMode::Local
}

fn uses_host_acp_discovery(deployment: &crate::config::ResolvedDeployment) -> bool {
    uses_trusted_local_identity(deployment)
        && matches!(
            deployment.runtime.sandbox_tier,
            awaken_runtime_host::SandboxTier::Local | awaken_runtime_host::SandboxTier::Namespace
        )
}

/// Discover local ACP agents once, register their secret-free WorkerLocal
/// locators idempotently, and compose the one liveness resolver. Server mode
/// deliberately does none of this.
pub async fn prepare_local_acp(
    deployment: &mut crate::config::ResolvedDeployment,
    seal_key: &[u8; 32],
) -> Result<Option<PreparedLocalAcp>, String> {
    if !uses_host_acp_discovery(deployment) {
        // Container-backed ACP executables belong to the configured image/Pod,
        // not the coordinator host. Host discovery would incorrectly erase a
        // valid deployment profile merely because that executable is absent
        // outside its Environment.
        return Ok(None);
    }
    let discovery: Arc<dyn AcpDiscovery> =
        Arc::new(AcpHostDiscovery::local(std::time::Duration::from_secs(3)));
    let installer: Arc<dyn AcpWrapperInstaller> = Arc::new(NpmWrapperInstaller);
    let negotiator: Arc<dyn AcpCapabilityNegotiator> = Arc::new(HostAcpCapabilityNegotiator::new(
        std::time::Duration::from_secs(10),
        Arc::new(awaken_protocol_acp::ProtocolAcpCapabilityHandshake),
    ));
    let stores =
        awaken_control::open_inference_materialization_stores(&deployment.control, seal_key).await;
    let resources = Some(local_worker_resources(deployment).await?);
    prepare_local_acp_with(
        deployment, discovery, installer, negotiator, stores, resources,
    )
    .await
}

async fn local_worker_resources(
    deployment: &crate::config::ResolvedDeployment,
) -> Result<awaken_worker::WorkerResourcePlane, String> {
    let validator: Arc<dyn awaken_resource_contract::ResourceBindingValidator> =
        match &deployment.control.admin {
            awaken_control::StoreBackend::Sqlite(path) => Arc::new(
                awaken_admin_config_api::SqliteAdminStore::open(&path.to_string_lossy())
                    .map_err(|error| format!("open local Worker Resource Catalog: {error}"))?,
            ),
            awaken_control::StoreBackend::Postgres(_) => {
                awaken_control::open_shared_resource_validator(Some(&deployment.control.admin))
                    .await?
                    .expect("Postgres admin backend produces a validator")
            }
        };
    let ports = match &deployment.resources {
        crate::config::ResourcePlaneStoreBackend::Embedded(root) => {
            awaken_server::embedded_resource_plane(root)
        }
        crate::config::ResourcePlaneStoreBackend::Postgres(url) => {
            awaken_server::shared_worker_resource_plane(Some(url))
                .await?
                .ok_or_else(|| "Postgres resource plane did not produce Worker ports".to_string())?
        }
    };
    let mounter = Arc::new(awaken_sandbox_memoryd::MemoryStoreMounter::new(
        ports.memory_repository(),
    ));
    Ok(awaken_worker::WorkerResourcePlane::new(ports, validator).with_memory_mounter(mounter))
}

fn configured_worker_builder(
    upstream: impl Into<String>,
    deployment: &crate::config::ResolvedDeployment,
    credentials: awaken_runtime_host::PinnedCredentialMaterializer,
) -> awaken_worker::WorkerNodeBuilder {
    let worker = &deployment.worker;
    let mut manifest = worker
        .build_digest
        .clone()
        .map(awaken_worker::StandardManifestConfig::new)
        .unwrap_or_default()
        .with_extra_capabilities(worker.capabilities.clone());
    if let Some(zone) = &worker.zone {
        manifest = manifest.with_zone(zone.clone());
    }
    if let Some(max_concurrent) = worker.max_concurrent {
        manifest = manifest.with_max_concurrent(max_concurrent);
    }

    let mut builder = awaken_worker::WorkerNodeBuilder::new(
        awaken_runtime_host::WorkerUpstream::new(upstream).with_worker_id(&worker.worker_id),
    )
    .with_deployment_config(deployment.runtime.clone())
    .with_standard_manifest_config(manifest)
    .with_inference_materializer(Arc::new(
        awaken_server::inference_materializer::CredentialInferenceMaterializer::from_pinned(
            credentials.clone(),
        ),
    ))
    .with_remote_attempt_executor(awaken_server::a2a_attempt_executor(Some(
        credentials.clone(),
    )))
    .with_hand_executor_factory(awaken_server::relay_hand_executor_factory())
    .with_credential_materializer(credentials)
    .with_graceful_drain(std::time::Duration::from_secs(worker.drain_grace_secs))
    .with_credential_observation_window(
        std::time::Duration::from_secs(worker.credential_probe_interval_secs),
        std::time::Duration::from_secs(worker.credential_observation_ttl_secs),
    )
    .with_standard_manifest(Default::default());
    builder = match &worker.admin_listen {
        Some(address) => builder.with_admin_listen(address),
        None => builder.without_admin_surface(),
    };
    builder
}

/// Compose the process Worker from the already-resolved product configuration.
/// Store opening and concrete server adapters remain at this CLI boundary.
pub async fn build_configured_worker(
    upstream: impl Into<String>,
    deployment: &crate::config::ResolvedDeployment,
    seal_key: &[u8; 32],
) -> Result<awaken_worker::WorkerNode, String> {
    let stores =
        awaken_control::open_inference_materialization_stores(&deployment.control, seal_key).await;
    let credentials =
        awaken_runtime_host::PinnedCredentialMaterializer::new(stores.credentials, stores.secrets);
    let resources = local_worker_resources(deployment).await?;
    configured_worker_builder(upstream, deployment, credentials)
        .with_resource_plane(resources)
        .build()
        .map_err(|error| error.to_string())
}

async fn prepare_local_acp_with(
    deployment: &mut crate::config::ResolvedDeployment,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
    stores: awaken_control::InferenceMaterializationStores,
    resources: Option<awaken_worker::WorkerResourcePlane>,
) -> Result<Option<PreparedLocalAcp>, String> {
    let wrapper_root = deployment.data_dir.join("acp-wrappers");
    let selected_cli_ids = deployment.runtime.acp.as_ref().map(|profile| {
        profile
            .cli_ids()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    });
    let workspace =
        awaken_runtime_host::SharedHost::provision_local_workspace_at(&deployment.data_dir);
    let probe_cwd = deployment.data_dir.join("acp-probe");
    tokio::fs::create_dir_all(&probe_cwd)
        .await
        .map_err(|error| format!("create local ACP probe directory: {error}"))?;
    let prepared = prepare_host_acp_with(
        LocalAcpPreparation {
            probe_cwd,
            initial_workspace: Some(workspace),
            wrapper_root,
            selected_cli_ids,
            credentials: stores.credentials.clone(),
        },
        discovery,
        installer,
        negotiator,
    )
    .await?;
    let routable_cli_ids = prepared.routable_cli_ids();
    let routable_launch_argv = prepared.routable_launch_argv();
    deployment.apply_local_acp_observations(prepared.observations, routable_cli_ids)?;
    let Some(profile) = deployment.runtime.acp.as_mut() else {
        return Ok(None);
    };
    profile.apply_launch_argv(&routable_launch_argv)?;
    // The registered Worker becomes the sole local execution pool. Keeping the
    // anonymous coordinator pool active would create two overlapping claimers
    // for ordinary runs, while only one can publish credential liveness.
    deployment.runtime.disable_local_pool = true;
    deployment.run_local_pool = false;
    Ok(Some(PreparedLocalAcp {
        resolver: prepared.resolver,
        stores,
        resources,
    }))
}

impl PreparedLocalAcp {
    /// Build the canonical registered Worker against this process's control
    /// URL. The Runtime Host selects trusted Workdir only for BackendOwned
    /// Sessions and retains this deployment's isolation tier for Provider runs.
    pub fn build_worker(
        self,
        upstream: impl Into<String>,
        deployment: &crate::config::ResolvedDeployment,
    ) -> Result<awaken_worker::WorkerNode, String> {
        let resolver = self.resolver;
        let credentials = awaken_runtime_host::PinnedCredentialMaterializer::new(
            self.stores.credentials,
            self.stores.secrets,
        );
        let mut builder = configured_worker_builder(upstream, deployment, credentials)
            .with_worker_local_credential_resolver(resolver.clone())
            .with_acp_capability_observation_source(resolver)
            .without_admin_surface();
        if let Some(resources) = self.resources {
            builder = builder.with_resource_plane(resources);
        }
        builder.build().map_err(|error| error.to_string())
    }
}

/// Run the same canonical discovery service as startup and render its
/// secret-free observations. No diagnostic state is persisted.
pub async fn local_acp_diagnostics(json: bool) -> String {
    let discovery = AcpHostDiscovery::local(std::time::Duration::from_secs(3));
    render_diagnostics(&discovery.discover_all().await, json)
}

fn render_diagnostics(observations: &[AcpHostObservation], json: bool) -> String {
    let rows: Vec<_> = observations
        .iter()
        .filter_map(|observation| {
            let cli = acp_cli(&observation.cli_id)?;
            Some(serde_json::json!({
                "id": observation.cli_id,
                "name": observation.display_name,
                "supported": true,
                "detected": observation.detected(),
                "version": observation.version,
                "login_state": observation.credential_state.map(credential_state_name),
                "reason_code": observation.reason_code,
                "remediation": cli.remediation(observation.reason_code.as_deref()),
            }))
        })
        .collect();
    if json {
        return serde_json::to_string_pretty(&serde_json::json!({ "acp": rows }))
            .expect("ACP diagnostics serialize");
    }
    let mut output = String::from("Awaken ACP diagnostics\n\n");
    for row in rows {
        let status = if row["detected"] == true {
            row["login_state"].as_str().unwrap_or("probe_failed")
        } else {
            "not_detected"
        };
        output.push_str(&format!(
            "  {:<12} {:<16} {}\n",
            row["id"].as_str().unwrap_or("unknown"),
            status,
            row["version"].as_str().unwrap_or("-")
        ));
        if let Some(remediation) = row["remediation"].as_str() {
            output.push_str(&format!("    {remediation}\n"));
        }
    }
    output
}

pub(crate) fn credential_state_name(state: CredentialObservationState) -> &'static str {
    match state {
        CredentialObservationState::Available => "available",
        CredentialObservationState::LoginRequired => "login_required",
        CredentialObservationState::Expired => "expired",
        CredentialObservationState::Invalid => "invalid",
        CredentialObservationState::Disabled => "disabled",
        CredentialObservationState::ProbeFailed => "probe_failed",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use awaken_acp_application::AcpDetectionState;
    use awaken_acp_contract::NegotiatedAcpCapabilities;
    use awaken_credential_vault::repo::{
        CredentialRepo, InMemoryCredentialRepo, ensure_worker_local,
    };
    use awaken_credential_vault::{CredentialSource, CredentialStatus, WorkerLocalBinding};
    use awaken_run_executor_acp::AcpCli;
    use awaken_runtime_contract::{
        CredentialMaterialError, CredentialMaterialSource, CredentialObservationSource,
        CredentialRef, WorkerLocalReferenceRevalidator,
    };

    use super::*;

    /// Cause/effect rule C1: resolved CLI deployment + opened credential and
    /// Resource stores installs the existing inference, A2A, relay, validator
    /// and mounter adapters once; the built Worker advertises Native, A2A and
    /// Resource support with one decodable merged credential evidence value.
    #[tokio::test]
    async fn configured_worker_projects_only_the_cli_installed_adapters() {
        let directory = tempfile::tempdir().unwrap();
        let deployment = crate::config::local_test_deployment(directory.path().into());
        deployment.ensure_data_layout().unwrap();
        let worker = build_configured_worker("http://127.0.0.1:1", &deployment, &[7_u8; 32])
            .await
            .expect("C1 complete CLI Worker topology");
        let capabilities = &worker.manifest().capabilities;
        assert!(capabilities.contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY));
        assert!(capabilities.contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY));
        assert!(capabilities.contains(awaken_worker_contract::REPOSITORY_CREDENTIALS_CAPABILITY));
        let evidence =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                capabilities,
            )
            .expect("C1 one merged credential evidence capability");
        assert!(
            evidence
                .material_sources
                .contains(&CredentialMaterialSource::ControlPlaneReference)
        );
    }

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

    struct FixedInstaller {
        failures: BTreeSet<String>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AcpWrapperInstaller for FixedInstaller {
        async fn resolved_argv(
            &self,
            cli: &AcpCli,
            _root: &Path,
        ) -> Result<Option<Vec<String>>, String> {
            if !cli.acquisition.requires_installation() {
                return Ok(None);
            }
            self.calls.lock().unwrap().push(cli.id.to_string());
            if self.failures.contains(cli.id) {
                return Err("fixture install failure".into());
            }
            Ok(Some(vec![format!("/fixed/{}-acp", cli.id)]))
        }
    }

    struct FixedNegotiator {
        fail: bool,
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for FixedNegotiator {
        async fn negotiate(
            &self,
            _argv: &[String],
            cwd: &Path,
            _auth_method_id: Option<&str>,
        ) -> Result<NegotiatedAcpCapabilities, String> {
            // Cause P0: a Workspace id is an ownership coordinate, not an OS
            // path. Effect: the application service provisions the explicit
            // probe cwd before invoking any real ACP process.
            assert!(cwd.is_dir(), "P0 probe cwd must exist");
            if self.fail {
                return Err("fixture capability failure".into());
            }
            Ok(NegotiatedAcpCapabilities {
                protocol_version: "1".into(),
                load_session: true,
                prompt_image: false,
                prompt_audio: false,
                prompt_embedded_context: false,
                mcp_http: true,
                mcp_sse: false,
                session_list: false,
                modes: Vec::new(),
                config_options: Vec::new(),
            })
        }
    }

    fn fixed_negotiator() -> Arc<dyn AcpCapabilityNegotiator> {
        Arc::new(FixedNegotiator { fail: false })
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
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
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
        // stops before I/O; the type has no material-resolution operation.
        //
        // Decision table:
        // R1 exact + available -> Available, one profile probe
        // R2 stale revision    -> Unavailable, no additional probe
        // R3 material API      -> absent from the resolver type
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

        // R3 is a compile-time property: `AcpLocalCredentialResolver` implements
        // only CredentialObservationSource + WorkerLocalReferenceRevalidator.
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
        .with_credential_materializer(awaken_runtime_host::PinnedCredentialMaterializer::new(
            credentials,
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        ))
        .with_worker_local_credential_resolver(resolver.clone())
        .with_standard_manifest(Default::default())
        .build()
        .expect("B1");

        let evidence =
            awaken_runtime_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
                &worker.manifest().capabilities,
            )
            .expect("B2 capability evidence");
        assert!(
            worker
                .manifest()
                .capabilities
                .contains("worker-local-credentials/v1"),
            "B2 liveness/use capability is independent of secret material"
        );
        assert!(
            !evidence
                .material_sources
                .contains(&CredentialMaterialSource::WorkerReference),
            "B2"
        );
    }

    #[tokio::test]
    async fn startup_discovery_registers_one_idempotent_binding_and_builds_the_existing_worker() {
        // Cause graph: one detected catalog row -> one launch profile -> one
        // stable WorkerLocal locator -> the existing WorkerNode manifest. A
        // repeated startup reaches the same repository key.
        //
        // Decision table:
        // P1 detected+available -> route + binding + acp capability
        // P2 repeated startup   -> same binding, no duplicate
        // P3 missing rows       -> diagnostics only, no route or binding
        // P4 explicitly unselected detected row -> diagnostic only, no acquisition
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().into());
        deployment.runtime.acp = Some(
            awaken_runtime_host::AcpWorkerProfile::new(
                ["codex".to_string()],
                Some("codex".to_string()),
            )
            .unwrap(),
        );
        let observations = awaken_run_executor_acp::known_acp_clis()
            .iter()
            .map(|cli| {
                if matches!(cli.id, "codex" | "claude") {
                    host(
                        cli.id,
                        AcpDetectionState::Detected,
                        Some(CredentialObservationState::Available),
                    )
                } else {
                    host(cli.id, AcpDetectionState::Missing, None)
                }
            })
            .map(|observation| (observation.cli_id.clone(), observation))
            .collect();
        let discovery: Arc<dyn AcpDiscovery> = Arc::new(FixedDiscovery {
            observations,
            calls: Mutex::new(Vec::new()),
        });
        let installer = Arc::new(FixedInstaller {
            failures: BTreeSet::new(),
            calls: Mutex::new(Vec::new()),
        });
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let stores = awaken_control::InferenceMaterializationStores {
            credentials: credentials.clone(),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        };

        let prepared = prepare_local_acp_with(
            &mut deployment,
            discovery.clone(),
            installer.clone(),
            fixed_negotiator(),
            stores.clone(),
            None,
        )
        .await
        .unwrap()
        .expect("P1");
        let workspace =
            awaken_runtime_host::SharedHost::provision_local_workspace_at(&deployment.data_dir);
        let first = credentials.list(&workspace).await.unwrap();
        assert_eq!(first.len(), 1, "P1/P3");
        assert_eq!(
            first[0]
                .worker_local_binding
                .as_ref()
                .map(|binding| binding.driver_id.as_str()),
            Some("acp:codex"),
            "P1"
        );
        let worker = prepared
            .build_worker("http://127.0.0.1:1", &deployment)
            .unwrap();
        assert!(worker.manifest().capabilities.contains("acp:codex"), "P1");
        assert!(
            worker
                .manifest()
                .capabilities
                .iter()
                .all(|capability| !capability.starts_with("acp-capability/")),
            "P1 mutable negotiated fingerprints belong only to heartbeat observations"
        );
        assert!(
            worker.manifest().sandbox_backends.contains("namespace"),
            "P1 managed runs retain the deployment tier; BackendOwned selects Workdir per Session"
        );
        assert!(
            deployment.runtime.disable_local_pool,
            "P1 sole execution pool"
        );
        assert_eq!(
            deployment
                .runtime
                .acp
                .as_ref()
                .and_then(|profile| profile.launch_argv("codex")),
            Some(&["/fixed/codex-acp".to_string()][..]),
            "P1 startup acquisition is the launch route"
        );

        prepare_local_acp_with(
            &mut deployment,
            discovery,
            installer.clone(),
            fixed_negotiator(),
            stores,
            None,
        )
        .await
        .unwrap()
        .expect("P2");
        let repeated = credentials.list(&workspace).await.unwrap();
        assert_eq!(repeated.len(), 1, "P2");
        assert_eq!(repeated[0].id, first[0].id, "P2");
        assert_eq!(&*installer.calls.lock().unwrap(), &["codex", "codex"], "P4");
        assert!(
            deployment
                .local_acp_observations
                .iter()
                .any(|row| row.cli_id == "claude" && row.detected()),
            "P4 diagnostics retain the unselected detected row"
        );
        let codex_observation = deployment
            .local_acp_observations
            .iter()
            .find(|row| row.cli_id == "codex")
            .expect("P1 codex observation");
        assert_eq!(
            codex_observation.capability_state,
            Some(awaken_acp_application::AcpCapabilityState::Verified),
            "P1"
        );
    }

    #[test]
    fn local_mode_discovers_identity_independently_of_the_managed_tier() {
        // Cause graph: local operating mode enables Worker host discovery; the
        // immutable provisioning variant later chooses Workdir vs managed tier.
        //
        // Decision table:
        // I1 Local + Workdir   -> trusted local Worker
        // I2 Local + Namespace-> discovery; BackendOwned selects Workdir
        // I3 Local + Docker   -> discovery; Provider retains Docker
        // I4 Server + Workdir -> no personal identity composition
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().into());
        deployment.runtime.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
        assert!(uses_trusted_local_identity(&deployment), "I1");

        deployment.runtime.sandbox_tier = awaken_runtime_host::SandboxTier::Namespace;
        assert!(uses_trusted_local_identity(&deployment), "I2");
        deployment.runtime.sandbox_tier = awaken_runtime_host::SandboxTier::Docker;
        assert!(uses_trusted_local_identity(&deployment), "I3");

        deployment.runtime.sandbox_tier = awaken_runtime_host::SandboxTier::Local;
        deployment.mode = crate::config::OperatingMode::Server;
        assert!(!uses_trusted_local_identity(&deployment), "I4");
    }

    #[tokio::test]
    async fn installed_wrapper_is_reused_without_running_the_installer() {
        // Cause graph: exact wrapper already exists -> canonical absolute argv.
        // Both first lookup and restart take the filesystem branch; a missing
        // npm binary or network therefore cannot affect either lookup.
        //
        // Decision table:
        // R1 existing wrapper, first lookup -> absolute argv
        // R2 existing wrapper, restart      -> identical argv, no subprocess
        let directory = tempfile::tempdir().unwrap();
        let executable = directory
            .path()
            .join("codex")
            .join("node_modules/.bin/codex-acp");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "fixture").unwrap();

        let cli = acp_cli("codex").unwrap();
        let first = NpmWrapperInstaller
            .resolved_argv(cli, directory.path())
            .await
            .expect("R1")
            .expect("R1 wrapper argv");
        let restarted = NpmWrapperInstaller
            .resolved_argv(cli, directory.path())
            .await
            .expect("R2")
            .expect("R2 wrapper argv");
        assert_eq!(first, restarted, "R2");
        assert_eq!(
            first,
            vec![executable.canonicalize().unwrap().to_string_lossy()]
        );
    }

    #[tokio::test]
    async fn wrapper_install_failure_removes_the_route_and_records_diagnostics() {
        // Cause graph: detected wrapper-backed CLI + failed startup acquisition
        // -> ProbeFailed observation -> no profile, binding, or ACP Worker. This
        // is fail-closed and never falls back to `npx` at run time.
        //
        // Decision table:
        // F1 install succeeds -> route/binding (covered by startup test above)
        // F2 install fails    -> diagnostic only, no executable route
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = crate::config::local_test_deployment(directory.path().into());
        let observations = awaken_run_executor_acp::known_acp_clis()
            .iter()
            .map(|cli| {
                let observation = if cli.id == "codex" {
                    host(
                        cli.id,
                        AcpDetectionState::Detected,
                        Some(CredentialObservationState::Available),
                    )
                } else {
                    host(cli.id, AcpDetectionState::Missing, None)
                };
                (cli.id.to_string(), observation)
            })
            .collect();
        let discovery: Arc<dyn AcpDiscovery> = Arc::new(FixedDiscovery {
            observations,
            calls: Mutex::new(Vec::new()),
        });
        let installer: Arc<dyn AcpWrapperInstaller> = Arc::new(FixedInstaller {
            failures: BTreeSet::from(["codex".to_string()]),
            calls: Mutex::new(Vec::new()),
        });
        let credentials = Arc::new(InMemoryCredentialRepo::new());
        let stores = awaken_control::InferenceMaterializationStores {
            credentials: credentials.clone(),
            secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
        };

        let prepared = prepare_local_acp_with(
            &mut deployment,
            discovery,
            installer,
            fixed_negotiator(),
            stores,
            None,
        )
        .await
        .expect("F2 startup remains diagnosable");
        assert!(prepared.is_none(), "F2");
        assert!(deployment.runtime.acp.is_none(), "F2");
        let codex = deployment
            .local_acp_observations
            .iter()
            .find(|observation| observation.cli_id == "codex")
            .expect("F2 diagnostic row");
        assert_eq!(codex.detection, AcpDetectionState::ProbeFailed, "F2");
        assert_eq!(
            codex.reason_code.as_deref(),
            Some("acp_wrapper_install_failed"),
            "F2"
        );
        let workspace =
            awaken_runtime_host::SharedHost::provision_local_workspace_at(&deployment.data_dir);
        assert!(credentials.list(&workspace).await.unwrap().is_empty(), "F2");
    }

    #[test]
    fn diagnostics_render_the_same_reason_and_profile_owned_remediation() {
        // Cause graph: canonical observation + catalog remediation -> text/JSON
        // diagnostic projections. Rendering performs no probe and owns no state.
        let observations = [host(
            "codex",
            AcpDetectionState::Detected,
            Some(CredentialObservationState::LoginRequired),
        )];
        let mut observations = observations;
        observations[0].reason_code = Some("acp_login_required".into());
        let json: serde_json::Value =
            serde_json::from_str(&render_diagnostics(&observations, true)).unwrap();
        assert_eq!(json["acp"][0]["login_state"], "login_required");
        assert!(
            json["acp"][0]["remediation"]
                .as_str()
                .unwrap()
                .contains("codex login")
        );
        let text = render_diagnostics(&observations, false);
        assert!(text.contains("codex") && text.contains("login_required"));
    }
}
#[test]
fn host_discovery_is_limited_to_host_backed_sandbox_tiers() {
    let directory = tempfile::tempdir().unwrap();
    let mut deployment = crate::config::local_test_deployment(directory.path().into());
    for tier in [
        awaken_runtime_host::SandboxTier::Local,
        awaken_runtime_host::SandboxTier::Namespace,
    ] {
        deployment.runtime.sandbox_tier = tier;
        assert!(uses_host_acp_discovery(&deployment));
    }
    for tier in [
        awaken_runtime_host::SandboxTier::Docker,
        awaken_runtime_host::SandboxTier::Podman,
        awaken_runtime_host::SandboxTier::K8s,
    ] {
        deployment.runtime.sandbox_tier = tier;
        assert!(!uses_host_acp_discovery(&deployment));
    }
}
