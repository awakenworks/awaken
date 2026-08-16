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
        if worker.credential_probe_interval_secs == 0
            || worker.credential_observation_ttl_secs <= worker.credential_probe_interval_secs
        {
            return Err(
                "Worker credential observation TTL must exceed the non-zero probe interval".into(),
            );
        }
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
    worker_id: Option<String>,
    worker_request_credential_file: Option<PathBuf>,
    worker_credential_material_root: Option<PathBuf>,
    worker_credential_trust_domain: Option<String>,
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
    sandbox_allow_local_fallback: Option<bool>,
    k8s_namespace: Option<String>,
    acp_clis: Option<Vec<String>>,
    acp_default_cli: Option<String>,
    container_image: Option<String>,
}

/// Fully validated inputs for the standalone Worker process.
#[derive(Debug, Clone)]
pub struct WorkerDaemonConfig {
    server: String,
    pub worker: WorkerBootstrap,
    runtime: awaken_runtime_host::DeploymentConfig,
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
        let mut runtime = awaken_runtime_host::DeploymentConfig::ephemeral();
        runtime.upstream = Some(server.clone());
        runtime.sandbox_tier = file
            .sandbox_tier
            .as_deref()
            .unwrap_or("namespace")
            .parse()?;
        runtime.sandbox_dir = file.sandbox_dir;
        runtime.sandbox.allow_local_fallback = file.sandbox_allow_local_fallback.unwrap_or(false);
        if let Some(namespace) = file.k8s_namespace {
            if namespace.trim().is_empty() {
                return Err("k8s_namespace must not be empty".into());
            }
            runtime.sandbox.k8s_namespace = namespace;
        }
        let acp_clis = file.acp_clis.unwrap_or_default();
        runtime.acp = (!acp_clis.is_empty())
            .then(|| awaken_runtime_host::AcpWorkerProfile::new(acp_clis, file.acp_default_cli))
            .transpose()?;
        runtime.container_image = file.container_image;
        Ok(Self {
            server,
            worker,
            runtime,
        })
    }

    pub async fn build(&self) -> Result<crate::WorkerNode, String> {
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
        let upstream = awaken_worker_transport_security::WorkerUpstream::remote(
            &self.server,
            &self.worker.worker_id,
            transport_credentials,
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
        let mut builder = crate::WorkerNodeBuilder::new(upstream)
            .with_deployment_config(self.runtime.clone())
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
    /// the sandbox tier belongs to the canonical runtime vocabulary.
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
            "role='worker'\nmode='server'\nworker_server='https://coordinator:3000'\nworker_id='worker-a'\nworker_request_credential_file='{}'\n",
            credential.display()
        ));
        let resolved = WorkerDaemonConfig::load(&path, None).expect("R1");
        assert!(resolved.runtime.database_url.is_none(), "R1");
        assert!(resolved.runtime.storage_dir.is_none(), "R1");
        let worker = resolved
            .build()
            .await
            .expect("R1 canonical Worker assembly");
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
        // rejects; S3 the removed operator-evidence key rejects as unknown.
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

        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_namespace='  '\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None).is_err(), "S2");
        write(
            "role='worker'\nworker_server='http://coordinator:3000'\nsandbox_tier='k8s'\nk8s_network_policy_enforcement='awaken-restricted-egress-v1'\n",
        );
        assert!(WorkerDaemonConfig::load(&path, None).is_err(), "S3");
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
