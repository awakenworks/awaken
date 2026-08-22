//! Worker application service for trusted-host ACP capabilities.
//!
//! The service owns host discovery, pinned wrapper acquisition, secret-free
//! WorkerLocal registration, and the liveness resolver installed into a Worker.
//! Composition roots provide storage paths and repositories; Runtime Host and the
//! ACP executor only consume the resulting immutable profile and resolver.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
pub use awaken_acp_contract::AcpCapabilityNegotiator;
use awaken_acp_contract::{
    AcpCapabilityObservation, AcpCapabilityObservationSource, AcpCapabilityObservationState,
};
use awaken_credential_vault::repo::{CredentialRepo, ensure_worker_local};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, WorkerLocalBinding,
};
use awaken_run_executor_acp::{AcpCli, acp_cli};
use awaken_runtime_contract::resolved::Backend;
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialObservation, CredentialObservationSource,
    CredentialObservationState, CredentialRef, WorkerLocalReferenceRevalidator,
};

mod capability_probe;
mod host_discovery;
pub use capability_probe::{
    AcpCapabilityState, EffectiveAcpCapabilityProfile, HostAcpCapabilityNegotiator,
};
pub use host_discovery::{AcpDetectionState, AcpDiscovery, AcpHostDiscovery, AcpHostObservation};

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
        if executable.is_file() && installed_wrapper_matches(&prefix, package) {
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
        if !installed_wrapper_matches(&prefix, package) {
            return Err(format!(
                "installed ACP wrapper for {} does not match pinned package {package}",
                cli.id
            ));
        }
        canonical_wrapper_argv(&executable).map(Some)
    }
}

fn installed_wrapper_matches(prefix: &Path, package: &str) -> bool {
    let Some((name, version)) = package.rsplit_once('@') else {
        return false;
    };
    if name.is_empty() || version.is_empty() {
        return false;
    }
    let Ok(bytes) = std::fs::read(prefix.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(name))
        .and_then(serde_json::Value::as_str)
        == Some(version)
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
    pub probe_cwd: PathBuf,
    pub initial_workspace: Option<String>,
    pub wrapper_root: PathBuf,
    pub selected_cli_ids: Option<BTreeSet<String>>,
    pub credentials: Arc<dyn CredentialRepo>,
}

/// Secret-free output installed into deployment profile and Worker builder.
pub struct PreparedAcpCapabilities {
    pub observations: Vec<AcpHostObservation>,
    pub launch_argv: BTreeMap<String, Vec<String>>,
    pub effective_profiles: BTreeMap<String, EffectiveAcpCapabilityProfile>,
    pub resolver: Arc<AcpLocalCredentialResolver>,
    selected_cli_ids: BTreeSet<String>,
}

/// Production host preparation using the canonical discovery and acquisition adapters.
/// Port-driven application service used by alternative composition roots and tests.
pub async fn prepare_host_acp_with(
    input: LocalAcpPreparation,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
) -> Result<PreparedAcpCapabilities, String> {
    std::fs::create_dir_all(&input.probe_cwd).map_err(|error| {
        format!(
            "create ACP host-probe directory {}: {error}",
            input.probe_cwd.display()
        )
    })?;
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
    let mut effective_profiles = BTreeMap::new();
    for observation in observations.iter_mut().filter(|observation| {
        observation.detected()
            && input
                .selected_cli_ids
                .as_ref()
                .is_none_or(|selected| selected.contains(&observation.cli_id))
    }) {
        if observation.credential_state != Some(CredentialObservationState::Available) {
            observation.capability_state = Some(AcpCapabilityState::Unavailable);
            observation.capability_reason_code = Some("acp_login_not_available".to_string());
            continue;
        }
        let cli = acp_cli(&observation.cli_id).expect("discovery returns catalog ids");
        let argv = launch_argv
            .get(&observation.cli_id)
            .cloned()
            .unwrap_or_else(|| cli.acquisition.local_argv());
        match negotiator
            .negotiate(&argv, &input.probe_cwd, cli.auth_method_id)
            .await
        {
            Ok(negotiated) => {
                let profile = EffectiveAcpCapabilityProfile::verified(
                    &observation.cli_id,
                    observation.version.as_deref().unwrap_or("unknown"),
                    negotiated,
                );
                observation.capability_state = Some(AcpCapabilityState::Verified);
                observation.capability_fingerprint = Some(profile.fingerprint.clone());
                observation.capability_reason_code = None;
                effective_profiles.insert(observation.cli_id.clone(), profile);
            }
            Err(_) => {
                observation.capability_state = Some(AcpCapabilityState::ProbeFailed);
                observation.capability_fingerprint = None;
                observation.capability_reason_code =
                    Some("acp_capability_probe_failed".to_string());
            }
        }
    }
    let capability_routes = observations
        .iter()
        .filter(|observation| observation.detected())
        .map(|observation| {
            let cli = acp_cli(&observation.cli_id).expect("discovery returns catalog ids");
            let argv = launch_argv
                .get(&observation.cli_id)
                .cloned()
                .unwrap_or_else(|| cli.acquisition.local_argv());
            (observation.cli_id.clone(), argv)
        })
        .collect();
    let resolver = Arc::new(
        AcpLocalCredentialResolver::from_sources(discovery, [])?.with_capability_observations(
            negotiator,
            capability_routes,
            input.probe_cwd,
        ),
    );
    let selected_cli_ids = observations
        .iter()
        .filter(|observation| {
            observation.detected()
                && input
                    .selected_cli_ids
                    .as_ref()
                    .is_none_or(|selected| selected.contains(&observation.cli_id))
        })
        .map(|observation| observation.cli_id.clone())
        .collect();
    let prepared = PreparedAcpCapabilities {
        observations,
        launch_argv,
        effective_profiles,
        resolver,
        selected_cli_ids,
    };
    if let Some(workspace) = input.initial_workspace {
        prepared
            .bind_workspace(input.credentials.as_ref(), &workspace)
            .await?;
    }
    Ok(prepared)
}

impl PreparedAcpCapabilities {
    /// ACP CLI routes that are safe for this Worker to publish.
    ///
    /// A detected CLI without a current login remains routable so a later login
    /// can become live without restarting the Worker. A CLI that reported an
    /// available login is published only after its protocol capabilities were
    /// verified; this prevents a misleading ready route when negotiation failed.
    #[must_use]
    pub fn routable_cli_ids(&self) -> Vec<String> {
        routable_cli_ids(&self.observations, &self.selected_cli_ids)
    }

    /// Startup acquisition evidence restricted to the exact admitted Worker
    /// routes. Diagnostic-only or failed-capability rows must never leak an argv
    /// entry into a deployment profile that does not advertise their CLI id.
    #[must_use]
    pub fn routable_launch_argv(&self) -> BTreeMap<String, Vec<String>> {
        let routable: BTreeSet<_> = self.routable_cli_ids().into_iter().collect();
        self.launch_argv
            .iter()
            .filter(|(cli_id, _)| routable.contains(*cli_id))
            .map(|(cli_id, argv)| (cli_id.clone(), argv.clone()))
            .collect()
    }

    /// Idempotently expose the already-discovered host ACP identities in one
    /// execution Workspace and add their exact revisions to the shared resolver.
    pub async fn bind_workspace(
        &self,
        credentials: &dyn CredentialRepo,
        workspace: &str,
    ) -> Result<(), String> {
        ensure_workspace_bindings(
            credentials,
            workspace,
            self.selected_cli_ids.iter().map(String::as_str),
        )
        .await?;
        let sources = credentials
            .list(workspace)
            .await
            .map_err(|error| format!("list local ACP bindings: {error}"))?;
        self.resolver.add_sources(sources)
    }
}

/// Idempotently create the non-secret WorkerLocal identities requested by one
/// execution Workspace. Discovery is deliberately not required here: the
/// control plane records intent, while Worker observations independently prove
/// whether a matching host identity is currently usable.
pub async fn ensure_workspace_bindings<'a>(
    credentials: &dyn CredentialRepo,
    workspace: &str,
    cli_ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    for cli_id in cli_ids {
        let cli = acp_cli(cli_id)
            .ok_or_else(|| format!("local ACP binding names unknown CLI `{cli_id}`"))?;
        ensure_worker_local(
            credentials,
            workspace,
            WorkerLocalBinding::new(format!("acp:{}", cli.id), "default"),
            None,
        )
        .await
        .map_err(|error| format!("register local ACP binding for {}: {error}", cli.id))?;
    }
    Ok(())
}

fn routable_cli_ids(
    observations: &[AcpHostObservation],
    selected_cli_ids: &BTreeSet<String>,
) -> Vec<String> {
    observations
        .iter()
        .filter(|observation| {
            selected_cli_ids.contains(&observation.cli_id)
                && observation.detected()
                && (observation.credential_state != Some(CredentialObservationState::Available)
                    || observation.capability_state == Some(AcpCapabilityState::Verified))
        })
        .map(|observation| observation.cli_id.clone())
        .collect()
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
    bindings: Mutex<BTreeMap<String, LocalBinding>>,
    last_host_observations: Mutex<BTreeMap<String, AcpHostObservation>>,
    capability: Option<CapabilityObservationConfig>,
}

struct CapabilityObservationConfig {
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
    launch_argv: BTreeMap<String, Vec<String>>,
    cwd: PathBuf,
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
            let Backend::Acp(backend) = Backend::from_ref(&binding.driver_id) else {
                continue;
            };
            let cli = acp_cli(backend.cli())
                .ok_or_else(|| format!("worker-local source names unknown ACP CLI `{backend}`"))?;
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
            bindings: Mutex::new(bindings),
            last_host_observations: Mutex::new(BTreeMap::new()),
            capability: None,
        })
    }

    pub fn add_sources(
        &self,
        sources: impl IntoIterator<Item = CredentialSource>,
    ) -> Result<(), String> {
        let additions = Self::from_sources(self.discovery.clone(), sources)?;
        let mut bindings = self.bindings.lock().expect("ACP bindings poisoned");
        for (id, binding) in additions
            .bindings
            .into_inner()
            .expect("new ACP bindings are not poisoned")
        {
            if let Some(existing) = bindings.get(&id)
                && existing.credential != binding.credential
            {
                return Err(format!("conflicting WorkerLocal credential id `{id}`"));
            }
            bindings.insert(id, binding);
        }
        Ok(())
    }

    #[must_use]
    pub fn with_capability_observations(
        mut self,
        negotiator: Arc<dyn AcpCapabilityNegotiator>,
        launch_argv: BTreeMap<String, Vec<String>>,
        cwd: PathBuf,
    ) -> Self {
        self.capability = Some(CapabilityObservationConfig {
            negotiator,
            launch_argv,
            cwd,
        });
        self
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
        self.last_host_observations
            .lock()
            .expect("ACP host-observation cache poisoned")
            .insert(binding.cli.id.to_string(), observation.clone());
        let (state, reason_code) = classify_host_observation(&observation);
        CredentialObservation {
            credential: binding.credential.clone(),
            state,
            observed_at_ms,
            reason_code,
        }
    }
}

#[async_trait]
impl AcpCapabilityObservationSource for AcpLocalCredentialResolver {
    async fn capability_observations(&self) -> Result<Vec<AcpCapabilityObservation>, String> {
        let Some(capability) = &self.capability else {
            return Ok(Vec::new());
        };
        let mut observations = Vec::new();
        let bindings = self
            .bindings
            .lock()
            .expect("ACP bindings poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for binding in &bindings {
            let observed_at_ms = wall_clock_ms();
            if binding.status != CredentialStatus::Active {
                observations.push(AcpCapabilityObservation {
                    backend_ref: format!("acp:{}", binding.cli.id),
                    adapter_version: "unknown".into(),
                    state: AcpCapabilityObservationState::Unavailable,
                    observed_at_ms,
                    fingerprint: None,
                    negotiated: None,
                    reason_code: Some("worker_local_source_disabled".into()),
                });
                continue;
            }
            let cached = self
                .last_host_observations
                .lock()
                .expect("ACP host-observation cache poisoned")
                .get(binding.cli.id)
                .cloned();
            let host = match cached {
                Some(observation) => observation,
                None => self.discovery.discover(binding.cli).await,
            };
            if host.credential_state != Some(CredentialObservationState::Available) {
                observations.push(AcpCapabilityObservation {
                    backend_ref: format!("acp:{}", binding.cli.id),
                    adapter_version: host.version.unwrap_or_else(|| "unknown".into()),
                    state: AcpCapabilityObservationState::Unavailable,
                    observed_at_ms,
                    fingerprint: None,
                    negotiated: None,
                    reason_code: Some("acp_login_not_available".into()),
                });
                continue;
            }
            let Some(argv) = capability.launch_argv.get(binding.cli.id) else {
                observations.push(AcpCapabilityObservation {
                    backend_ref: format!("acp:{}", binding.cli.id),
                    adapter_version: host.version.unwrap_or_else(|| "unknown".into()),
                    state: AcpCapabilityObservationState::ProbeFailed,
                    observed_at_ms,
                    fingerprint: None,
                    negotiated: None,
                    reason_code: Some("acp_launch_route_missing".into()),
                });
                continue;
            };
            match capability
                .negotiator
                .negotiate(argv, &capability.cwd, binding.cli.auth_method_id)
                .await
            {
                Ok(negotiated) => {
                    let profile = EffectiveAcpCapabilityProfile::verified(
                        binding.cli.id,
                        host.version.as_deref().unwrap_or("unknown"),
                        negotiated,
                    );
                    observations.push(AcpCapabilityObservation {
                        backend_ref: format!("acp:{}", binding.cli.id),
                        adapter_version: profile.cli_version,
                        state: AcpCapabilityObservationState::Verified,
                        observed_at_ms,
                        fingerprint: Some(profile.fingerprint),
                        negotiated: Some(profile.negotiated),
                        reason_code: None,
                    });
                }
                Err(_) => observations.push(AcpCapabilityObservation {
                    backend_ref: format!("acp:{}", binding.cli.id),
                    adapter_version: host.version.unwrap_or_else(|| "unknown".into()),
                    state: AcpCapabilityObservationState::ProbeFailed,
                    observed_at_ms,
                    fingerprint: None,
                    negotiated: None,
                    reason_code: Some("acp_capability_probe_failed".into()),
                }),
            }
        }
        observations.sort_by(|left, right| left.backend_ref.cmp(&right.backend_ref));
        Ok(observations)
    }
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
impl CredentialObservationSource for AcpLocalCredentialResolver {
    async fn credential_observations(
        &self,
    ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
        let mut observations = BTreeSet::new();
        let bindings = self
            .bindings
            .lock()
            .expect("ACP bindings poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for binding in &bindings {
            observations.insert(self.observe(binding).await);
        }
        Ok(observations)
    }
}

#[async_trait]
impl WorkerLocalReferenceRevalidator for AcpLocalCredentialResolver {
    async fn revalidate_worker_reference(
        &self,
        credential: &CredentialRef,
    ) -> Result<CredentialObservation, CredentialMaterialError> {
        let binding = self
            .bindings
            .lock()
            .expect("ACP bindings poisoned")
            .get(&credential.id)
            .filter(|binding| binding.credential.revision == credential.revision)
            .cloned()
            .ok_or(CredentialMaterialError::Unavailable)?;
        require_available(self.observe(&binding).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_credential_vault::repo::InMemoryCredentialRepo;

    #[test]
    fn wrapper_cache_requires_the_exact_pinned_package_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("package.json");
        std::fs::write(
            &manifest,
            r#"{"dependencies":{"@agentclientprotocol/claude-agent-acp":"0.69.0"}}"#,
        )
        .unwrap();
        assert!(installed_wrapper_matches(
            directory.path(),
            "@agentclientprotocol/claude-agent-acp@0.69.0"
        ));
        assert!(
            !installed_wrapper_matches(
                directory.path(),
                "@agentclientprotocol/claude-agent-acp@0.64.2"
            ),
            "a catalog downgrade or upgrade cannot reuse different bytes"
        );

        std::fs::write(&manifest, b"not-json").unwrap();
        assert!(
            !installed_wrapper_matches(
                directory.path(),
                "@agentclientprotocol/claude-agent-acp@0.69.0"
            ),
            "a corrupt manifest fails closed"
        );
        std::fs::remove_file(&manifest).unwrap();
        assert!(
            !installed_wrapper_matches(
                directory.path(),
                "@agentclientprotocol/claude-agent-acp@0.69.0"
            ),
            "a legacy unversioned cache must be reacquired once"
        );
        assert!(!installed_wrapper_matches(directory.path(), "unversioned"));
    }

    struct AvailableDiscovery;

    fn observation(
        id: &str,
        detection: AcpDetectionState,
        credential: Option<CredentialObservationState>,
        capability: Option<AcpCapabilityState>,
    ) -> AcpHostObservation {
        AcpHostObservation {
            cli_id: id.into(),
            display_name: id.into(),
            detection,
            version: None,
            credential_state: credential,
            reason_code: None,
            capability_state: capability,
            capability_fingerprint: None,
            capability_reason_code: None,
        }
    }

    #[test]
    fn worker_route_admission_follows_the_capability_decision_table() {
        // Causes: catalog selection, executable detection, current login, and
        // successful protocol negotiation. Acquisition argv is diagnostic
        // evidence until the same route decision admits its CLI. Effects:
        // R1 unselected -> no route; R2 missing -> no route;
        // R3 detected + not logged in -> route for live revalidation;
        // R4 available + negotiation failed -> no route and no launch argv;
        // R5 available + verified -> route and its launch argv.
        let selected = ["codex".to_string()].into_iter().collect();
        assert!(
            routable_cli_ids(
                &[observation(
                    "claude",
                    AcpDetectionState::Detected,
                    None,
                    None
                )],
                &selected
            )
            .is_empty(),
            "R1"
        );
        assert!(
            routable_cli_ids(
                &[observation("codex", AcpDetectionState::Missing, None, None)],
                &selected
            )
            .is_empty(),
            "R2"
        );
        assert_eq!(
            routable_cli_ids(
                &[observation(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::LoginRequired),
                    Some(AcpCapabilityState::Unavailable),
                )],
                &selected,
            ),
            ["codex"],
            "R3"
        );
        assert!(
            routable_cli_ids(
                &[observation(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::Available),
                    Some(AcpCapabilityState::ProbeFailed),
                )],
                &selected,
            )
            .is_empty(),
            "R4"
        );
        assert_eq!(
            routable_cli_ids(
                &[observation(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::Available),
                    Some(AcpCapabilityState::Verified),
                )],
                &selected,
            ),
            ["codex"],
            "R5"
        );

        let discovery: Arc<dyn AcpDiscovery> = Arc::new(AvailableDiscovery);
        let resolver = Arc::new(
            AcpLocalCredentialResolver::from_sources(discovery, []).expect("empty resolver"),
        );
        for (rule, capability, expected) in [
            ("R4", AcpCapabilityState::ProbeFailed, BTreeMap::new()),
            (
                "R5",
                AcpCapabilityState::Verified,
                BTreeMap::from([("codex".to_string(), vec!["/wrapper/codex".to_string()])]),
            ),
        ] {
            let prepared = PreparedAcpCapabilities {
                observations: vec![observation(
                    "codex",
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::Available),
                    Some(capability),
                )],
                launch_argv: BTreeMap::from([(
                    "codex".to_string(),
                    vec!["/wrapper/codex".to_string()],
                )]),
                effective_profiles: BTreeMap::new(),
                resolver: resolver.clone(),
                selected_cli_ids: ["codex".to_string()].into_iter().collect(),
            };
            assert_eq!(prepared.routable_launch_argv(), expected, "{rule}");
        }
    }

    #[async_trait]
    impl AcpDiscovery for AvailableDiscovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            AcpHostObservation {
                cli_id: cli.id.into(),
                display_name: cli.display_name.into(),
                detection: AcpDetectionState::Detected,
                version: Some("test".into()),
                credential_state: Some(CredentialObservationState::Available),
                reason_code: None,
                capability_state: None,
                capability_fingerprint: None,
                capability_reason_code: None,
            }
        }
    }

    #[tokio::test]
    async fn workspace_binding_extends_one_shared_liveness_resolver() {
        // Cause/effect decision table:
        // B1 first Workspace -> one idempotent WorkerLocal revision observed;
        // B2 second Workspace -> both revisions observed by the same resolver;
        // B3 repeat Workspace -> no duplicate source or competing resolver.
        let discovery: Arc<dyn AcpDiscovery> = Arc::new(AvailableDiscovery);
        let resolver = Arc::new(
            AcpLocalCredentialResolver::from_sources(discovery, []).expect("empty resolver"),
        );
        let prepared = PreparedAcpCapabilities {
            observations: Vec::new(),
            launch_argv: BTreeMap::new(),
            effective_profiles: BTreeMap::new(),
            resolver: resolver.clone(),
            selected_cli_ids: ["codex".to_string()].into_iter().collect(),
        };
        let credentials = InMemoryCredentialRepo::new();
        prepared
            .bind_workspace(&credentials, "workspace-a")
            .await
            .expect("B1");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            1,
            "B1"
        );
        prepared
            .bind_workspace(&credentials, "workspace-b")
            .await
            .expect("B2");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            2,
            "B2"
        );
        prepared
            .bind_workspace(&credentials, "workspace-a")
            .await
            .expect("B3");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            2,
            "B3"
        );
    }
}
