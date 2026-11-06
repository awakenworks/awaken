// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: This source implements the repository-controlled credential product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Composite Worker-local credential resolver for every discovered ACP agent.
//!
//! It adapts the canonical ACP discovery observations to the existing credential
//! liveness port. It never opens, returns, or materializes CLI credentials.

use awaken_time::WallClock;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use awaken_acp_application::{ensure_workspace_bindings, project_local_credential_sources};
use awaken_acp_catalog::acp_cli;
use awaken_acp_contract::AcpHostObservation;
use awaken_credential_contract::CredentialObservationState;
use awaken_credential_vault::{CredentialSource, repo::CredentialRepo};
use awaken_run_executor_acp::host::{
    AcpCapabilityNegotiator, AcpDiscovery, AcpHostDiscovery, AcpLocalCredentialResolver,
    AcpWrapperInstaller, HostAcpPreparation, PreparedHostAcpCapabilities,
    prepare_host_acp_capabilities_with,
};

/// Local CLI composition inputs; the Worker preparation has no repository.
struct LocalAcpPreparation {
    probe_cwd: PathBuf,
    initial_workspace: Option<String>,
    wrapper_root: PathBuf,
    selected_cli_ids: Option<BTreeSet<String>>,
    credentials: Arc<dyn CredentialRepo>,
    clock: Arc<dyn WallClock>,
}

async fn prepare_host_acp_with(
    input: LocalAcpPreparation,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
) -> Result<PreparedHostAcpCapabilities, String> {
    let host = prepare_host_acp_capabilities_with(
        HostAcpPreparation {
            probe_cwd: input.probe_cwd,
            wrapper_root: input.wrapper_root,
            selected_cli_ids: input.selected_cli_ids,
            clock: input.clock,
        },
        discovery,
        installer,
        negotiator,
    )
    .await?;
    if let Some(workspace) = input.initial_workspace {
        bind_workspace(&host, input.credentials.as_ref(), &workspace).await?;
    }
    Ok(host)
}

async fn bind_workspace(
    prepared: &PreparedHostAcpCapabilities,
    credentials: &dyn CredentialRepo,
    workspace: &str,
) -> Result<(), String> {
    let catalog = awaken_acp_catalog::known_acp_publication_capabilities();
    ensure_workspace_bindings(
        credentials,
        workspace,
        prepared.selected_cli_ids(),
        &catalog,
    )
    .await?;
    let sources = credentials
        .list(workspace)
        .await
        .map_err(|error| format!("list local ACP bindings: {error}"))?;
    add_sources(&prepared.resolver, sources)
}

/// Adapt stored metadata at the Coordinator boundary, never in a Worker.
#[cfg(test)]
fn resolver_from_sources(
    discovery: Arc<dyn AcpDiscovery>,
    sources: impl IntoIterator<Item = CredentialSource>,
    clock: Arc<dyn WallClock>,
) -> Result<AcpLocalCredentialResolver, String> {
    AcpLocalCredentialResolver::from_bindings(
        discovery,
        project_local_credential_sources(
            &awaken_acp_catalog::known_acp_publication_capabilities(),
            sources,
        )?,
        clock,
    )
}
fn add_sources(
    resolver: &AcpLocalCredentialResolver,
    sources: impl IntoIterator<Item = CredentialSource>,
) -> Result<(), String> {
    resolver.add_bindings(project_local_credential_sources(
        &awaken_acp_catalog::known_acp_publication_capabilities(),
        sources,
    )?)
}

/// One startup result for a trusted local ACP Worker. The durable
/// sources remain in the credential repository; this value carries only the
/// already-configured dependencies needed to build the existing WorkerNode.
pub struct PreparedLocalAcp {
    resolver: Arc<AcpLocalCredentialResolver>,
    stores: awaken_control::InferenceMaterializationStores,
}

/// The AllInOne process's canonical registered Worker. ACP observations are an
/// optional capability of this Worker, never the condition that creates it.
#[derive(Clone)]
pub struct PreparedLocalWorker {
    resolver: Option<Arc<AcpLocalCredentialResolver>>,
    stores: awaken_control::InferenceMaterializationStores,
    admin_tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    inference_override: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
}

/// Prepare the embedded Worker selected by an AllInOne deployment. Host ACP
/// discovery enriches its manifest when applicable; Native and A2A execution use
/// the same WorkerNode when no local ACP CLI exists.
pub async fn prepare_local_worker(
    deployment: &mut awaken_process_startup::config::ResolvedDeployment,
    seal_key: &[u8; 32],
    clock: Arc<dyn WallClock>,
) -> Result<PreparedLocalWorker, String> {
    let installations = awaken_process_startup::verify_deployment_installations(deployment).await?;
    let workspace = installations.platform_workspace_before_write()?;
    prepare_local_worker_with_workspace(deployment, seal_key, workspace, clock).await
}

pub(crate) async fn prepare_local_worker_with_workspace(
    deployment: &mut awaken_process_startup::config::ResolvedDeployment,
    seal_key: &[u8; 32],
    workspace: Option<String>,
    clock: Arc<dyn WallClock>,
) -> Result<PreparedLocalWorker, String> {
    let prepared = prepare_local_acp_with_workspace(deployment, seal_key, workspace, clock).await?;
    let worker = match prepared {
        Some(prepared) => PreparedLocalWorker {
            resolver: Some(prepared.resolver),
            stores: prepared.stores,
            admin_tools: Vec::new(),
            inference_override: None,
        },
        None => PreparedLocalWorker {
            resolver: None,
            stores: awaken_control::open_inference_materialization_stores(
                &deployment.control,
                seal_key,
            )
            .await,
            admin_tools: Vec::new(),
            inference_override: None,
        },
    };
    // One registered Worker claims Native, ACP, and outbound A2A attempts. The
    // Coordinator's anonymous local pool would be a second execution owner.
    deployment.runtime.disable_local_pool = true;
    deployment.run_local_pool = false;
    Ok(worker)
}

fn uses_trusted_local_identity(
    deployment: &awaken_process_startup::config::ResolvedDeployment,
) -> bool {
    deployment.mode == awaken_process_startup::config::OperatingMode::Local
}

fn uses_host_acp_discovery(
    deployment: &awaken_process_startup::config::ResolvedDeployment,
) -> bool {
    uses_trusted_local_identity(deployment)
        && deployment
            .configured_acp_clis
            .as_ref()
            .is_none_or(|cli_ids| !cli_ids.is_empty())
        && matches!(
            deployment.runtime.sandbox_tier,
            awaken_deployment_config::SandboxTier::Local
                | awaken_deployment_config::SandboxTier::Namespace
        )
}

/// Discover local ACP agents once, register their secret-free WorkerLocal
/// locators idempotently, and configure the one liveness resolver. Server mode
/// deliberately does none of this.
pub async fn prepare_local_acp(
    deployment: &mut awaken_process_startup::config::ResolvedDeployment,
    seal_key: &[u8; 32],
    clock: Arc<dyn WallClock>,
) -> Result<Option<PreparedLocalAcp>, String> {
    let installations = awaken_process_startup::verify_deployment_installations(deployment).await?;
    let workspace = installations.platform_workspace_before_write()?;
    prepare_local_acp_with_workspace(deployment, seal_key, workspace, clock).await
}

async fn prepare_local_acp_with_workspace(
    deployment: &mut awaken_process_startup::config::ResolvedDeployment,
    seal_key: &[u8; 32],
    workspace: Option<String>,
    clock: Arc<dyn WallClock>,
) -> Result<Option<PreparedLocalAcp>, String> {
    if !uses_host_acp_discovery(deployment) {
        // Container-backed ACP executables belong to the configured image/Pod,
        // not the coordinator host. Host discovery would incorrectly erase a
        // valid deployment profile merely because that executable is absent
        // outside its Environment.
        return Ok(None);
    }
    let adapters = awaken_run_executor_acp::host::HostAcpAdapters::production(Arc::new(
        awaken_protocol_acp::ProtocolAcpCapabilityHandshake,
    ));
    let stores =
        awaken_control::open_inference_materialization_stores(&deployment.control, seal_key).await;
    let workspace = workspace.ok_or_else(|| {
        "local ACP preparation requires the admitted platform Workspace coordinate".to_owned()
    })?;
    prepare_local_acp_with(
        deployment,
        adapters.discovery,
        adapters.installer,
        adapters.negotiator,
        stores,
        workspace,
        clock,
    )
    .await
}

fn configured_worker_upstream(
    upstream: impl Into<String>,
    deployment: &awaken_process_startup::config::ResolvedDeployment,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<awaken_worker_transport_security::WorkerUpstream, String> {
    awaken_worker::worker_upstream(
        &deployment.worker,
        upstream,
        awaken_worker::WorkerUpstreamSecurity::Local,
        awaken_worker::PodIdentity::installed(),
        clock,
    )
}

fn configured_worker_builder(
    upstream: impl Into<String>,
    deployment: &awaken_process_startup::config::ResolvedDeployment,
    credentials: awaken_credential_materializer::PinnedCredentialMaterializer,
    clock: Arc<dyn awaken_time::WallClock>,
    inference_override: Option<Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>>,
) -> Result<awaken_worker::WorkerNodeBuilder, String> {
    let worker = &deployment.worker;
    let upstream = configured_worker_upstream(upstream, deployment, clock.clone())?;
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

    let mut builder = awaken_worker::WorkerNodeBuilder::new(upstream, clock)
        .with_deployment_config(
            deployment.runtime.clone(),
            deployment.observability.trace_file.is_some(),
        )
        .with_standard_manifest_config(manifest)
        .with_inference_materializer(inference_override.unwrap_or_else(|| {
            Arc::new(
                awaken_credential_materializer::CredentialInferenceMaterializer::from_pinned(
                    credentials.clone(),
                ),
            )
        }))
        .with_remote_attempt_executor(crate::a2a_attempt_executor(Some(credentials.clone())))
        .with_hand_executor_factory(awaken_tool_relay::relay_hand_executor_factory())
        .with_credential_materializer(credentials)
        .with_graceful_drain(std::time::Duration::from_secs(worker.drain_grace_secs))
        .with_credential_observation_window(
            std::time::Duration::from_secs(worker.credential_probe_interval_secs),
            std::time::Duration::from_secs(worker.credential_observation_ttl_secs),
        )
        .with_remote_session_resource_support()
        .with_standard_manifest(Default::default());
    builder = match &worker.admin_listen {
        Some(address) => builder.with_admin_listen(address),
        None => builder.without_admin_surface(),
    };
    Ok(builder)
}

async fn prepare_local_acp_with(
    deployment: &mut awaken_process_startup::config::ResolvedDeployment,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
    stores: awaken_control::InferenceMaterializationStores,
    workspace: String,
    clock: Arc<dyn WallClock>,
) -> Result<Option<PreparedLocalAcp>, String> {
    let wrapper_root = deployment.data_dir.join("acp-wrappers");
    let selected_cli_ids = deployment
        .configured_acp_clis
        .as_ref()
        .map(|cli_ids| cli_ids.iter().cloned().collect::<BTreeSet<_>>());
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
            clock,
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
    }))
}

impl PreparedLocalAcp {
    /// Build the canonical registered Worker against this process's control
    /// URL. The Runtime Host selects trusted Workdir only for BackendOwned
    /// Sessions and retains this deployment's isolation tier for Provider runs.
    /// `clock` is the embedding process's one wall clock.
    pub async fn build_worker(
        self,
        upstream: impl Into<String>,
        deployment: &awaken_process_startup::config::ResolvedDeployment,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<awaken_worker::WorkerNode, String> {
        let resolver = self.resolver;
        let credentials = awaken_credential_materializer::PinnedCredentialMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                self.stores.credentials,
                self.stores.secrets,
                clock.clone(),
            )),
            clock.clone(),
        );
        configured_worker_builder(upstream, deployment, credentials, clock, None)?
            .with_worker_local_credential_resolver(resolver.clone())
            .with_acp_capability_observation_source(resolver)
            .without_admin_surface()
            .prepare_session_environment_from_deployment()
            .await
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())
    }
}

impl PreparedLocalWorker {
    /// The composing product supplies the existing inference port to the Worker
    /// that executes it; Coordinator startup does not materialize a model.
    pub fn with_inference_materializer(
        mut self,
        selected: Arc<dyn awaken_inference_endpoint::InferenceExecutorMaterializer>,
    ) -> Self {
        self.inference_override = Some(selected);
        self
    }

    pub fn with_admin_tools(
        mut self,
        tools: Vec<Arc<dyn awaken_tool_contract::tool::RawTool>>,
    ) -> Self {
        self.admin_tools = tools;
        self
    }

    /// `clock` is the embedding process's one wall clock.
    pub async fn build_worker(
        &self,
        upstream: impl Into<String>,
        deployment: &awaken_process_startup::config::ResolvedDeployment,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<awaken_worker::WorkerNode, String> {
        let credentials = awaken_credential_materializer::PinnedCredentialMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                self.stores.credentials.clone(),
                self.stores.secrets.clone(),
                clock.clone(),
            )),
            clock.clone(),
        );
        let mut builder = configured_worker_builder(
            upstream,
            deployment,
            credentials,
            clock,
            self.inference_override.clone(),
        )?
        .with_admin_tools(self.admin_tools.clone())
        .without_admin_surface();
        if let Some(resolver) = &self.resolver {
            builder = builder
                .with_worker_local_credential_resolver(resolver.clone())
                .with_acp_capability_observation_source(resolver.clone());
        }
        builder
            .prepare_session_environment_from_deployment()
            .await
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())
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
    use awaken_acp_catalog::AcpCli;
    use awaken_acp_contract::AcpDetectionState;
    use awaken_acp_contract::NegotiatedAcpCapabilities;
    use awaken_agent_contract::credential::{CredentialMaterialSource, CredentialRef};
    use awaken_credential_contract::{
        CredentialMaterialError, CredentialObservationSource, WorkerLocalReferenceRevalidator,
    };
    use awaken_credential_vault::repo::{
        CredentialRepo, InMemoryCredentialRepo, ensure_worker_local,
    };
    use awaken_credential_vault::{CredentialSource, CredentialStatus, WorkerLocalBinding};

    use super::*;

    #[test]
    fn all_in_one_worker_uses_the_projected_request_signer() {
        // Causes/effects: A1 absent request credential retains the loopback-only
        // compatibility authorizer; A2 one valid credential matching worker_id
        // adds a route-bound AwakenWorker assertion; A3 missing/invalid/mismatched
        // projection fails before a WorkerNode can start. Decision rules W1-W3
        // cover each posture at the single WorkerUpstream construction boundary.
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = awaken_process_startup::config::local_test_deployment_at(
            directory.path().into(),
            awaken_time::system_clock(),
        );
        let unsigned = configured_worker_upstream(
            "http://coordinator",
            &deployment,
            awaken_time::system_clock(),
        )
        .unwrap();
        let unsigned_request = unsigned
            .authorize_request(
                "POST",
                "/v1/worker/register",
                unsigned
                    .client()
                    .post("http://coordinator/v1/worker/register"),
            )
            .unwrap()
            .build()
            .unwrap();
        assert!(
            unsigned_request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .is_none(),
            "W1"
        );

        let credential = directory.path().join("worker.json");
        std::fs::write(
            &credential,
            r#"{"worker_id":"awaken-worker","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        deployment.worker.request_credential_file = Some(credential.clone());
        let signed = configured_worker_upstream(
            "http://coordinator",
            &deployment,
            awaken_time::system_clock(),
        )
        .unwrap();
        let signed_request = signed
            .authorize_request(
                "POST",
                "/v1/worker/register",
                signed
                    .client()
                    .post("http://coordinator/v1/worker/register"),
            )
            .unwrap()
            .build()
            .unwrap();
        assert!(
            signed_request
                .headers()
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("AwakenWorker ")),
            "W2"
        );

        std::fs::write(
            &credential,
            r#"{"worker_id":"other-worker","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        assert!(
            configured_worker_upstream(
                "http://coordinator",
                &deployment,
                awaken_time::system_clock()
            )
            .is_err(),
            "W3"
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

    #[tokio::test]
    async fn continuity_fence_precedes_public_acp_effects() {
        /* Cause/effect graph: C1 expected Workspace marker matches; C2 Session
         * authority is corrupt. Effects: E1 the canonical
         * session_storage_invalid diagnostic terminates the public ACP entry;
         * E2 the data tree and deployment flags remain unchanged; therefore E3
         * discovery, acquisition, credential-store opening/registration,
         * probe/workspace provisioning, and wrapper installation are never
         * reached. Decision rule A1=C1+C2=>E1+E2+E3. The valid fresh continuity
         * row is owned by the config/report decision table; the
         * startup-discovery test below covers the post-gate injected seam. That
         * seam starts only after this public entry's store boundary and does not
         * duplicate the continuity gate. */
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("platform-workspace-id"),
            "workspace_expected",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("sessions.db"),
            b"SQLite format 3\0truncated",
        )
        .unwrap();
        let mut deployment = awaken_process_startup::config::local_test_deployment_at(
            directory.path().into(),
            awaken_time::system_clock(),
        );
        deployment.expected_platform_workspace_id = Some("workspace_expected".into());
        let snapshot = || {
            std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (entry.file_name(), std::fs::read(entry.path()).unwrap())
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = snapshot();
        let initial_disable_local_pool = deployment.runtime.disable_local_pool;
        let initial_run_local_pool = deployment.run_local_pool;

        let error = match prepare_local_acp(
            &mut deployment,
            &[0x61; 32],
            awaken_time::system_clock(),
        )
        .await
        {
            Ok(_) => panic!("A1 corrupt Session authority must stop public ACP preparation"),
            Err(error) => error,
        };
        assert!(error.starts_with("session_storage_invalid:"), "A1: {error}");
        assert_eq!(snapshot(), before, "A1/E2 data tree");
        assert_eq!(
            deployment.runtime.disable_local_pool, initial_disable_local_pool,
            "A1/E2 runtime profile"
        );
        assert_eq!(
            deployment.run_local_pool, initial_run_local_pool,
            "A1/E2 deployment profile"
        );
        assert!(deployment.local_acp_observations.is_empty(), "A1/E2");
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
        // Effects: each detected/login/missing/failure/disabled partition maps to
        // its existing typed state, and disabled performs no process probe.
        // Constraints/invariants: a failed credential is local to that exact
        // reference and the projection never transports login material.
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
        // The built-in host adapter supports one default account per CLI.
        // A second Workspace is another reference, not a fictional account.
        let mut disabled = ensure_worker_local(
            &repo,
            "disabled-ws",
            WorkerLocalBinding::new("acp:codex", "default"),
            None,
        )
        .await
        .unwrap();
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
        let resolver = resolver_from_sources(
            discovery.clone(),
            [codex, claude, gemini, opencode, disabled],
            awaken_time::system_clock(),
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
        // Effects: an exact Available pin succeeds after one targeted probe;
        // stale identity returns Unavailable with no additional I/O.
        // Constraints/invariants: pre-launch validation cannot substitute a
        // nearby revision/account or acquire secret bytes.
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
            resolver_from_sources(discovery.clone(), [codex], awaken_time::system_clock()).unwrap();

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
        assert!(
            resolver_from_sources(discovery.clone(), [missing], awaken_time::system_clock(),)
                .is_err()
        );

        let unrelated = ensure_worker_local(
            &repo,
            "ws",
            WorkerLocalBinding::new("git:ssh", "default"),
            None,
        )
        .await
        .unwrap();
        let resolver =
            resolver_from_sources(discovery, [unrelated], awaken_time::system_clock()).unwrap();
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
        let resolver = Arc::new(
            resolver_from_sources(discovery, [source], awaken_time::system_clock()).unwrap(),
        );
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
            awaken_worker_transport_security::WorkerUpstream::new("http://control"),
            awaken_time::system_clock(),
        )
        .with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::new(
                Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                    credentials,
                    Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
                    awaken_time::system_clock(),
                )),
                awaken_time::system_clock(),
            ),
        )
        .with_worker_local_credential_resolver(resolver.clone())
        .with_standard_manifest(Default::default())
        .build()
        .expect("B1");

        let evidence =
            awaken_credential_contract::CredentialRealizationCapabilities::from_manifest_capabilities(
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

    struct CompositionInference;
    impl awaken_inference_endpoint::InferenceExecutorMaterializer for CompositionInference {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &["composition-probe"]
        }
        fn materialize_pinned(
            &self,
            _: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
            _: &awaken_runtime_contract::RuntimeRunContext,
        ) -> Option<Arc<dyn awaken_runtime_contract::llm::LlmExecutor>> {
            panic!("a Worker constructor must not execute inference")
        }
    }

    #[tokio::test]
    async fn all_in_one_worker_exists_without_an_acp_capability() {
        // Cause/effect graph: C1=AllInOne; C2=local ACP resolver present.
        // Effects: E1=the canonical WorkerNode still exists; E2=Native and generic
        // outbound A2A capabilities are advertised; E3=no invented ACP capability;
        // E4=the canonical registration-bound Session Resource adapter is installed.
        // Decision table: W1 C1&&!C2 -> E1+E2+E3+E4. The C1+C2 row is owned by the
        // startup-discovery test below and adds exact ACP observations to this base.
        // W2 C1&&!C2 + product inference provided => E1-E4 still hold and
        // exactly that port's supported scheme is advertised by the real Worker.
        // No constructor may call materialize_pinned or execute a model.
        for supplied_inference in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut deployment = awaken_process_startup::config::local_test_deployment_at(
                directory.path().into(),
                awaken_time::system_clock(),
            );
            deployment.runtime.disable_local_pool = true;
            deployment.run_local_pool = false;
            let prepared = PreparedLocalWorker {
                resolver: None,
                stores: awaken_control::InferenceMaterializationStores {
                    credentials: Arc::new(InMemoryCredentialRepo::new()),
                    secrets: Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
                },
                admin_tools: Vec::new(),
                inference_override: None,
            };
            let prepared = if supplied_inference {
                prepared.with_inference_materializer(Arc::new(CompositionInference))
            } else {
                prepared
            };
            let worker = prepared
                .build_worker(
                    "http://127.0.0.1:1",
                    &deployment,
                    awaken_time::system_clock(),
                )
                .await
                .expect("W1 WorkerNode");
            assert!(
                worker
                    .manifest()
                    .capabilities
                    .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
                "W1/E4 Native composition retains the standard Resource adapter"
            );
            assert!(
                worker.manifest().capabilities.contains("native-runtime"),
                "W1"
            );
            assert!(
                worker
                    .manifest()
                    .capabilities
                    .contains(awaken_runtime_contract::A2A_RUNTIME_CAPABILITY),
                "W1"
            );
            assert!(
                worker
                    .manifest()
                    .capabilities
                    .iter()
                    .all(|capability| !capability.starts_with("acp:")),
                "W1"
            );
            assert_eq!(
                worker.manifest().capabilities.contains("composition-probe"),
                supplied_inference,
                "W2 product-selected inference reaches the actual registered Worker, not a discarded Coordinator context"
            );
        }
    }

    #[tokio::test]
    async fn startup_discovery_registers_one_idempotent_binding_and_builds_the_existing_worker() {
        // Cause graph: one detected catalog row -> one launch profile -> one
        // stable WorkerLocal locator -> the existing WorkerNode manifest. A
        // repeated startup reaches the same repository key.
        //
        // Decision table:
        // P1 detected+available -> route + binding + acp capability + the same
        // registration-bound Resource adapter used without ACP (W1/E4 above),
        // with the configured platform tier projected by the sole Host owner.
        // P2 repeated startup   -> same binding, no duplicate
        // P3 missing rows       -> diagnostics only, no route or binding
        // P4 explicitly unselected detected row -> diagnostic only, no acquisition
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = awaken_process_startup::config::local_test_deployment_at(
            directory.path().into(),
            awaken_time::system_clock(),
        );
        deployment.configured_acp_clis = Some(vec!["codex".to_string()]);
        deployment.runtime.acp = Some(
            awaken_acp_contract::AcpWorkerProfile::new(
                ["codex".to_string()],
                awaken_acp_catalog::acp_adapter_policy(),
            )
            .unwrap(),
        );
        let observations = awaken_acp_catalog::known_acp_clis()
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

        let workspace = awaken_installation_workspace::provision_at(
            &deployment.data_dir,
            awaken_deployment_config::mint_local_workspace,
        );
        let prepared = prepare_local_acp_with(
            &mut deployment,
            discovery.clone(),
            installer.clone(),
            fixed_negotiator(),
            stores.clone(),
            workspace.clone(),
            awaken_time::system_clock(),
        )
        .await
        .unwrap()
        .expect("P1");
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
            .build_worker(
                "http://127.0.0.1:1",
                &deployment,
                awaken_time::system_clock(),
            )
            .await
            .unwrap();
        assert!(worker.manifest().capabilities.contains("acp:codex"), "P1");
        assert!(
            worker
                .manifest()
                .capabilities
                .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
            "P1/E4 ACP composition retains the standard Resource adapter"
        );
        assert!(
            worker
                .manifest()
                .capabilities
                .iter()
                .all(|capability| !capability.starts_with("acp-capability/")),
            "P1 mutable negotiated fingerprints belong only to heartbeat observations"
        );
        assert!(
            worker
                .manifest()
                .sandbox_backends
                .contains(deployment.runtime.sandbox_support().1),
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
            workspace.clone(),
            awaken_time::system_clock(),
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
            Some(awaken_acp_contract::AcpCapabilityObservationState::Verified),
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
        // I4 Server + Workdir -> no personal identity startup
        let directory = tempfile::tempdir().unwrap();
        let mut deployment = awaken_process_startup::config::local_test_deployment_at(
            directory.path().into(),
            awaken_time::system_clock(),
        );
        deployment.runtime.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
        assert!(uses_trusted_local_identity(&deployment), "I1");

        deployment.runtime.sandbox_tier = awaken_deployment_config::SandboxTier::Namespace;
        assert!(uses_trusted_local_identity(&deployment), "I2");
        deployment.runtime.sandbox_tier = awaken_deployment_config::SandboxTier::Docker;
        assert!(uses_trusted_local_identity(&deployment), "I3");

        deployment.runtime.sandbox_tier = awaken_deployment_config::SandboxTier::Local;
        deployment.mode = awaken_process_startup::config::OperatingMode::Server;
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
        std::fs::write(
            directory.path().join("codex/package.json"),
            r#"{"dependencies":{"@agentclientprotocol/codex-acp":"1.10.0"}}"#,
        )
        .unwrap();

        #[cfg(windows)]
        let entry = {
            let package = directory
                .path()
                .join("codex/node_modules/@agentclientprotocol/codex-acp");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("package.json"),
                r#"{"version":"1.10.0","bin":{"codex-acp":"main.js"}}"#,
            )
            .unwrap();
            let entry = package.join("main.js");
            std::fs::write(&entry, "process.exit(0)").unwrap();
            entry
        };
        let cli = acp_cli("codex").unwrap();
        let first = awaken_run_executor_acp::host::NpmWrapperInstaller
            .resolved_argv(cli, directory.path())
            .await
            .expect("R1")
            .expect("R1 wrapper argv");
        let restarted = awaken_run_executor_acp::host::NpmWrapperInstaller
            .resolved_argv(cli, directory.path())
            .await
            .expect("R2")
            .expect("R2 wrapper argv");
        assert_eq!(first, restarted, "R2");
        #[cfg(not(windows))]
        assert_eq!(
            first,
            vec![executable.canonicalize().unwrap().to_string_lossy()]
        );
        #[cfg(windows)]
        {
            assert_eq!(first.len(), 2);
            assert!(std::path::Path::new(&first[0]).is_absolute());
            assert_eq!(first[1], entry.canonicalize().unwrap().to_string_lossy());
        }
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
        let mut deployment = awaken_process_startup::config::local_test_deployment_at(
            directory.path().into(),
            awaken_time::system_clock(),
        );
        let observations = awaken_acp_catalog::known_acp_clis()
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

        let workspace = awaken_installation_workspace::provision_at(
            &deployment.data_dir,
            awaken_deployment_config::mint_local_workspace,
        );
        let prepared = prepare_local_acp_with(
            &mut deployment,
            discovery,
            installer,
            fixed_negotiator(),
            stores,
            workspace.clone(),
            awaken_time::system_clock(),
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

#[cfg(test)]
#[test]
fn host_discovery_is_limited_to_host_backed_sandbox_tiers() {
    // Cause/effect table: D1 Local/Namespace + default or nonempty ACP list
    // -> discover; D2 explicit empty list -> no CLI process, acquisition, or
    // credential probing; D3 container tier -> no host discovery regardless of
    // list. An empty selection must not invoke discover_all before filtering:
    // some installed CLI launchers bootstrap packages even for --version.
    let directory = tempfile::tempdir().unwrap();
    let mut deployment = awaken_process_startup::config::local_test_deployment_at(
        directory.path().into(),
        awaken_time::system_clock(),
    );
    for tier in [
        awaken_deployment_config::SandboxTier::Local,
        awaken_deployment_config::SandboxTier::Namespace,
    ] {
        deployment.runtime.sandbox_tier = tier;
        deployment.configured_acp_clis = None;
        assert!(uses_host_acp_discovery(&deployment));
        deployment.configured_acp_clis = Some(vec!["codex".to_owned()]);
        assert!(uses_host_acp_discovery(&deployment));
        deployment.configured_acp_clis = Some(Vec::new());
        assert!(!uses_host_acp_discovery(&deployment));
    }
    for tier in [
        awaken_deployment_config::SandboxTier::Docker,
        awaken_deployment_config::SandboxTier::Podman,
        awaken_deployment_config::SandboxTier::K8s,
    ] {
        deployment.runtime.sandbox_tier = tier;
        for selection in [None, Some(Vec::new()), Some(vec!["codex".to_owned()])] {
            deployment.configured_acp_clis = selection;
            assert!(!uses_host_acp_discovery(&deployment));
        }
    }
}

#[cfg(test)]
mod registration_tests;
