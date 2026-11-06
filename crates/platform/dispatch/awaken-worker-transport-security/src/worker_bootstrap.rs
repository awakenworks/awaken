// @formal entrypoint worker_credential.publication_budget_covers_probe_and_delivery
//! The Worker bootstrap: the identity, upstream credentials and node settings
//! one Worker process starts with.
//!
//! It is resolved where the Worker's request authentication is owned, because
//! its identity is either authored or read from a Kubernetes Pod projection
//! (this crate's `kubernetes` feature), and because every process that reads
//! the configuration file resolves it: the standalone Worker, and the
//! AllInOne, Control and Coordinator startups. The Worker builds its upstream
//! from the resolved value (`awaken_worker::worker_upstream`).

use std::path::{Path, PathBuf};

/// Worker-owned bootstrap settings, resolved by the standalone Worker and by
/// every startup that reads the same configuration file (AllInOne, Control,
/// Coordinator) without linking the Worker.
#[derive(Debug, Clone)]
pub struct WorkerBootstrap {
    pub worker_id: String,
    pub request_credential_file: Option<PathBuf>,
    pub kubernetes_identity_directory: Option<PathBuf>,
    pub credential_material_root: PathBuf,
    pub credential_envelope_keys_file: Option<PathBuf>,
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
    pub kubernetes_identity_directory: Option<PathBuf>,
    pub credential_material_root: Option<PathBuf>,
    pub credential_envelope_keys_file: Option<PathBuf>,
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
        let worker_id = if let Some(directory) = &input.kubernetes_identity_directory {
            if input.request_credential_file.is_some() {
                return Err("Worker cannot mix Pod-bound tokens and signing credentials".into());
            }
            #[cfg(feature = "kubernetes")]
            {
                let pod =
                    crate::kubernetes::KubernetesWorkerRequestAuthorizer::from_projected_directory(
                        directory,
                    )?;
                let exact = pod.pod().worker_id();
                if input
                    .worker_id
                    .as_ref()
                    .is_some_and(|configured| configured != &exact)
                {
                    return Err("worker_id must exactly match its Kubernetes Pod projection".into());
                }
                Some(exact)
            }
            #[cfg(not(feature = "kubernetes"))]
            {
                let _ = directory;
                return Err(
                    "Pod-bound Worker transport requires the container-k8s build feature".into(),
                );
            }
        } else {
            input.worker_id
        };
        if input.credential_envelope_keys_file.is_some()
            && (input.credential_material_root.is_some() || input.credential_trust_domain.is_some())
        {
            return Err("credential envelope keys and file projection settings select competing material resolvers".into());
        }
        if input
            .credential_envelope_keys_file
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err("credential_envelope_keys_file must not be empty".into());
        }
        let worker = Self {
            worker_id: worker_id.unwrap_or_else(|| "awaken-worker".into()),
            request_credential_file: input.request_credential_file,
            kubernetes_identity_directory: input.kubernetes_identity_directory,
            credential_envelope_keys_file: input.credential_envelope_keys_file,
            credential_material_root: input
                .credential_material_root
                .unwrap_or_else(|| data_dir.join("worker-credentials")),
            credential_trust_domain: input.credential_trust_domain.unwrap_or_else(|| {
                awaken_agent_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN.into()
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
        awaken_worker_contract::validate_observation_timing(
            std::time::Duration::from_secs(worker.credential_probe_interval_secs),
            std::time::Duration::from_secs(worker.credential_observation_ttl_secs),
            None,
        )?;
        Ok(worker)
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkerBootstrap, WorkerBootstrapInput};
    use std::path::{Path, PathBuf};

    /// Decision table: C1 envelope path, C2 explicit projected root, C3 explicit
    /// projected holder. !C1 keeps the original file resolver; C1+!C2+!C3 selects
    /// the envelope resolver; C1+(C2|C3) rejects competing ownership before IO.
    /// Empty envelope paths reject even when no other source is authored.
    #[test]
    fn material_resolver_configuration_has_one_owner() {
        for envelope in [false, true] {
            for root in [false, true] {
                for holder in [false, true] {
                    let result = WorkerBootstrap::resolve(
                        WorkerBootstrapInput {
                            credential_envelope_keys_file: envelope
                                .then(|| PathBuf::from("keys.json")),
                            credential_material_root: root.then(|| PathBuf::from("projection")),
                            credential_trust_domain: holder.then(|| "worker-a".into()),
                            ..Default::default()
                        },
                        Path::new("worker"),
                    );
                    assert_eq!(result.is_ok(), !envelope || !(root || holder));
                }
            }
        }
        assert!(
            WorkerBootstrap::resolve(
                WorkerBootstrapInput {
                    credential_envelope_keys_file: Some(PathBuf::new()),
                    ..Default::default()
                },
                Path::new("worker")
            )
            .is_err()
        );
    }
}
