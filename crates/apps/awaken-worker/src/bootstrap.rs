// @formal entrypoint worker.remote_transport_is_secure_exact_and_identity_bound
// @formal entrypoint worker_credential.projected_binding_use_rechecks_after_probe
//! Authority-store-isolated Worker process configuration and assembly.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_worker_transport_security::{WorkerBootstrap, WorkerBootstrapInput};
use serde::Deserialize;

const POD_IDENTITY_UNAVAILABLE: &str =
    "Pod-bound Worker transport requires the container-k8s build feature";

/// The Worker's Pod-bound transport integration, selected once by the
/// composition root and passed to configuration admission and upstream
/// construction. `None` means this Worker was composed without Kubernetes
/// identity support, so a Pod identity is refused before any projection I/O.
/// The refusal is a value, not a `cfg`: a dependency that enables the
/// transport's Kubernetes parser cannot reopen it.
#[derive(Clone, Copy)]
pub struct PodIdentity {
    remote_upstream: PodRemoteUpstream,
}

/// Build the authenticated remote upstream from a Pod projection directory,
/// the resolved Worker ID, the server URL and an optional private CA.
type PodRemoteUpstream = fn(
    &Path,
    &str,
    &str,
    Option<&[u8]>,
) -> Result<awaken_worker_transport_security::WorkerUpstream, String>;

impl std::fmt::Debug for PodIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PodIdentity")
    }
}

impl PodIdentity {
    /// The integration this Worker build carries: Kubernetes projected tokens
    /// with the `container-k8s` feature, otherwise none.
    #[must_use]
    pub fn installed() -> Option<Self> {
        #[cfg(feature = "container-k8s")]
        {
            Some(Self {
                remote_upstream: kubernetes_remote_upstream,
            })
        }
        #[cfg(not(feature = "container-k8s"))]
        {
            None
        }
    }
}

#[cfg(feature = "container-k8s")]
fn kubernetes_remote_upstream(
    directory: &Path,
    worker_id: &str,
    server: &str,
    server_ca_certificate: Option<&[u8]>,
) -> Result<awaken_worker_transport_security::WorkerUpstream, String> {
    let authorizer = awaken_worker_transport_security::kubernetes::KubernetesWorkerRequestAuthorizer::from_projected_directory(directory)?;
    if authorizer.pod().worker_id() != worker_id {
        return Err("Worker Pod projection changed after bootstrap resolution".into());
    }
    authorizer.into_remote_upstream(server, server_ca_certificate)
}

// Admit the process-owned capability before projection I/O or publication of
// a supposedly validated Worker configuration; parsing remains transport-owned.
fn resolve_worker_bootstrap(
    input: WorkerBootstrapInput,
    data_dir: &Path,
    pod_identity: Option<PodIdentity>,
) -> Result<WorkerBootstrap, String> {
    if input.kubernetes_identity_directory.is_some() && pod_identity.is_none() {
        return Err(POD_IDENTITY_UNAVAILABLE.into());
    }
    WorkerBootstrap::resolve(input, data_dir)
}

/// Explicit transport posture: embedding may use a trusted local channel;
/// standalone Workers must use the canonical authenticated HTTPS constructor.
pub enum WorkerUpstreamSecurity<'a> {
    Local,
    Remote {
        server_ca_certificate: Option<&'a [u8]>,
    },
}

/// The Worker's upstream to its Coordinator, built from its resolved
/// bootstrap. One credential-loading boundary for standalone and embedded startup.
/// Credential parsing and secure remote admission remain transport-owned.
/// `pod_identity` is the composition root's Pod integration (see
/// [`PodIdentity`]). `clock` is the Worker process's one clock; its signed
/// request assertions are stamped with it.
pub fn worker_upstream(
    bootstrap: &WorkerBootstrap,
    server: impl Into<String>,
    security: WorkerUpstreamSecurity<'_>,
    pod_identity: Option<PodIdentity>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<awaken_worker_transport_security::WorkerUpstream, String> {
    let server = server.into();
    if let Some(directory) = &bootstrap.kubernetes_identity_directory {
        if bootstrap.request_credential_file.is_some() {
            return Err("Worker cannot mix Pod-bound tokens and signing credentials".into());
        }
        let pod_identity = pod_identity.ok_or(POD_IDENTITY_UNAVAILABLE)?;
        let ca = match security {
            WorkerUpstreamSecurity::Local => None,
            WorkerUpstreamSecurity::Remote {
                server_ca_certificate,
            } => server_ca_certificate,
        };
        return (pod_identity.remote_upstream)(directory, &bootstrap.worker_id, &server, ca);
    }
    use awaken_worker_transport_security::{
        WorkerUpstream, parse_projected_signing_credentials, projected_request_authorizer,
    };
    let source = bootstrap
        .request_credential_file
        .as_ref()
        .map(|path| {
            std::fs::read_to_string(path).map_err(|error| {
                format!("read Worker request credential {}: {error}", path.display())
            })
        })
        .transpose()?;
    match security {
        WorkerUpstreamSecurity::Local => {
            let upstream = WorkerUpstream::new(server).with_worker_id(&bootstrap.worker_id);
            match source {
                Some(source) => Ok(
                    upstream.with_request_authorizer(projected_request_authorizer(
                        &source,
                        &bootstrap.worker_id,
                        clock,
                    )?),
                ),
                None => Ok(upstream),
            }
        }
        WorkerUpstreamSecurity::Remote {
            server_ca_certificate,
        } => {
            let source = source.ok_or_else(|| {
                "server-mode Worker requires worker_request_credential_file".to_owned()
            })?;
            let credentials = parse_projected_signing_credentials(&source)?;
            match server_ca_certificate {
                Some(ca) => WorkerUpstream::remote_with_ca_certificate(
                    server,
                    &bootstrap.worker_id,
                    credentials,
                    ca,
                    clock,
                ),
                None => WorkerUpstream::remote(server, &bootstrap.worker_id, credentials, clock),
            }
        }
    }
}

/// Strict file shape accepted by the standalone Worker artifact. Authority DB,
/// Control seal and Coordinator trust fields are intentionally unrepresentable.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFileConfig {
    log_filter: Option<String>,
    data_dir: Option<PathBuf>,
    mode: Option<String>,
    role: Option<String>,
    worker_server: Option<String>,
    worker_server_ca_certificate_file: Option<PathBuf>,
    worker_id: Option<String>,
    worker_request_credential_file: Option<PathBuf>,
    worker_kubernetes_identity_directory: Option<PathBuf>,
    worker_credential_material_root: Option<PathBuf>,
    credential_envelope_keys_file: Option<PathBuf>,
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
    k8s_unschedulable_wait_secs: Option<u64>,
    k8s_continuation_volume: Option<awaken_container_contract::K8sContinuationVolume>,
    k8s_image_prewarm: Option<awaken_container_contract::K8sImagePrewarm>,
    k8s_image_pull_secrets: Option<Vec<String>>,
    acp_clis: Option<Vec<String>>,
    container_image: Option<String>,
    package_image_registry: Option<String>,
    package_registry_auth_file: Option<PathBuf>,
    package_registry_insecure: Option<bool>,
    package_build_host_network: Option<bool>,
    k8s_kaniko_image: Option<String>,
    k8s_package_push_secret: Option<String>,
    package_image_builder: Option<String>,
    package_local_cache_ttl_secs: Option<u64>,
    sandbox_inherit_agent_stderr: Option<bool>,
}

/// Fully validated inputs for the standalone Worker process.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    pub observability: awaken_observability::ObservabilityConfig,
    server: String,
    server_ca_certificate_file: Option<PathBuf>,
    pub worker: WorkerBootstrap,
    runtime: awaken_deployment_config::DeploymentConfig,
    local_credential_bindings_file: Option<PathBuf>,
    pod_identity: Option<PodIdentity>,
}

impl WorkerDaemonConfig {
    pub fn load(
        path: &Path,
        server_override: Option<String>,
        pod_identity: Option<PodIdentity>,
    ) -> Result<Self, String> {
        let source = std::fs::read_to_string(path)
            .map_err(|error| format!("read Worker config {}: {error}", path.display()))?;
        let file: WorkerFileConfig = toml::from_str(&source)
            .map_err(|error| format!("parse Worker config {}: {error}", path.display()))?;
        let mut observability = awaken_observability::ObservabilityConfig::default();
        if let Some(filter) = file.log_filter {
            if filter.trim().is_empty() {
                return Err("log_filter must not be empty".into());
            }
            observability.filter = filter;
        }
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
        let worker = resolve_worker_bootstrap(
            WorkerBootstrapInput {
                worker_id: file.worker_id,
                request_credential_file: file.worker_request_credential_file,
                kubernetes_identity_directory: file.worker_kubernetes_identity_directory,
                credential_material_root: file.worker_credential_material_root,
                credential_envelope_keys_file: file.credential_envelope_keys_file,
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
            pod_identity,
        )?;
        let mut runtime = awaken_deployment_config::DeploymentConfig::ephemeral();
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
        let tier = runtime.sandbox_tier;
        runtime
            .sandbox
            .apply_k8s_unschedulable_wait_secs(tier, file.k8s_unschedulable_wait_secs)?;
        if let Some(policy) = file.k8s_image_prewarm {
            policy.validate()?;
            if runtime.sandbox_tier != awaken_deployment_config::SandboxTier::K8s {
                return Err("k8s_image_prewarm requires sandbox_tier=k8s".into());
            }
            runtime.sandbox.k8s_image_prewarm = Some(policy);
        }
        if let Some(policy) = file.k8s_continuation_volume {
            policy.validate().map_err(|error| error.to_string())?;
            runtime.sandbox.k8s_continuation_volume = Some(policy);
        }
        awaken_deployment_config::SandboxPackageSettingsInput {
            k8s_image_pull_secrets: file.k8s_image_pull_secrets.unwrap_or_default(),
            package_image_registry: file.package_image_registry,
            package_registry_auth_file: file.package_registry_auth_file,
            package_registry_insecure: file.package_registry_insecure.unwrap_or(false),
            package_build_host_network: file.package_build_host_network.unwrap_or(false),
            k8s_kaniko_image: file.k8s_kaniko_image,
            k8s_package_push_secret: file.k8s_package_push_secret,
            package_image_builder: file.package_image_builder,
            package_local_cache_ttl_secs: file.package_local_cache_ttl_secs,
            inherit_agent_stderr: file.sandbox_inherit_agent_stderr.unwrap_or(false),
        }
        .apply(runtime.sandbox_tier, &mut runtime.sandbox)?;
        let acp_clis = file.acp_clis.unwrap_or_default();
        runtime.acp = (!acp_clis.is_empty())
            .then(|| {
                awaken_acp_contract::AcpWorkerProfile::new(
                    acp_clis,
                    awaken_acp_catalog::acp_adapter_policy(),
                )
            })
            .transpose()?;
        runtime.container_image = file.container_image;
        if let Some(path) = &file.worker_local_credential_bindings_file {
            if path.as_os_str().is_empty() {
                return Err("worker_local_credential_bindings_file must not be empty".into());
            }
            if !matches!(
                runtime.sandbox_tier,
                awaken_deployment_config::SandboxTier::Local
                    | awaken_deployment_config::SandboxTier::Namespace
            ) || !runtime.sandbox.trusted_host_enabled
                || runtime.acp.is_none()
            {
                return Err("local credential bindings require a host sandbox tier, sandbox_trusted_host_enabled=true and nonempty acp_clis".into());
            }
        }
        Ok(Self {
            observability,
            server,
            server_ca_certificate_file: file.worker_server_ca_certificate_file,
            worker,
            runtime,
            local_credential_bindings_file: file.worker_local_credential_bindings_file,
            pod_identity,
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
        // The Worker daemon's one wall clock: its request assertions, its
        // credential envelopes and the Host it builds (dispatch pool, recovery,
        // children) share it.
        let clock = awaken_time::system_clock();
        let resolver: Arc<dyn awaken_credential_contract::CredentialMaterialResolver> =
            match &self.worker.credential_envelope_keys_file {
                Some(path) => Arc::new(
                    awaken_credential_envelope::RecipientKeyEnvelope::open(path, clock.clone())
                        .map_err(|error| {
                            format!("Worker credential envelope configuration: {error}")
                        })?,
                ),
                None => Arc::new(crate::WorkerCredentialFileResolver::new(
                    &self.worker.credential_material_root,
                    &self.worker.credential_trust_domain,
                    clock.clone(),
                )),
            };
        let credentials =
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                resolver,
                clock.clone(),
            );
        let certificate = self
            .server_ca_certificate_file
            .as_ref()
            .map(|path| {
                std::fs::read(path).map_err(|error| {
                    format!(
                        "read Worker server CA certificate {}: {error}",
                        path.display()
                    )
                })
            })
            .transpose()?;
        let upstream = worker_upstream(
            &self.worker,
            &self.server,
            WorkerUpstreamSecurity::Remote {
                server_ca_certificate: certificate.as_deref(),
            },
            self.pod_identity,
            clock.clone(),
        )?;

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
        let mut builder = crate::WorkerNodeBuilder::new(upstream, clock.clone());
        if matches!(
            runtime.sandbox_tier,
            awaken_deployment_config::SandboxTier::Local
                | awaken_deployment_config::SandboxTier::Namespace
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
                    clock: clock.clone(),
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
            let mut profile = awaken_acp_contract::AcpWorkerProfile::new(
                selected.clone(),
                awaken_acp_catalog::acp_adapter_policy(),
            )?;
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
            .with_deployment_config(runtime, self.observability.trace_file.is_some())
            .with_standard_manifest_config(manifest)
            .with_inference_materializer(Arc::new(inference))
            .with_hand_executor_factory(awaken_tool_relay::relay_hand_executor_factory())
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
    fn standalone_image_prewarm_preserve_policy_and_reject_invalid_inputs() {
        // Same cause/effect rules as Control CLI configuration: omitted=>disabled;
        // valid+k8s=>exact policy; zero budgets/foreign backend/unknown key=>error.
        // A temp file exercises the actual standalone strict reader, no network.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.toml");
        for (tier, policy, accepts) in [
            ("k8s", None, true),
            ("k8s", Some("{max_images=8}"), true),
            ("k8s", Some("{max_images=0}"), false),
            ("k8s", Some("{max_images=257}"), false),
            ("docker", Some("{}"), false),
            ("k8s", Some("{unknown=true}"), false),
        ] {
            let mut source = format!(
                "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='{tier}'\n"
            );
            if let Some(policy) = policy {
                source.push_str(&format!("k8s_image_prewarm={policy}\n"));
            }
            std::fs::write(&path, source).unwrap();
            let result = WorkerDaemonConfig::load(&path, None, None);
            assert_eq!(result.is_ok(), accepts, "{tier}/{policy:?}");
            if let Ok(config) = result {
                assert_eq!(
                    config.runtime.sandbox.k8s_image_prewarm.is_some(),
                    policy.is_some()
                );
            }
        }
    }

    #[test]
    fn standalone_log_filter_uses_typed_observability_policy() {
        // Decision table: L1 omitted filter keeps the shared defaults; L2 an
        // explicit target directive is preserved exactly; L3 empty/whitespace
        // rejects before Worker assembly or network IO. The formatter/exporter
        // remain shared defaults, not a second standalone logging mechanism.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.toml");
        for (authored, expected) in [
            ("", Some("info")),
            (
                "log_filter='info,awaken_dispatch::pool=debug'",
                Some("info,awaken_dispatch::pool=debug"),
            ),
            ("log_filter='off'", Some("off")),
            ("log_filter=''", None),
            ("log_filter='  '", None),
        ] {
            std::fs::write(
                &path,
                format!("worker_server='http://coordinator'\n{authored}\n"),
            )
            .unwrap();
            let result = WorkerDaemonConfig::load(&path, None, None);
            match expected {
                Some(filter) => assert_eq!(
                    result.unwrap().observability,
                    awaken_observability::ObservabilityConfig {
                        filter: filter.into(),
                        ..Default::default()
                    },
                    "L1/L2 {authored}",
                ),
                None => assert!(
                    result.unwrap_err().contains("log_filter must not be empty"),
                    "L3 {authored}"
                ),
            }
        }
    }

    #[cfg(feature = "container-k8s")]
    #[test]
    fn pod_bootstrap_freezes_projected_identity_and_never_falls_back_to_signing() {
        // P1 installed integration + complete projection => derive exact Worker
        // ID and use the existing HTTPS upstream with a rotating Pod bearer. P2
        // missing metadata/mismatched authored ID => reject. P3 signer and Pod
        // token together => reject even after public config mutation. P4
        // metadata replacement after resolve => reject; rotating token alone
        // remains supported by the transport's own decision table. The absent
        // integration is the plain unit test below.
        let pod = PodIdentity::installed();
        assert!(
            pod.is_some(),
            "the container-k8s Worker carries its Pod integration"
        );
        let directory = tempfile::tempdir().unwrap();
        let input = || WorkerBootstrapInput {
            kubernetes_identity_directory: Some(directory.path().into()),
            ..Default::default()
        };
        assert!(
            resolve_worker_bootstrap(input(), directory.path(), pod).is_err(),
            "P2 missing"
        );
        for (name, value) in [
            ("namespace", "workers"),
            ("name", "worker-a"),
            ("uid", "uid-a"),
            ("token", "fixture-token"),
        ] {
            std::fs::write(directory.path().join(name), value).unwrap();
        }
        // P5 the standalone TOML entry must forward the same projection into
        // the shared resolver; a parsed-but-discarded field would select the
        // default Worker identity and lose Pod authentication at deployment.
        let config_path = directory.path().join("worker.toml");
        std::fs::write(
            &config_path,
            format!(
                "role = \"worker\"\nmode = \"server\"\nworker_server = \"https://coordinator\"\nsandbox_tier = \"k8s\"\nworker_kubernetes_identity_directory = {:?}",
                directory.path().to_str().unwrap()
            ),
        )
        .unwrap();
        let loaded = WorkerDaemonConfig::load(&config_path, None, pod).expect("P5");
        assert_eq!(loaded.worker.worker_id, "k8s:workers:worker-a:uid-a", "P5");
        assert_eq!(
            loaded.worker.kubernetes_identity_directory.as_deref(),
            Some(directory.path()),
            "P5"
        );
        let mut worker = resolve_worker_bootstrap(input(), directory.path(), pod).expect("P1");
        assert_eq!(worker.worker_id, "k8s:workers:worker-a:uid-a");
        let upstream = worker_upstream(
            &worker,
            "https://coordinator",
            WorkerUpstreamSecurity::Local,
            pod,
            awaken_time::system_clock(),
        )
        .unwrap();
        let request = upstream
            .authorize_request(
                "POST",
                "/v1/worker/register",
                upstream
                    .client()
                    .post("https://coordinator/v1/worker/register"),
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            request.headers()["authorization"],
            "Bearer fixture-token",
            "P1"
        );
        assert!(
            worker_upstream(
                &worker,
                "http://coordinator",
                WorkerUpstreamSecurity::Local,
                pod,
                awaken_time::system_clock()
            )
            .is_err(),
            "P1 still HTTPS"
        );
        let mut mismatch = input();
        mismatch.worker_id = Some("foreign".into());
        assert!(
            resolve_worker_bootstrap(mismatch, directory.path(), pod).is_err(),
            "P2 mismatch"
        );
        worker.request_credential_file = Some(directory.path().join("signer"));
        assert!(
            worker_upstream(
                &worker,
                "https://coordinator",
                WorkerUpstreamSecurity::Local,
                pod,
                awaken_time::system_clock()
            )
            .is_err(),
            "P3 mutation"
        );
        worker.request_credential_file = None;
        std::fs::write(directory.path().join("uid"), "replacement").unwrap();
        assert!(
            worker_upstream(
                &worker,
                "https://coordinator",
                WorkerUpstreamSecurity::Local,
                pod,
                awaken_time::system_clock()
            )
            .is_err(),
            "P4"
        );
        let mut mixed = input();
        mixed.request_credential_file = Some(directory.path().join("signer"));
        assert!(
            resolve_worker_bootstrap(mixed, directory.path(), pod).is_err(),
            "P3"
        );
    }

    #[test]
    fn unavailable_pod_identity_rejects_before_projection_io_and_after_dto_mutation() {
        // U1 the composition installed no Pod integration (`None`), whatever
        // features a dependency enabled; U2 the projection does not exist; U3
        // a caller mutates the compatibility DTO after parsing. U1+U2 must
        // reject at Worker configuration admission before projection I/O.
        // U1+U3 must also reject at use, without selecting signing or local
        // auth. U4 an installed integration, not a feature, builds the upstream.
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-projection");
        let error = resolve_worker_bootstrap(
            WorkerBootstrapInput {
                kubernetes_identity_directory: Some(missing.clone()),
                ..Default::default()
            },
            directory.path(),
            None,
        )
        .unwrap_err();
        assert!(error.contains("container-k8s"), "U1/U2 {error}");
        assert!(!missing.exists(), "the projection was never provisioned");

        let config = directory.path().join("worker.toml");
        std::fs::write(
            &config,
            format!(
                "role = \"worker\"\nmode = \"server\"\nworker_server = \"https://coordinator\"\nworker_kubernetes_identity_directory = {:?}",
                missing.to_str().unwrap()
            ),
        )
        .unwrap();
        let error = WorkerDaemonConfig::load(&config, None, None).unwrap_err();
        assert!(
            error.contains("container-k8s"),
            "configuration admission: {error}"
        );

        let mut forged = WorkerBootstrap::resolve(
            WorkerBootstrapInput {
                worker_id: Some("worker-a".into()),
                ..Default::default()
            },
            directory.path(),
        )
        .unwrap();
        forged.kubernetes_identity_directory = Some(missing);
        let error = worker_upstream(
            &forged,
            "https://coordinator",
            WorkerUpstreamSecurity::Local,
            None,
            awaken_time::system_clock(),
        )
        .err()
        .expect("U3 compatibility DTO mutation cannot grant an unavailable integration");
        assert!(error.contains("container-k8s"), "U3 {error}");

        let installed = PodIdentity {
            remote_upstream: |_, worker_id, _, _| {
                Err(format!("installed Pod upstream for {worker_id}"))
            },
        };
        let error = worker_upstream(
            &forged,
            "https://coordinator",
            WorkerUpstreamSecurity::Local,
            Some(installed),
            awaken_time::system_clock(),
        )
        .err()
        .expect("U4 the fixture integration refuses");
        assert_eq!(error, "installed Pod upstream for worker-a", "U4");
    }

    #[test]
    fn shared_bootstrap_keeps_local_and_remote_security_postures_distinct() {
        // B1 local + absent signer => explicit header transport. B2 remote +
        // absent signer => reject. B3 exact signer => both construct; only
        // remote requires HTTPS. B4 missing/malformed/mismatched signer => both
        // reject without fallback. B5 invalid private CA => remote rejects.
        // This is the shared decision boundary used by both startup callers.
        let directory = tempfile::tempdir().unwrap();
        let mut worker = WorkerBootstrap::resolve(
            WorkerBootstrapInput {
                worker_id: Some("worker-a".into()),
                ..Default::default()
            },
            directory.path(),
        )
        .unwrap();
        let remote = || WorkerUpstreamSecurity::Remote {
            server_ca_certificate: None,
        };
        assert!(
            worker_upstream(
                &worker,
                "http://coordinator",
                WorkerUpstreamSecurity::Local,
                None,
                awaken_time::system_clock()
            )
            .is_ok(),
            "B1"
        );
        assert!(
            worker_upstream(
                &worker,
                "https://coordinator",
                remote(),
                None,
                awaken_time::system_clock()
            )
            .is_err(),
            "B2"
        );
        let file = directory.path().join("credential.json");
        worker.request_credential_file = Some(file.clone());
        let valid = r#"{"worker_id":"worker-a","key_id":"key","credential_id":"credential","secret_base64":"c2VjcmV0"}"#;
        for source in [None, Some("not-json"), Some("[]"), Some(valid)] {
            if let Some(source) = source {
                std::fs::write(&file, source).unwrap();
            }
            for is_remote in [false, true] {
                let security = if is_remote {
                    remote()
                } else {
                    WorkerUpstreamSecurity::Local
                };
                assert_eq!(
                    worker_upstream(
                        &worker,
                        "https://coordinator",
                        security,
                        None,
                        awaken_time::system_clock()
                    )
                    .is_ok(),
                    source == Some(valid),
                    "B3/B4 {is_remote}"
                );
            }
        }
        assert!(
            worker_upstream(
                &worker,
                "http://coordinator",
                WorkerUpstreamSecurity::Local,
                None,
                awaken_time::system_clock()
            )
            .is_ok(),
            "B3 local"
        );
        assert!(
            worker_upstream(
                &worker,
                "http://coordinator",
                remote(),
                None,
                awaken_time::system_clock()
            )
            .is_err(),
            "B3 remote"
        );
        assert!(
            worker_upstream(
                &worker,
                "https://coordinator",
                WorkerUpstreamSecurity::Remote {
                    server_ca_certificate: Some(b"invalid")
                },
                None,
                awaken_time::system_clock()
            )
            .is_err(),
            "B5"
        );
        worker.worker_id = "other".into();
        for security in [WorkerUpstreamSecurity::Local, remote()] {
            assert!(
                worker_upstream(
                    &worker,
                    "https://coordinator",
                    security,
                    None,
                    awaken_time::system_clock()
                )
                .is_err(),
                "B4 mismatch"
            );
        }
    }

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
                            WorkerDaemonConfig::load(&path, None, None).is_ok(),
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
            cli: &awaken_acp_catalog::AcpCli,
        ) -> awaken_acp_contract::AcpHostObservation {
            let mut observation =
                awaken_run_executor_acp::host::AcpDiscovery::discover(&HostFixture, cli).await;
            observation.detection = awaken_acp_contract::AcpDetectionState::Missing;
            observation.credential_state = None;
            observation
        }
    }
    #[async_trait::async_trait]
    impl awaken_run_executor_acp::host::AcpDiscovery for HostFixture {
        async fn discover(
            &self,
            cli: &awaken_acp_catalog::AcpCli,
        ) -> awaken_acp_contract::AcpHostObservation {
            use awaken_acp_contract::{AcpDetectionState, AcpHostObservation};
            AcpHostObservation {
                cli_id: cli.id.into(),
                display_name: cli.id.into(),
                detection: AcpDetectionState::Detected,
                version: Some("fixture".into()),
                credential_state: Some(
                    awaken_credential_contract::CredentialObservationState::Available,
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
            cli: &awaken_acp_catalog::AcpCli,
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
        // W6 Windows Namespace is unsupported => startup rejects explicitly;
        // Local still executes the full wiring/projection table on every OS.
        use awaken_acp_contract::AcpLocalCredentialBinding;
        use awaken_agent_contract::credential::CredentialRef;
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
                let config = WorkerDaemonConfig::load(&path, None, None).unwrap();
                let mut missing = host_adapters();
                missing.discovery = Arc::new(MissingHostFixture);
                let error = config
                    .build_with_acp_adapters(missing)
                    .await
                    .err()
                    .expect("W5");
                assert!(error.contains("no configured host ACP CLI"), "W5 {error}");
                let worker = config.build_with_acp_adapters(host_adapters()).await;
                if tier == "namespace" && cfg!(target_os = "windows") {
                    let error = worker
                        .err()
                        .expect("W6 unsupported Namespace rejects startup");
                    assert!(
                        error.contains(
                            "OS-native Namespace sandbox is unsupported on this platform"
                        ),
                        "W6: {error}"
                    );
                    continue;
                }
                let worker = worker.expect("W1 supported backend assembly");
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
                        &awaken_lease_testkit::ManualClock::new(1),
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
                            &awaken_lease_testkit::ManualClock::new(2),
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
        let default_source = format!(
            "role='worker'\nmode='server'\ndata_dir='{}'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\n",
            directory.path().display(),
            credential.display()
        );
        write(&default_source);
        assert_eq!(
            WorkerDaemonConfig::load(&path, None, None)
                .expect("R1 default syntax")
                .runtime
                .sandbox_tier,
            awaken_deployment_config::SandboxTier::Namespace,
            "omitted tier preserves the Native default"
        );
        // R1 owns portable configuration/assembly. Native backend availability
        // is independently required by the platform portability gate.
        write(&format!("{default_source}sandbox_tier='local'\n"));
        let resolved = WorkerDaemonConfig::load(&path, None, None).expect("R1");
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

        // PBN: standalone default is false; explicit true crosses the same typed
        // settings boundary without invoking a builder or changing Session networking.
        assert!(!resolved.runtime.sandbox.package_build_host_network);
        write(&format!(
            "role='worker'\nworker_server='https://coordinator:3000'\nworker_request_credential_file='{}'\nsandbox_tier='docker'\npackage_build_host_network=true\n",
            credential.display()
        ));
        let package_host =
            WorkerDaemonConfig::load(&path, None, None).expect("PBN host build config");
        assert!(package_host.runtime.sandbox.package_build_host_network);

        // Sandbox configuration cause/effect decision table: S1 K8s tier plus
        // non-empty namespace, image and package policy preserves configuration but makes no
        // isolation claim before live provider attestation; S2 empty namespace
        // rejects; S3 the removed operator-evidence key rejects as unknown; S4
        // the trusted-host switch enters the typed Deployment value exactly.
        // The canonical K8s adapter is therefore the only evidence owner.
        write(&format!(
            "role='worker'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\nsandbox_tier='k8s'\nk8s_namespace='agents'\nk8s_image_pull_secrets=['registry-auth']\nk8s_kaniko_image='mirror/kaniko:debug'\nk8s_package_push_secret='registry-push'\ncontainer_image='awaken-sandbox:local'\npackage_image_registry='registry.example/team/'\npackage_image_builder='k8s'\nsandbox_inherit_agent_stderr=true\n", // awaken-allow(secret): Kubernetes Secret name fixture, not credential material.
            credential.display()
        ));
        let k8s = WorkerDaemonConfig::load(&path, None, None).expect("S1");
        assert_eq!(k8s.runtime.sandbox.k8s_namespace, "agents", "S1");
        assert_eq!(
            k8s.runtime.container_image.as_deref(),
            Some("awaken-sandbox:local"),
            "S1"
        );
        assert!(!k8s.runtime.sandbox_support().0.network_isolation, "S1");
        assert_eq!(
            k8s.runtime.sandbox.k8s_image_pull_secrets,
            ["registry-auth"],
            "S1"
        );
        assert_eq!(
            k8s.runtime.sandbox.package_image_registry.as_deref(),
            Some("registry.example/team"),
            "S1"
        );
        assert_eq!(
            k8s.runtime.sandbox.package_image_builder,
            Some(awaken_deployment_config::PackageImageBuilder::Kubernetes),
            "S1"
        );
        // S1 independent pull/push coordinates and selected executor cross the
        // standalone file -> shared SandboxSettings path without API effects.
        assert_eq!(
            k8s.runtime.sandbox.k8s_kaniko_image, "mirror/kaniko:debug",
            "S1"
        );
        assert_eq!(
            k8s.runtime.sandbox.k8s_package_push_secret.as_deref(),
            Some("registry-push"),
            "S1"
        );
        assert!(k8s.runtime.sandbox.inherit_agent_stderr, "S1");

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
            let result = WorkerDaemonConfig::load(&path, None, None);
            assert_eq!(result.is_ok(), accepts, "KP {policy}: {result:?}");
            if let Ok(config) = result {
                assert_eq!(
                    config.runtime.sandbox.k8s_continuation_volume,
                    Some(awaken_container_contract::K8sContinuationVolume {
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

        assert_eq!(
            k8s.runtime.sandbox.k8s_unschedulable_wait_secs, 300,
            "S5 default"
        );
        // S5 the authored scheduler wait reaches the typed Deployment value;
        // a value over one day or on another tier is refused.
        for (body, expected) in [
            (
                "sandbox_tier='k8s'\nk8s_unschedulable_wait_secs=45\n",
                Some(45),
            ),
            (
                "sandbox_tier='k8s'\nk8s_unschedulable_wait_secs=86401\n",
                None,
            ),
            (
                "sandbox_tier='docker'\nk8s_unschedulable_wait_secs=45\n",
                None,
            ),
        ] {
            write(&format!(
                "role='worker'\nworker_server='http://coordinator:3000'\n{body}"
            ));
            let loaded = WorkerDaemonConfig::load(&path, None, None);
            match expected {
                Some(secs) => assert_eq!(
                    loaded
                        .expect("S5")
                        .runtime
                        .sandbox
                        .k8s_unschedulable_wait_secs,
                    secs,
                    "S5"
                ),
                None => {
                    let Err(error) = loaded else {
                        panic!("S5 must refuse {body}");
                    };
                    assert!(error.contains("k8s_unschedulable_wait_secs"), "S5 {body}");
                }
            }
        }
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_namespace='  '\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None, None).is_err(), "S2");
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_network_policy_enforcement='awaken-restricted-egress-v1'\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None, None).is_err(), "S3");
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='namespace'\nsandbox_trusted_host_enabled=true\n",
        );
        assert!(
            WorkerDaemonConfig::load(&path, None, None)
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
            let result = WorkerDaemonConfig::load(&path, None, None);
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
            assert!(
                WorkerDaemonConfig::load(&path, None, None).is_err(),
                "R2/R3"
            );
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
        let error = WorkerDaemonConfig::load(&path, None, None)
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
        let error = WorkerDaemonConfig::load(&path, None, None)
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
