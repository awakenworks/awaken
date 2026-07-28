//! Worker application service for trusted-host ACP capabilities.
//!
//! The service owns host discovery, pinned wrapper acquisition, secret-free
//! WorkerLocal registration, and the liveness resolver installed into a Worker.
//! Composition roots provide storage paths and repositories; Runtime Host and the
//! ACP executor only consume the resulting immutable profile and resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_credential_vault::repo::{CredentialRepo, ensure_worker_local};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, WorkerLocalBinding,
};
use awaken_run_executor_acp::{
    AcpCli, AcpDetectionState, AcpDiscovery, AcpHostDiscovery, AcpHostObservation, acp_cli,
};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
    CredentialObservation, CredentialObservationState, CredentialRef, ResolvedCredentialMaterial,
};

/// Acquisition port for an ACP protocol wrapper declared by the canonical catalog.
#[async_trait]
pub trait AcpWrapperInstaller: Send + Sync {
    async fn resolved_argv(&self, cli: &AcpCli, root: &Path)
    -> Result<Option<Vec<String>>, String>;
}

/// Production acquisition adapter for pinned npm wrappers.
pub struct NpmWrapperInstaller;

#[async_trait]
impl AcpWrapperInstaller for NpmWrapperInstaller {
    async fn resolved_argv(
        &self,
        cli: &AcpCli,
        root: &Path,
    ) -> Result<Option<Vec<String>>, String> {
        let awaken_run_executor_acp::AcpAcquisition::PinnedNpmWrapper {
            installer,
            package,
            bin,
        } = cli.acquisition
        else {
            return Ok(None);
        };
        let prefix = root.join(cli.id);
        let executable = prefix.join("node_modules").join(".bin").join(bin);
        if executable.is_file() {
            return canonical_wrapper_argv(&executable).map(Some);
        }
        std::fs::create_dir_all(&prefix).map_err(|error| {
            format!("create ACP wrapper directory {}: {error}", prefix.display())
        })?;
        let mut command = tokio::process::Command::new(installer);
        command
            .args([
                "install",
                "--no-audit",
                "--no-fund",
                "--save-exact",
                "--prefix",
            ])
            .arg(&prefix)
            .arg(package)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(120), command.output())
            .await
            .map_err(|_| format!("install pinned ACP wrapper for {} timed out", cli.id))?
            .map_err(|error| format!("start {installer} for {}: {error}", cli.id))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "install pinned ACP wrapper for {} failed: {}",
                cli.id,
                stderr.lines().next().unwrap_or("npm exited unsuccessfully")
            ));
        }
        canonical_wrapper_argv(&executable).map(Some)
    }
}

fn canonical_wrapper_argv(executable: &Path) -> Result<Vec<String>, String> {
    let executable = executable.canonicalize().map_err(|error| {
        format!(
            "installed ACP wrapper {} is unavailable: {error}",
            executable.display()
        )
    })?;
    Ok(vec![executable.to_string_lossy().into_owned()])
}

/// Inputs owned by a composition root, not by ACP discovery.
pub struct LocalAcpPreparation {
    pub workspace: String,
    pub wrapper_root: PathBuf,
    pub selected_cli_ids: Option<BTreeSet<String>>,
    pub credentials: Arc<dyn CredentialRepo>,
}

/// Secret-free output installed into deployment profile and Worker builder.
pub struct PreparedAcpCapabilities {
    pub observations: Vec<AcpHostObservation>,
    pub launch_argv: BTreeMap<String, Vec<String>>,
    pub resolver: Arc<AcpLocalCredentialResolver>,
}

/// Production host preparation using the canonical discovery and acquisition adapters.
pub async fn prepare_host_acp(
    input: LocalAcpPreparation,
) -> Result<PreparedAcpCapabilities, String> {
    prepare_host_acp_with(
        input,
        Arc::new(AcpHostDiscovery::local(std::time::Duration::from_secs(3))),
        Arc::new(NpmWrapperInstaller),
    )
    .await
}

/// Port-driven application service used by alternative composition roots and tests.
pub async fn prepare_host_acp_with(
    input: LocalAcpPreparation,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
) -> Result<PreparedAcpCapabilities, String> {
    let mut observations = discovery.discover_all().await;
    let mut launch_argv = BTreeMap::new();
    for observation in observations.iter_mut().filter(|observation| {
        observation.detected()
            && input
                .selected_cli_ids
                .as_ref()
                .is_none_or(|selected| selected.contains(&observation.cli_id))
    }) {
        let cli = acp_cli(&observation.cli_id).expect("discovery returns catalog ids");
        match installer.resolved_argv(cli, &input.wrapper_root).await {
            Ok(Some(argv)) => {
                launch_argv.insert(observation.cli_id.clone(), argv);
            }
            Ok(None) => {}
            Err(_) => {
                observation.detection = AcpDetectionState::ProbeFailed;
                observation.credential_state = Some(CredentialObservationState::ProbeFailed);
                observation.reason_code = Some("acp_wrapper_install_failed".to_string());
            }
        }
    }
    for observation in observations.iter().filter(|observation| {
        observation.detected()
            && input
                .selected_cli_ids
                .as_ref()
                .is_none_or(|selected| selected.contains(&observation.cli_id))
    }) {
        ensure_worker_local(
            input.credentials.as_ref(),
            &input.workspace,
            WorkerLocalBinding::new(format!("acp:{}", observation.cli_id), "default"),
            None,
        )
        .await
        .map_err(|error| {
            format!(
                "register local ACP binding for {}: {error}",
                observation.cli_id
            )
        })?;
    }
    let sources = input
        .credentials
        .list(&input.workspace)
        .await
        .map_err(|error| format!("list local ACP bindings: {error}"))?;
    let resolver = Arc::new(AcpLocalCredentialResolver::from_sources(
        discovery, sources,
    )?);
    Ok(PreparedAcpCapabilities {
        observations,
        launch_argv,
        resolver,
    })
}

#[derive(Clone)]
struct LocalBinding {
    credential: CredentialRef,
    cli: &'static AcpCli,
    status: CredentialStatus,
}

/// Liveness-only resolver for every `acp:*` WorkerLocal source on one Worker.
pub struct AcpLocalCredentialResolver {
    discovery: Arc<dyn AcpDiscovery>,
    bindings: BTreeMap<String, LocalBinding>,
}

impl AcpLocalCredentialResolver {
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
        let observed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        if binding.status != CredentialStatus::Active {
            return CredentialObservation {
                credential: binding.credential.clone(),
                state: CredentialObservationState::Disabled,
                observed_at_ms,
                reason_code: Some("worker_local_source_disabled".to_string()),
            };
        }
        let observation = self.discovery.discover(binding.cli).await;
        let (state, reason_code) = classify_host_observation(&observation);
        CredentialObservation {
            credential: binding.credential.clone(),
            state,
            observed_at_ms,
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
