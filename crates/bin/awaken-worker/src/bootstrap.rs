//! Authority-store-isolated Worker process configuration and assembly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

/// Worker-owned bootstrap settings also reused by the all-in-one startup.
#[derive(Debug, Clone)]
pub struct WorkerBootstrap {
    pub worker_id: String,
    pub request_credential_file: Option<PathBuf>,
    pub credential_material_root: PathBuf,
    pub credential_trust_domain: String,
    pub admin_listen: Option<String>,
    pub drain_grace_secs: u64,
    pub build_digest: Option<String>,
    pub zone: Option<String>,
    pub capabilities: Vec<String>,
    pub max_concurrent: Option<u32>,
    pub credential_probe_interval_secs: u64,
    pub credential_observation_ttl_secs: u64,
}

/// Optional authored Worker values resolved through one defaulting/validation
/// path by both the standalone artifact and the all-in-one startup.
#[derive(Debug, Clone, Default)]
pub struct WorkerBootstrapInput {
    pub worker_id: Option<String>,
    pub request_credential_file: Option<PathBuf>,
    pub credential_material_root: Option<PathBuf>,
    pub credential_trust_domain: Option<String>,
    pub admin_listen: Option<String>,
    pub drain_grace_secs: Option<u64>,
    pub build_digest: Option<String>,
    pub zone: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub max_concurrent: Option<u32>,
    pub credential_probe_interval_secs: Option<u64>,
    pub credential_observation_ttl_secs: Option<u64>,
}

impl WorkerBootstrap {
    pub fn resolve(input: WorkerBootstrapInput, data_dir: &Path) -> Result<Self, String> {
        let worker = Self {
            worker_id: input.worker_id.unwrap_or_else(|| "awaken-worker".into()),
            request_credential_file: input.request_credential_file,
            credential_material_root: input
                .credential_material_root
                .unwrap_or_else(|| data_dir.join("worker-credentials")),
            credential_trust_domain: input.credential_trust_domain.unwrap_or_else(|| {
                awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN.into()
            }),
            admin_listen: input.admin_listen.or_else(|| Some("0.0.0.0:9090".into())),
            drain_grace_secs: input.drain_grace_secs.unwrap_or(20),
            build_digest: input.build_digest,
            zone: input.zone,
            capabilities: input.capabilities.unwrap_or_default(),
            max_concurrent: input.max_concurrent,
            credential_probe_interval_secs: input.credential_probe_interval_secs.unwrap_or(10),
            credential_observation_ttl_secs: input.credential_observation_ttl_secs.unwrap_or(30),
        };
        if worker.worker_id.trim().is_empty() {
            return Err("worker_id must not be empty".into());
        }
        if worker.credential_material_root.as_os_str().is_empty() {
            return Err("worker_credential_material_root must not be empty".into());
        }
        if worker.credential_trust_domain.trim().is_empty() {
            return Err("worker_credential_trust_domain must not be empty".into());
        }
        crate::credential_liveness::validate_observation_timing(
            std::time::Duration::from_secs(worker.credential_probe_interval_secs),
            std::time::Duration::from_secs(worker.credential_observation_ttl_secs),
            None,
        )?;
        Ok(worker)
    }
}

/// Strict file shape accepted by the standalone Worker artifact. Authority DB,
/// Control seal and Coordinator trust fields are intentionally unrepresentable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFileConfig {
    data_dir: Option<PathBuf>,
    mode: Option<String>,
    role: Option<String>,
    worker_server: Option<String>,
    worker_server_ca_certificate_file: Option<PathBuf>,
    worker_id: Option<String>,
    worker_request_credential_file: Option<PathBuf>,
    worker_credential_material_root: Option<PathBuf>,
    worker_credential_trust_domain: Option<String>,
    worker_local_credential_bindings_file: Option<PathBuf>,
    worker_admin_listen: Option<String>,
    worker_drain_grace_secs: Option<u64>,
    worker_build_digest: Option<String>,
    worker_zone: Option<String>,
    worker_capabilities: Option<Vec<String>>,
    worker_max_concurrent: Option<u32>,
    worker_credential_probe_interval_secs: Option<u64>,
    worker_credential_observation_ttl_secs: Option<u64>,
    sandbox_tier: Option<String>,
    sandbox_dir: Option<PathBuf>,
    sandbox_trusted_host_enabled: Option<bool>,
    sandbox_trusted_host_scoped_execution: Option<bool>,
    sandbox_allow_local_fallback: Option<bool>,
    k8s_namespace: Option<String>,
    k8s_continuation_volume: Option<awaken_sandbox_container::K8sContinuationVolume>,
    acp_clis: Option<Vec<String>>,
    container_image: Option<String>,
}

/// Fully validated inputs for the standalone Worker process.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    server: String,
    server_ca_certificate_file: Option<PathBuf>,
    pub worker: WorkerBootstrap,
    runtime: awaken_session_executor::DeploymentConfig,
    local_credential_bindings_file: Option<PathBuf>,
}

impl WorkerDaemonConfig {
    pub fn load(path: &Path, server_override: Option<String>) -> Result<Self, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("read Worker config {}: {error}", path.display()))?;
        let file: WorkerFileConfig = toml::from_str(&source)
            .map_err(|error| format!("parse Worker config {}: {error}", path.display()))?;
        if file.role.as_deref().is_some_and(|role| role != "worker") {
            return Err("standalone Worker config requires role = \"worker\"".into());
        }
        if file.mode.as_deref().is_some_and(|mode| mode != "server") {
            return Err("standalone Worker config requires mode = \"server\"".into());
        }
        let server = server_override
            .or(file.worker_server)
            .ok_or_else(|| "Worker requires --server or worker_server".to_owned())?;
        if !(server.starts_with("http://") || server.starts_with("https://")) {
            return Err("Worker server must be an http:// or https:// URL".into());
        }
        let data_dir = file.data_dir.unwrap_or_else(|| PathBuf::from("."));
        let worker = WorkerBootstrap::resolve(
            WorkerBootstrapInput {
                worker_id: file.worker_id,
                request_credential_file: file.worker_request_credential_file,
                credential_material_root: file.worker_credential_material_root,
                credential_trust_domain: file.worker_credential_trust_domain,
                admin_listen: file.worker_admin_listen,
                drain_grace_secs: file.worker_drain_grace_secs,
                build_digest: file.worker_build_digest,
                zone: file.worker_zone,
                capabilities: file.worker_capabilities,
                max_concurrent: file.worker_max_concurrent,
                credential_probe_interval_secs: file.worker_credential_probe_interval_secs,
                credential_observation_ttl_secs: file.worker_credential_observation_ttl_secs,
            },
            &data_dir,
        )?;
        let mut runtime = awaken_session_executor::DeploymentConfig::ephemeral();
        // `data_dir` is the standalone Worker's installation-stable root. The
        // Runtime derives its durable workspace identity here rather than from
        // a process incarnation, so a restarted container Worker can discover
        // and reconcile the same physical Session realizations.
        runtime.storage_dir = Some(data_dir.clone());
        runtime.sandbox_tier = file
            .sandbox_tier
            .as_deref()
            .unwrap_or("namespace")
            .parse()?;
        runtime.sandbox_dir = file.sandbox_dir;
        runtime.sandbox.trusted_host_enabled = file.sandbox_trusted_host_enabled.unwrap_or(false);
        runtime.sandbox.trusted_host_scoped_execution =
            file.sandbox_trusted_host_scoped_execution.unwrap_or(false);
        runtime.validate_local_execution()?;
        runtime.sandbox.allow_local_fallback = file.sandbox_allow_local_fallback.unwrap_or(false);
        if let Some(namespace) = file.k8s_namespace {
            if namespace.trim().is_empty() {
                return Err("k8s_namespace must not be empty".into());
            }
            runtime.sandbox.k8s_namespace = namespace;
        }
        if let Some(policy) = file.k8s_continuation_volume {
            policy.validate().map_err(|error| error.to_string())?;
            runtime.sandbox.k8s_continuation_volume = Some(policy);
        }
        let acp_clis = file.acp_clis.unwrap_or_default();
        runtime.acp = (!acp_clis.is_empty())
            .then(|| awaken_session_executor::AcpWorkerProfile::new(acp_clis))
            .transpose()?;
        runtime.container_image = file.container_image;
        if let Some(path) = &file.worker_local_credential_bindings_file {
            if path.as_os_str().is_empty() {
                return Err("worker_local_credential_bindings_file must not be empty".into());
            }
            if !matches!(
                runtime.sandbox_tier,
                awaken_session_executor::SandboxTier::Local
                    | awaken_session_executor::SandboxTier::Namespace
            ) || !runtime.sandbox.trusted_host_enabled
                || runtime.acp.is_none()
            {
                return Err("local credential bindings require a host sandbox tier, sandbox_trusted_host_enabled=true and nonempty acp_clis".into());
            }
        }
        Ok(Self {
            server,
            server_ca_certificate_file: file.worker_server_ca_certificate_file,
            worker,
            runtime,
            local_credential_bindings_file: file.worker_local_credential_bindings_file,
        })
    }

    pub async fn build(&self) -> Result<crate::WorkerNode, String> {
        self.build_with_acp_adapters(awaken_run_executor_acp::host::HostAcpAdapters::production(
            Arc::new(awaken_protocol_acp::ProtocolAcpCapabilityHandshake),
        ))
        .await
    }

    // Test ports substitute only external discovery/acquisition/handshake;
    // authentication, profile selection and Worker assembly remain production.
    async fn build_with_acp_adapters(
        &self,
        adapters: awaken_run_executor_acp::host::HostAcpAdapters,
    ) -> Result<crate::WorkerNode, String> {
        let resolver = Arc::new(crate::WorkerCredentialFileResolver::new(
            &self.worker.credential_material_root,
            &self.worker.credential_trust_domain,
        ));
        let credentials =
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(resolver);
        let credential_file = self
            .worker
            .request_credential_file
            .as_deref()
            .ok_or_else(|| {
                "server-mode Worker requires worker_request_credential_file".to_owned()
            })?;
        let credential_source = std::fs::read_to_string(credential_file).map_err(|error| {
            format!(
                "read Worker request credential {}: {error}",
                credential_file.display()
            )
        })?;
        let transport_credentials =
            awaken_worker_transport_security::parse_projected_signing_credentials(
                &credential_source,
            )?;
        let upstream = match self.server_ca_certificate_file.as_deref() {
            Some(path) => {
                let certificate = std::fs::read(path).map_err(|error| {
                    format!(
                        "read Worker server CA certificate {}: {error}",
                        path.display()
                    )
                })?;
                awaken_worker_transport_security::WorkerUpstream::remote_with_ca_certificate(
                    &self.server,
                    &self.worker.worker_id,
                    transport_credentials,
                    &certificate,
                )?
            }
            None => awaken_worker_transport_security::WorkerUpstream::remote(
                &self.server,
                &self.worker.worker_id,
                transport_credentials,
            )?,
        };

        let mut manifest = self
            .worker
            .build_digest
            .clone()
            .map(crate::StandardManifestConfig::new)
            .unwrap_or_default()
            .with_extra_capabilities(self.worker.capabilities.clone());
        if let Some(zone) = &self.worker.zone {
            manifest = manifest.with_zone(zone);
        }
        if let Some(limit) = self.worker.max_concurrent {
            manifest = manifest.with_max_concurrent(limit);
        }
        let inference =
            awaken_credential_materializer::CredentialInferenceMaterializer::from_pinned(
                credentials.clone(),
            );
        let mut runtime = self.runtime.clone();
        let mut builder = crate::WorkerNodeBuilder::new(upstream);
        if matches!(
            runtime.sandbox_tier,
            awaken_session_executor::SandboxTier::Local | awaken_session_executor::SandboxTier::Namespace
        ) && let Some(profile) = &runtime.acp
        {
            use awaken_run_executor_acp::host::{
                HostAcpPreparation, ProjectedAcpCredentials, prepare_host_acp_capabilities_with,
            };
            let root = runtime
                .storage_dir
                .as_ref()
                .expect("validated Worker installation root");
            let prepared = prepare_host_acp_capabilities_with(
                HostAcpPreparation {
                    probe_cwd: root.join("acp-probe"),
                    wrapper_root: root.join("acp-wrappers"),
                    selected_cli_ids: Some(profile.cli_ids().map(str::to_string).collect()),
                },
                adapters.discovery,
                adapters.installer,
                adapters.negotiator,
            )
            .await?;
            let selected = prepared.routable_cli_ids();
            if selected.is_empty() {
                return Err("no configured host ACP CLI is currently routable".into());
            }
            let mut profile = awaken_session_executor::AcpWorkerProfile::new(selected.clone())?;
            profile.apply_launch_argv(&prepared.routable_launch_argv())?;
            runtime.acp = Some(profile);
            if let Some(path) = &self.local_credential_bindings_file {
                let projected = Arc::new(ProjectedAcpCredentials::new(
                    prepared.resolver,
                    path.clone(),
                    selected.into_iter().collect(),
                )?);
                projected
                    .refresh()
                    .await
                    .map_err(|error| format!("read local credential bindings: {error}"))?;
                builder = builder
                    .with_worker_local_credential_resolver(projected.clone())
                    .with_acp_capability_observation_source(projected);
            } else {
                builder = builder.with_acp_capability_observation_source(prepared.resolver);
            }
        }
        builder = builder
            .with_deployment_config(runtime)
            .with_standard_manifest_config(manifest)
            .with_inference_materializer(Arc::new(inference))
            .with_hand_executor_factory(crate::relay_hand_executor_factory())
            .with_credential_materializer(credentials)
            .with_remote_session_resource_support()
            .with_graceful_drain(std::time::Duration::from_secs(self.worker.drain_grace_secs))
            .with_credential_observation_window(
                std::time::Duration::from_secs(self.worker.credential_probe_interval_secs),
                std::time::Duration::from_secs(self.worker.credential_observation_ttl_secs),
            )
            .with_standard_manifest(Default::default());
        builder = match &self.worker.admin_listen {
            Some(address) => builder.with_admin_listen(address),
            None => builder.without_admin_surface(),
        };
        builder
            .prepare_session_environment_from_deployment()
            .await
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_binding_configuration_requires_explicit_host_consent() {
        // C1 nonempty path, C2 host tier, C3 explicit trusted-host consent,
        // C4 known/nonempty CLI selection. R1 all true => configuration accepted;
        // R2 any false => reject before IO/discovery; R3 no projection leaves
        // Native/container configuration unchanged. Cross every host/container
        // tier with both consent values and empty/valid/unknown CLI sets.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.toml");
        for tier in ["local", "namespace", "docker", "podman", "k8s"] {
            for consent in [false, true] {
                for clis in ["[]", "['codex', 'claude']", "['unknown']"] {
                    for binding in ["", "bindings.json"] {
                        std::fs::write(&path, format!(
                            "worker_server='http://coordinator:3000'\nsandbox_tier='{tier}'\nsandbox_trusted_host_enabled={consent}\nacp_clis={clis}\nworker_local_credential_bindings_file='{binding}'\n"
                        )).unwrap();
                        assert_eq!(
                            WorkerDaemonConfig::load(&path, None).is_ok(),
                            matches!(tier, "local" | "namespace")
                                && consent
                                && clis == "['codex', 'claude']"
                                && !binding.is_empty(),
                            "R1/R2 {tier} {consent} {clis} {binding}"
                        );
                    }
                }
            }
        }
    }

    struct HostFixture;
    struct MissingHostFixture;
    #[async_trait::async_trait]
    impl awaken_run_executor_acp::host::AcpDiscovery for MissingHostFixture {
        async fn discover(
            &self,
            cli: &awaken_run_executor_acp::AcpCli,
        ) -> awaken_run_executor_acp::host::AcpHostObservation {
            let mut observation =
                awaken_run_executor_acp::host::AcpDiscovery::discover(&HostFixture, cli).await;
            observation.detection = awaken_run_executor_acp::host::AcpDetectionState::Missing;
            observation.credential_state = None;
            observation
        }
    }
    #[async_trait::async_trait]
    impl awaken_run_executor_acp::host::AcpDiscovery for HostFixture {
        async fn discover(
            &self,
            cli: &awaken_run_executor_acp::AcpCli,
        ) -> awaken_run_executor_acp::host::AcpHostObservation {
            use awaken_run_executor_acp::host::*;
            AcpHostObservation {
                cli_id: cli.id.into(),
                display_name: cli.id.into(),
                detection: AcpDetectionState::Detected,
                version: Some("fixture".into()),
                credential_state: Some(
                    awaken_runtime_contract::CredentialObservationState::Available,
                ),
                reason_code: None,
                capability_state: None,
                capability_fingerprint: None,
                capability_reason_code: None,
            }
        }
    }
    #[async_trait::async_trait]
    impl awaken_run_executor_acp::host::AcpWrapperInstaller for HostFixture {
        async fn resolved_argv(
            &self,
            cli: &awaken_run_executor_acp::AcpCli,
            root: &Path,
        ) -> Result<Option<Vec<String>>, String> {
            Ok(Some(vec![root.join(cli.id).to_string_lossy().into_owned()]))
        }
    }
    #[async_trait::async_trait]
    impl awaken_acp_contract::AcpCapabilityNegotiator for HostFixture {
        async fn negotiate(
            &self,
            _: &[String],
            _: &Path,
            _: Option<&str>,
        ) -> Result<awaken_acp_contract::NegotiatedAcpCapabilities, String> {
            Ok(awaken_acp_contract::NegotiatedAcpCapabilities {
                protocol_version: "1".into(),
                load_session: true,
                prompt_image: false,
                prompt_audio: false,
                prompt_embedded_context: true,
                mcp_http: true,
                mcp_sse: false,
                session_list: false,
                modes: vec![],
                config_options: vec![],
            })
        }
    }
    fn host_adapters() -> awaken_run_executor_acp::host::HostAcpAdapters {
        awaken_run_executor_acp::host::HostAcpAdapters {
            discovery: Arc::new(HostFixture),
            installer: Arc::new(HostFixture),
            negotiator: Arc::new(HostFixture),
        }
    }

    #[tokio::test]
    async fn standalone_host_wiring_uses_one_profile_and_live_binding_projection() {
        // End-to-end composition decision table (external ACP IO is port-driven):
        // W1 Codex+Claude on Local/Namespace, explicit reference projection =>
        // installed exact resolver + capability source + matching pinned argv,
        // existing signed upstream and resource/materialization adapters retained.
        // W2 projection removed after build => next heartbeat empty and use denied.
        // W3 no binding file => protocol capabilities only, never synthetic login
        // authority. W4 malformed file => build rejects. Native/no ACP is covered
        // by standalone_config_boundary_decision_table, which uses public build().
        // W5 all selected CLIs missing => startup fails, never a fabricated route.
        use awaken_run_executor_acp::host::AcpLocalCredentialBinding;
        use awaken_runtime_contract::CredentialRef;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.toml");
        let signer = directory.path().join("signer.json");
        let bindings = directory.path().join("bindings.json");
        std::fs::write(&signer,
            r#"{"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#).unwrap();
        let rows = ["codex", "claude"].map(|cli| AcpLocalCredentialBinding {
            credential: CredentialRef {
                id: format!("ref-{cli}"),
                revision: 1,
            },
            cli_id: cli.into(),
            enabled: true,
        });
        for tier in ["local", "namespace"] {
            for projected in [true, false] {
                let projection = if projected {
                    format!(
                        "worker_local_credential_bindings_file='{}'\n",
                        bindings.display()
                    )
                } else {
                    String::new()
                };
                std::fs::write(&path, format!(
                    "worker_server='https://coordinator:3000'\nworker_id='worker-a'\ndata_dir='{}'\nworker_request_credential_file='{}'\nsandbox_tier='{tier}'\nsandbox_trusted_host_enabled=true\nacp_clis=['codex', 'claude']\n{projection}",
                    directory.path().display(), signer.display()
                )).unwrap();
                std::fs::write(&bindings, serde_json::to_vec(&rows).unwrap()).unwrap();
                let config = WorkerDaemonConfig::load(&path, None).unwrap();
                let mut missing = host_adapters();
                missing.discovery = Arc::new(MissingHostFixture);
                let error = config
                    .build_with_acp_adapters(missing)
                    .await
                    .err()
                    .expect("W5");
                assert!(error.contains("no configured host ACP CLI"), "W5 {error}");
                let worker = config
                    .build_with_acp_adapters(host_adapters())
                    .await
                    .unwrap();
                assert!(worker.manifest().capabilities.contains("acp:codex"));
                assert!(worker.manifest().capabilities.contains("acp:claude"));
                assert_eq!(
                    worker
                        .manifest()
                        .capabilities
                        .contains(awaken_worker_contract::WORKER_LOCAL_CREDENTIALS_CAPABILITY),
                    projected
                );
                assert!(
                    worker
                        .manifest()
                        .capabilities
                        .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY)
                );
                let profile = worker.deployment.acp.as_ref().unwrap();
                for cli in ["codex", "claude"] {
                    assert_eq!(
                        profile.launch_argv(cli).unwrap(),
                        [directory
                            .path()
                            .join("acp-wrappers")
                            .join(cli)
                            .to_string_lossy()
                            .to_string()]
                    );
                }
                let cache = crate::WorkerObservationCache::default();
                cache
                    .refresh(
                        worker.credential_observation_resolver.as_deref(),
                        worker.acp_capability_observation_source.as_deref(),
                        1,
                        std::time::Duration::from_secs(30),
                    )
                    .await
                    .unwrap();
                let heartbeat = cache
                    .snapshot()
                    .into_heartbeat(1, true, 0, Default::default());
                assert_eq!(heartbeat.acp_capability_observations.len(), 2);
                assert_eq!(
                    heartbeat.credential_observations.len(),
                    if projected { 2 } else { 0 }
                );
                if let Some(resolver) = &worker.credential_observation_resolver {
                    for row in &rows {
                        assert!(
                            resolver
                                .revalidate_worker_reference(&row.credential)
                                .await
                                .is_ok()
                        );
                    }
                    std::fs::remove_file(&bindings).unwrap();
                    cache
                        .refresh(
                            Some(resolver.as_ref()),
                            worker.acp_capability_observation_source.as_deref(),
                            2,
                            std::time::Duration::from_secs(30),
                        )
                        .await
                        .unwrap();
                    assert!(
                        cache
                            .snapshot()
                            .into_heartbeat(2, true, 0, Default::default())
                            .credential_observations
                            .is_empty()
                    );
                    assert!(
                        resolver
                            .revalidate_worker_reference(&rows[0].credential)
                            .await
                            .is_err(),
                        "W2"
                    );
                    std::fs::write(&bindings, b"invalid").unwrap();
                    assert!(
                        config
                            .build_with_acp_adapters(host_adapters())
                            .await
                            .is_err(),
                        "W4"
                    );
                }
            }
        }
    }

    #[test]
    fn worker_bootstrap_rejects_each_empty_identity_boundary() {
        /* Worker identity cause/effect decision table. C1 worker_id is nonblank,
         * C2 material root is nonempty, C3 trust domain is nonblank. E1 is one
         * fully typed bootstrap; E2 is a field-specific error before transport or
         * storage construction. Rules W1 C1+C2+C3=>E1; W2 !C1=>E2(worker_id);
         * W3 !C2=>E2(material root); W4 !C3=>E2(trust domain). */
        let data_dir = Path::new("/worker-data");
        assert!(
            WorkerBootstrap::resolve(WorkerBootstrapInput::default(), data_dir).is_ok(),
            "W1"
        );
        for (rule, input, field) in [
            (
                "W2",
                WorkerBootstrapInput {
                    worker_id: Some(" ".into()),
                    ..Default::default()
                },
                "worker_id",
            ),
            (
                "W3",
                WorkerBootstrapInput {
                    credential_material_root: Some(PathBuf::new()),
                    ..Default::default()
                },
                "worker_credential_material_root",
            ),
            (
                "W4",
                WorkerBootstrapInput {
                    credential_trust_domain: Some(" ".into()),
                    ..Default::default()
                },
                "worker_credential_trust_domain",
            ),
        ] {
            let error = WorkerBootstrap::resolve(input, data_dir).expect_err(rule);
            assert!(error.contains(field), "{rule}: {error}");
        }
    }

    /// Cause/effect decision table: C1 role is Worker, C2 mode is server, C3 no
    /// authority-only key is present, C4 server is valid, C5 probe TTL exceeds
    /// interval, C6 the projected signer exists and matches Worker identity, C7
    /// the sandbox tier belongs to the canonical runtime vocabulary, C8 the raw
    /// server endpoint is carried only by Worker startup and lowers once into the
    /// authenticated `WorkerUpstream` rather than being copied into Runtime axes,
    /// C9 `data_dir` is the installation-stable Runtime storage coordinate.
    /// R1 all true -> an isolated, resource/inference-capable Worker; R2
    /// !C1/!C2/!C4/!C5/!C7 -> config validation failure; R3 !C3 -> serde rejects the
    /// unknown authority field; R4 !C6 -> startup fails before any network
    /// activity. The built manifest is the terminal startup effect.
    #[tokio::test]
    async fn standalone_config_boundary_decision_table() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.toml");
        let write = |source: &str| std::fs::write(&path, source).unwrap();
        let credential = directory.path().join("worker.json");
        std::fs::write(
            &credential,
            r#"{"worker_id":"worker-a","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        write(&format!(
            "role='worker'\nmode='server'\ndata_dir='{}'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\n",
            directory.path().display(),
            credential.display()
        ));
        let resolved = WorkerDaemonConfig::load(&path, None).expect("R1");
        assert!(resolved.runtime.database_url.is_none(), "R1");
        assert_eq!(
            resolved.runtime.storage_dir.as_deref(),
            Some(directory.path()),
            "R1/C9 uses the authored installation root as durable Runtime identity"
        );
        let worker = resolved
            .build()
            .await
            .expect("R1 canonical Worker assembly");
        assert_eq!(
            worker.upstream.base_url(),
            "https://coordinator:3000",
            "R1/C8 installs the one authenticated upstream"
        );
        assert!(
            worker
                .manifest()
                .capabilities
                .contains(awaken_worker_contract::SESSION_RESOURCES_CAPABILITY),
            "R1"
        );

        // Sandbox configuration cause/effect decision table: S1 K8s tier plus
        // non-empty namespace and image preserves configuration but makes no
        // isolation claim before live provider attestation; S2 empty namespace
        // rejects; S3 the removed operator-evidence key rejects as unknown; S4
        // the trusted-host switch enters the typed Deployment value exactly.
        // The canonical K8s adapter is therefore the only evidence owner.
        write(&format!(
            "role='worker'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\nsandbox_tier='k8s'\nk8s_namespace='agents'\ncontainer_image='awaken-sandbox:local'\n",
            credential.display()
        ));
        let k8s = WorkerDaemonConfig::load(&path, None).expect("S1");
        assert_eq!(k8s.runtime.sandbox.k8s_namespace, "agents", "S1");
        assert_eq!(
            k8s.runtime.container_image.as_deref(),
            Some("awaken-sandbox:local"),
            "S1"
        );
        assert!(!k8s.runtime.sandbox_support().0.network_isolation, "S1");

        // KP1 omitted volume policy remains None. KP2 authored retained policy
        // crosses the standalone Worker file boundary into the same Runtime
        // setting; KP3 blank/missing size or unknown nested key rejects before
        // any build/network call. Class omission delegates admission to Kubernetes.
        assert!(k8s.runtime.sandbox.k8s_continuation_volume.is_none(), "KP1");
        for (policy, accepts, class) in [
            ("{ size = '1Gi' }", true, None),
            (
                "{ size = '1Gi', storage_class_name = 'retained' }",
                true,
                Some("retained"),
            ),
            ("{ size = '' }", false, None),
            ("{ storage_class_name = 'retained' }", false, None),
            ("{ size = '1Gi', unknown = true }", false, None),
        ] {
            write(&format!(
                "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_continuation_volume={policy}\n"
            ));
            let result = WorkerDaemonConfig::load(&path, None);
            assert_eq!(result.is_ok(), accepts, "KP {policy}: {result:?}");
            if let Ok(config) = result {
                assert_eq!(
                    config.runtime.sandbox.k8s_continuation_volume,
                    Some(awaken_sandbox_container::K8sContinuationVolume {
                        size: "1Gi".into(),
                        storage_class_name: class.map(str::to_owned),
                    }),
                    "KP2 exact policy"
                );
                assert!(
                    !config.runtime.sandbox_support().0.network_isolation,
                    "KP2 policy is not runtime attestation"
                );
            }
        }

        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_namespace='  '\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None).is_err(), "S2");
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_network_policy_enforcement='awaken-restricted-egress-v1'\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None).is_err(), "S3");
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='namespace'\nsandbox_trusted_host_enabled=true\n",
        );
        assert!(
            WorkerDaemonConfig::load(&path, None)
                .expect("S4")
                .runtime
                .trusted_host_enabled(),
            "S4"
        );
        // S5 explicit scoped selection shares Host's platform/consent rule;
        // S6 omitted selection stays off. No config value attests supervision.
        for (tier, trusted) in [("local", false), ("namespace", false), ("namespace", true)] {
            write(&format!(
                "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='{tier}'\nsandbox_trusted_host_enabled={trusted}\nsandbox_trusted_host_scoped_execution=true\n"
            ));
            let result = WorkerDaemonConfig::load(&path, None);
            let allowed = cfg!(target_os = "linux") && (tier == "local" || trusted);
            assert_eq!(result.is_ok(), allowed, "S5 {tier} {trusted}");
            if let Ok(config) = result {
                assert!(config.runtime.sandbox.trusted_host_scoped_execution);
                assert!(
                    !config
                        .runtime
                        .trusted_host_support()
                        .unwrap()
                        .process_tree_supervision
                );
            }
        }
        assert!(!k8s.runtime.sandbox.trusted_host_scoped_execution, "S6");
        assert!(
            worker
                .manifest()
                .capabilities
                .contains(awaken_worker_contract::PROVIDER_CREDENTIAL_SOURCE_CAPABILITY),
            "R1"
        );

        for source in [
            "role='control'\nworker_server='http://coordinator:3000'\n",
            "role='worker'\nmode='local'\nworker_server='http://coordinator:3000'\n",
            "role='worker'\nworker_server='postgres://authority'\n",
            "role='worker'\nworker_server='http://coordinator:3000'\nworker_credential_probe_interval_secs=10\nworker_credential_observation_ttl_secs=10\n",
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='vm'\n",
            "role='worker'\nworker_server='http://coordinator:3000'\nruntime_database_url='postgres://authority'\n",
        ] {
            write(source);
            assert!(WorkerDaemonConfig::load(&path, None).is_err(), "R2/R3");
        }

        std::fs::write(
            &credential,
            r#"{"worker_id":"worker-b","key_id":"key-a","credential_id":"credential-a","secret_base64":"c2VjcmV0"}"#,
        )
        .unwrap();
        write(&format!(
            "role='worker'\nmode='server'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\n",
            credential.display()
        ));
        let error = WorkerDaemonConfig::load(&path, None)
            .expect("R4 config syntax remains valid")
            .build()
            .await
            .err()
            .expect("R4 mismatched signer must fail startup");
        assert!(error.contains("configured worker_id"), "R4: {error}");

        let missing_credential = directory.path().join("missing-worker.json");
        write(&format!(
            "role='worker'\nmode='server'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\n",
            missing_credential.display()
        ));
        let error = WorkerDaemonConfig::load(&path, None)
            .expect("R4 missing projection is a startup-time cause")
            .build()
            .await
            .err()
            .expect("R4 missing signer must fail startup");
        assert!(
            error.contains("read Worker request credential"),
            "R4: {error}"
        );
    }
}
