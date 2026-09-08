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
use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationSource};
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
        // Protocol capability is independent of backend-owned login liveness.
        // Provider-routed execution supplies its own admitted credential later;
        // this prompt-free probe neither grants nor materializes that authority.
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
    let selected_cli_ids: BTreeSet<_> = observations
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
    let capability_routes = selected_cli_ids
        .iter()
        .map(|cli_id| {
            let cli = acp_cli(cli_id).expect("discovery returns catalog ids");
            let argv = launch_argv
                .get(cli_id)
                .cloned()
                .unwrap_or_else(|| cli.acquisition.local_argv());
            (cli_id.clone(), argv)
        })
        .collect();
    let resolver = Arc::new(
        AcpLocalCredentialResolver::from_sources(discovery, [])?.with_capability_observations(
            negotiator,
            capability_routes,
            input.probe_cwd,
        ),
    );
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
        let additions = additions
            .bindings
            .into_inner()
            .expect("new ACP bindings are not poisoned");
        let mut bindings = self.bindings.lock().expect("ACP bindings poisoned");
        // Validate the entire batch under the same lock as publication: an
        // error must not leak its earlier inserts or status-only updates, and
        // another Workspace must not invalidate the check before commit.
        for (id, binding) in &additions {
            if let Some(existing) = bindings.get(id)
                && existing.credential != binding.credential
            {
                return Err(format!("conflicting WorkerLocal credential id `{id}`"));
            }
        }
        bindings.extend(additions);
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
        // Protocol capabilities belong to a configured host backend, not to
        // any Workspace credential. One backend gets one observation even if
        // many Workspaces use it, or no host-login binding is active at all.
        for (cli_id, argv) in &capability.launch_argv {
            let cli = acp_cli(cli_id)
                .ok_or_else(|| format!("ACP capability route names unknown CLI `{cli_id}`"))?;
            let observed_at_ms = wall_clock_ms();
            let cached = self
                .last_host_observations
                .lock()
                .expect("ACP host-observation cache poisoned")
                .get(cli.id)
                .cloned();
            let host = match cached {
                Some(observation) => observation,
                None => self.discovery.discover(cli).await,
            };
            if !host.detected() {
                observations.push(
                    AcpCapabilityObservation::unavailable(
                        format!("acp:{}", cli.id),
                        host.version.unwrap_or_else(|| "unknown".into()),
                        observed_at_ms,
                        "acp_cli_not_detected",
                    )
                    .map_err(|error| error.to_string())?,
                );
                continue;
            }
            match capability
                .negotiator
                .negotiate(argv, &capability.cwd, cli.auth_method_id)
                .await
            {
                Ok(negotiated) => {
                    let profile = EffectiveAcpCapabilityProfile::verified(
                        cli.id,
                        host.version.as_deref().unwrap_or("unknown"),
                        negotiated,
                    );
                    observations.push(
                        AcpCapabilityObservation::verified(
                            format!("acp:{}", cli.id),
                            profile.cli_version,
                            observed_at_ms,
                            profile.fingerprint,
                            profile.negotiated,
                        )
                        .map_err(|error| error.to_string())?,
                    );
                }
                Err(_) => observations.push(
                    AcpCapabilityObservation::probe_failed(
                        format!("acp:{}", cli.id),
                        host.version.unwrap_or_else(|| "unknown".into()),
                        observed_at_ms,
                        "acp_capability_probe_failed",
                    )
                    .map_err(|error| error.to_string())?,
                ),
            }
        }
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

#[async_trait]
impl CredentialObservationSource for AcpLocalCredentialResolver {
    async fn credential_observations(
        &self,
    ) -> Result<BTreeSet<CredentialObservation>, CredentialMaterialError> {
        let mut observations = BTreeSet::new();
        // The following capability phase may reuse this batch's host probes,
        // but must not borrow a probe from a previous refresh after every local
        // binding has been disabled. Credential identities remain independent.
        self.last_host_observations
            .lock()
            .expect("ACP host-observation cache poisoned")
            .clear();
        let mut host_observations: BTreeMap<&str, CredentialObservation> = BTreeMap::new();
        let bindings = self
            .bindings
            .lock()
            .expect("ACP bindings poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for binding in &bindings {
            let observation = if binding.status == CredentialStatus::Active {
                match host_observations.get(binding.cli.id) {
                    Some(observation) => CredentialObservation {
                        credential: binding.credential.clone(),
                        ..observation.clone()
                    },
                    None => {
                        let observation = self.observe(binding).await;
                        host_observations.insert(binding.cli.id, observation.clone());
                        observation
                    }
                }
            } else {
                self.observe(binding).await
            };
            observations.insert(observation);
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
        awaken_runtime_contract::require_exact_available_worker_reference(
            credential,
            self.observe(&binding).await,
        )
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

    struct CapabilityDiscovery {
        state: CredentialObservationState,
        detected: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl AcpDiscovery for CapabilityDiscovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            observation(
                cli.id,
                if cli.id == "claude" && self.detected.load(std::sync::atomic::Ordering::SeqCst) {
                    AcpDetectionState::Detected
                } else {
                    AcpDetectionState::Missing
                },
                Some(self.state),
                None,
            )
        }
    }

    struct CapabilityFixture {
        success: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AcpWrapperInstaller for CapabilityFixture {
        async fn resolved_argv(&self, _: &AcpCli, _: &Path) -> Result<Option<Vec<String>>, String> {
            Ok(Some(vec!["fixture-acp".into()]))
        }
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for CapabilityFixture {
        async fn negotiate(
            &self,
            _: &[String],
            _: &Path,
            _: Option<&str>,
        ) -> Result<awaken_acp_contract::NegotiatedAcpCapabilities, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !self.success {
                return Err("fixture negotiation failed".into());
            }
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

    #[tokio::test]
    async fn an_unknown_capability_route_fails_before_negotiation() {
        // R1 a custom composition supplies a route outside the canonical CLI
        // catalog => typed source failure, no protocol call and no advertisement.
        // Normal profiles are validated upstream, but this public port must not
        // panic or silently accept an unvalidated adapter identity.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = Arc::new(CapabilityFixture {
            success: true,
            calls: AtomicUsize::new(0),
        });
        let resolver = AcpLocalCredentialResolver::from_sources(Arc::new(AvailableDiscovery), [])
            .unwrap()
            .with_capability_observations(
                fixture.clone(),
                [("unknown".into(), vec!["must-not-launch".into()])]
                    .into_iter()
                    .collect(),
                PathBuf::from("."),
            );
        assert!(
            resolver
                .capability_observations()
                .await
                .unwrap_err()
                .contains("unknown CLI")
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disabling_the_last_binding_cannot_reuse_stale_installation_evidence() {
        // R1 active + installed -> Available credential and Verified protocol;
        // R2 disable the last binding AND remove the CLI before refresh ->
        // Disabled credential + Unavailable protocol, with no new negotiation;
        // R3 reinstall without re-enabling -> still Disabled but Verified protocol.
        // This is the production credential-then-capability sequence, not manual
        // cache clearing. It detects stale probes surviving a status transition.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let repo = InMemoryCredentialRepo::new();
        let mut source = ensure_worker_local(
            &repo,
            "workspace",
            WorkerLocalBinding::new("acp:claude", "default"),
            None,
        )
        .await
        .unwrap();
        let discovery = Arc::new(CapabilityDiscovery {
            state: CredentialObservationState::Available,
            detected: AtomicBool::new(true),
        });
        let fixture = Arc::new(CapabilityFixture {
            success: true,
            calls: AtomicUsize::new(0),
        });
        let resolver =
            AcpLocalCredentialResolver::from_sources(discovery.clone(), [source.clone()])
                .unwrap()
                .with_capability_observations(
                    fixture.clone(),
                    [("claude".into(), vec!["claude-acp".into()])]
                        .into_iter()
                        .collect(),
                    PathBuf::from("."),
                );
        assert_eq!(
            observed_bindings(&resolver).await.values().next(),
            Some(&CredentialObservationState::Available)
        );
        assert!(
            resolver.capability_observations().await.unwrap()[0]
                .negotiated()
                .is_some()
        );
        source.status = CredentialStatus::Disabled;
        resolver.add_sources([source]).unwrap();
        for installed in [false, true] {
            discovery.detected.store(installed, Ordering::SeqCst);
            assert_eq!(
                observed_bindings(&resolver).await.values().next(),
                Some(&CredentialObservationState::Disabled)
            );
            assert_eq!(
                resolver.capability_observations().await.unwrap()[0]
                    .negotiated()
                    .is_some(),
                installed
            );
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                if installed { 2 } else { 1 }
            );
        }
    }

    #[tokio::test]
    async fn capability_routes_follow_the_selected_profile_without_local_credentials() {
        // C1 installed host CLIs exceed the authored allowlist; C2 no host-login
        // bindings exist; C3 allowlist is empty/one/two CLIs. R1 only the selected
        // routes are acquired, negotiated, and refreshed; R2 C2 never erases
        // protocol evidence or manufactures credential evidence; R3 empty means
        // none, not automatic discovery. This catches the previously masked
        // detected-but-unselected routes when capabilities followed bindings.
        use std::sync::atomic::{AtomicUsize, Ordering};
        for cli_ids in [vec![], vec!["codex"], vec!["claude", "codex"]] {
            let directory = tempfile::tempdir().unwrap();
            let fixture = Arc::new(CapabilityFixture {
                success: true,
                calls: AtomicUsize::new(0),
            });
            let prepared = prepare_host_acp_with(
                LocalAcpPreparation {
                    probe_cwd: directory.path().join("probe"),
                    wrapper_root: directory.path().join("wrappers"),
                    initial_workspace: None,
                    selected_cli_ids: Some(cli_ids.iter().map(|id| (*id).to_string()).collect()),
                    credentials: Arc::new(InMemoryCredentialRepo::new()),
                },
                Arc::new(AvailableDiscovery),
                fixture.clone(),
                fixture.clone(),
            )
            .await
            .unwrap();
            assert_eq!(prepared.routable_cli_ids(), cli_ids);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), cli_ids.len());
            assert!(
                prepared
                    .resolver
                    .credential_observations()
                    .await
                    .unwrap()
                    .is_empty()
            );
            let capabilities = prepared.resolver.capability_observations().await.unwrap();
            assert_eq!(
                capabilities
                    .iter()
                    .map(|row| row.backend_ref().to_string())
                    .collect::<Vec<_>>(),
                cli_ids
                    .iter()
                    .map(|id| format!("acp:{id}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(fixture.calls.load(Ordering::SeqCst), cli_ids.len() * 2);
        }
    }

    #[tokio::test]
    async fn workspace_credentials_share_one_probe_and_capability_per_host_backend() {
        // Causes: C1 zero/three bindings; C2 each binding Active/Disabled;
        // C3 two bindings share Codex across Workspaces, one uses Claude;
        // C4 negotiation succeeds/fails; C5 another heartbeat refresh occurs.
        // Decision table: every C2 combination (2^3) crosses C4, plus C1=zero.
        // R1 retain every exact credential ref and its independent state.
        // R2 active refs sharing a CLI -> ONE login probe per refresh, never
        //    one per Workspace. A disabled ref must not borrow Available.
        // R3 each configured backend -> ONE capability row/negotiation even
        //    with zero active bindings (Provider-owned ACP needs no host login).
        // R4 failed negotiation -> one failed row, not an earlier verified row.
        // R5 another refresh -> fresh login evidence; no permanent probe cache.
        // This is per-batch de-duplication, not cross-request coalescing or an
        // exactly-once external-probe claim. WorkerObservationCache remains the
        // sole serialized publication/expiry boundary. AcpObservationRefresh.tla
        // covers the two-reference/two-refresh abstraction of this projection.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingDiscovery(AtomicUsize);
        #[async_trait]
        impl AcpDiscovery for CountingDiscovery {
            async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
                self.0.fetch_add(1, Ordering::SeqCst);
                AvailableDiscovery.discover(cli).await
            }
        }
        let repo = InMemoryCredentialRepo::new();
        let mut sources = Vec::new();
        for (workspace, cli) in [("a", "codex"), ("b", "codex"), ("c", "claude")] {
            sources.push(
                ensure_worker_local(
                    &repo,
                    workspace,
                    WorkerLocalBinding::new(format!("acp:{cli}"), "default"),
                    None,
                )
                .await
                .unwrap(),
            );
        }
        for mask in 0..=8 {
            for success in [true, false] {
                let discovery = Arc::new(CountingDiscovery(AtomicUsize::new(0)));
                let negotiator = Arc::new(CapabilityFixture {
                    success,
                    calls: AtomicUsize::new(0),
                });
                let selected = if mask == 8 {
                    Vec::new()
                } else {
                    sources
                        .iter()
                        .enumerate()
                        .map(|(index, source)| {
                            let mut source = source.clone();
                            if mask & (1 << index) == 0 {
                                source.status = CredentialStatus::Disabled;
                            }
                            source
                        })
                        .collect()
                };
                let resolver =
                    AcpLocalCredentialResolver::from_sources(discovery.clone(), selected.clone())
                        .unwrap()
                        .with_capability_observations(
                            negotiator.clone(),
                            [
                                ("codex".into(), vec!["codex-acp".into()]),
                                ("claude".into(), vec!["claude-acp".into()]),
                            ]
                            .into_iter()
                            .collect(),
                            PathBuf::from("."),
                        );
                for refresh in 1..=2 {
                    let credentials = observed_bindings(&resolver).await;
                    assert_eq!(credentials.len(), selected.len(), "R1 mask={mask}");
                    for source in &selected {
                        assert_eq!(
                            credentials[&CredentialRef {
                                id: source.id.0.clone(),
                                revision: 1
                            }],
                            if source.status == CredentialStatus::Active {
                                CredentialObservationState::Available
                            } else {
                                CredentialObservationState::Disabled
                            },
                            "R1/R2 mask={mask}"
                        );
                    }
                    let capabilities = resolver.capability_observations().await.unwrap();
                    assert_eq!(
                        capabilities
                            .iter()
                            .map(|row| row.backend_ref())
                            .collect::<Vec<_>>(),
                        ["acp:claude", "acp:codex"],
                        "R3 mask={mask}"
                    );
                    assert!(
                        capabilities
                            .iter()
                            .all(|row| row.negotiated().is_some() == success),
                        "R4"
                    );
                    assert_eq!(
                        negotiator.calls.load(Ordering::SeqCst),
                        refresh * 2,
                        "R3/R5 mask={mask}"
                    );
                    assert_eq!(
                        discovery.0.load(Ordering::SeqCst),
                        refresh * 2,
                        "R2/R5 mask={mask}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn protocol_capability_does_not_grant_backend_owned_login() {
        // Graph: detected CLI + active registration -> prompt-free negotiation;
        // independent login observation -> BackendOwned credential admission.
        // | rule | login state | negotiation | protocol | credential |
        // | C1 | Available | succeeds | verified | available |
        // | C2 | any other | succeeds | verified | original denial |
        // | C3 | any | fails | unverified | unchanged |
        // | C4 | CLI disappears | not attempted | unavailable | no grant |
        // | C5 | source disabled | still probed | unchanged | disabled |
        // Cross C1-C3 over all six credential states at startup AND heartbeat.
        // Every row supplies the exact observed credential reference; the
        // contract-owned admission test separately covers identity mismatch.
        // C4 keeps installation detection; C5 keeps exact credential revocation
        // without revoking the independent Provider-owned protocol route.
        // No Provider key is present in any negotiation.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for state in [
            CredentialObservationState::Available,
            CredentialObservationState::LoginRequired,
            CredentialObservationState::Expired,
            CredentialObservationState::Invalid,
            CredentialObservationState::ProbeFailed,
            CredentialObservationState::Disabled,
        ] {
            for success in [true, false] {
                let directory = tempfile::tempdir().unwrap();
                let discovery = Arc::new(CapabilityDiscovery {
                    state,
                    detected: AtomicBool::new(true),
                });
                let fixture = Arc::new(CapabilityFixture {
                    success,
                    calls: AtomicUsize::new(0),
                });
                let prepared = prepare_host_acp_with(
                    LocalAcpPreparation {
                        probe_cwd: directory.path().join("probe"),
                        wrapper_root: directory.path().join("wrappers"),
                        initial_workspace: Some("workspace".into()),
                        selected_cli_ids: Some(["claude".into()].into_iter().collect()),
                        credentials: Arc::new(InMemoryCredentialRepo::new()),
                    },
                    discovery.clone(),
                    fixture.clone(),
                    fixture.clone(),
                )
                .await
                .unwrap();
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
                assert_eq!(prepared.effective_profiles.contains_key("claude"), success);
                let credentials = prepared.resolver.credential_observations().await.unwrap();
                assert_eq!(credentials.len(), 1);
                let credential = credentials.into_iter().next().unwrap();
                assert_eq!(credential.state, state);
                // C1-C3 exercise the production use-time revalidator, not a
                // second state-only helper. The neutral contract separately
                // owns the id/revision mismatch decision table.
                assert_eq!(
                    prepared
                        .resolver
                        .revalidate_worker_reference(&credential.credential)
                        .await
                        .is_ok(),
                    state == CredentialObservationState::Available
                );
                let capabilities = prepared.resolver.capability_observations().await.unwrap();
                assert_eq!(capabilities.len(), 1);
                assert_eq!(capabilities[0].negotiated().is_some(), success);
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
                discovery.detected.store(false, Ordering::SeqCst);
                prepared
                    .resolver
                    .last_host_observations
                    .lock()
                    .unwrap()
                    .clear();
                assert!(
                    prepared.resolver.capability_observations().await.unwrap()[0]
                        .negotiated()
                        .is_none()
                );
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
                discovery.detected.store(true, Ordering::SeqCst);
                for binding in prepared.resolver.bindings.lock().unwrap().values_mut() {
                    binding.status = CredentialStatus::Disabled;
                }
                assert_eq!(
                    prepared.resolver.capability_observations().await.unwrap()[0]
                        .negotiated()
                        .is_some(),
                    success
                );
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
                assert_eq!(
                    prepared
                        .resolver
                        .credential_observations()
                        .await
                        .unwrap()
                        .into_iter()
                        .next()
                        .unwrap()
                        .state,
                    CredentialObservationState::Disabled
                );
            }
        }
    }

    async fn observed_bindings(
        resolver: &AcpLocalCredentialResolver,
    ) -> BTreeMap<CredentialRef, CredentialObservationState> {
        // Observation time is freshly sampled, not part of binding identity.
        resolver
            .credential_observations()
            .await
            .unwrap()
            .into_iter()
            .map(|observation| (observation.credential, observation.state))
            .collect()
    }

    #[tokio::test]
    async fn binding_batch_rejection_preserves_every_existing_and_absent_entry() {
        // Cause/effect decision table (the source iterator order is not map order):
        // R1 malformed revision / missing locator / unknown ACP / duplicate id
        //    => reject before any mutation, including a preceding valid source.
        // R2 existing id has another revision, at first/middle/last map position
        //    => reject the WHOLE batch: neither new keys nor status updates leak.
        // R3 empty batch => no-op; retry after rejection => ordinary full success;
        //    same-revision status update => publish the requested Disabled state.
        // Constraint: WorkerLocal rows are generated by the canonical repository;
        // only the named fault is injected. Status-only updates intentionally keep
        // the existing API semantics; this test does not claim revision rotation.
        let repo = InMemoryCredentialRepo::new();
        let mut sources = Vec::new();
        for workspace in ["a", "m", "z"] {
            sources.push(
                ensure_worker_local(
                    &repo,
                    workspace,
                    WorkerLocalBinding::new("acp:codex", "default"),
                    None,
                )
                .await
                .unwrap(),
            );
        }
        for conflict_index in 0..sources.len() {
            for reverse in [false, true] {
                let resolver = AcpLocalCredentialResolver::from_sources(
                    Arc::new(AvailableDiscovery),
                    sources.clone(),
                )
                .unwrap();
                let before = observed_bindings(&resolver).await;
                let fresh = ensure_worker_local(
                    &repo,
                    "b",
                    WorkerLocalBinding::new("acp:claude", "default"),
                    None,
                )
                .await
                .unwrap();
                let mut batch = sources.clone();
                for source in &mut batch {
                    source.status = CredentialStatus::Disabled;
                }
                batch[conflict_index].version += 1;
                batch.push(fresh.clone());
                if reverse {
                    batch.reverse();
                }
                assert!(resolver.add_sources(batch).is_err(), "R2");
                assert_eq!(observed_bindings(&resolver).await, before, "R2");
                resolver.add_sources([]).expect("R3 empty");
                assert_eq!(observed_bindings(&resolver).await, before);
                resolver.add_sources([fresh]).expect("R3 retry");
                assert_eq!(resolver.credential_observations().await.unwrap().len(), 4);
                let mut disabled = sources[0].clone();
                disabled.status = CredentialStatus::Disabled;
                resolver.add_sources([disabled]).expect("R3 status update");
                assert_eq!(
                    observed_bindings(&resolver).await[&CredentialRef {
                        id: sources[0].id.0.clone(),
                        revision: 1,
                    }],
                    CredentialObservationState::Disabled
                );
            }
        }
        for fault in [
            "revision",
            "negative_revision",
            "locator",
            "cli",
            "duplicate",
        ] {
            let resolver = AcpLocalCredentialResolver::from_sources(
                Arc::new(AvailableDiscovery),
                [sources[2].clone()],
            )
            .unwrap();
            let before = observed_bindings(&resolver).await;
            let mut malformed = sources[1].clone();
            match fault {
                "revision" => malformed.version = 0,
                "negative_revision" => malformed.version = -1,
                "locator" => malformed.worker_local_binding = None,
                "cli" => {
                    malformed.worker_local_binding =
                        Some(WorkerLocalBinding::new("acp:unknown", "default"))
                }
                "duplicate" => malformed = sources[0].clone(),
                _ => unreachable!(),
            }
            assert!(
                resolver
                    .add_sources([sources[0].clone(), malformed])
                    .is_err(),
                "R1 {fault}"
            );
            assert_eq!(observed_bindings(&resolver).await, before, "R1 {fault}");
        }
    }

    #[tokio::test]
    async fn binding_batch_concurrent_writers_publish_only_complete_accepted_batches() {
        // R4 two valid batches share an absent id at DIFFERENT revisions:
        // exactly one succeeds; its exclusive key + shared revision both appear;
        // the rejected batch's exclusive key never appears, whichever wins.
        // R5 shared id at the SAME revision: both succeed and their union appears;
        // replay is idempotent. Barriers start real OS threads without sleeps.
        // Twenty-four repetitions exercise scheduling, not exhaustive proof;
        // AcpLocalBindingBatch.tla checks bounded check/commit interleavings.
        let repo = InMemoryCredentialRepo::new();
        let mut sources = Vec::new();
        for workspace in ["a", "b", "z"] {
            sources.push(
                ensure_worker_local(
                    &repo,
                    workspace,
                    WorkerLocalBinding::new("acp:codex", "default"),
                    None,
                )
                .await
                .unwrap(),
            );
        }
        for conflict in [false, true] {
            for _ in 0..24 {
                let resolver =
                    AcpLocalCredentialResolver::from_sources(Arc::new(AvailableDiscovery), [])
                        .unwrap();
                let mut second_shared = sources[2].clone();
                if conflict {
                    second_shared.version = 2;
                }
                let batches = [
                    vec![sources[0].clone(), sources[2].clone()],
                    vec![sources[1].clone(), second_shared],
                ];
                let barrier = std::sync::Barrier::new(3);
                let results = std::thread::scope(|scope| {
                    let handles = batches
                        .iter()
                        .map(|batch| {
                            let resolver = &resolver;
                            let barrier = &barrier;
                            scope.spawn(move || {
                                barrier.wait();
                                resolver.add_sources(batch.clone())
                            })
                        })
                        .collect::<Vec<_>>();
                    barrier.wait();
                    handles
                        .into_iter()
                        .map(|handle| handle.join().unwrap())
                        .collect::<Vec<_>>()
                });
                assert_eq!(
                    results.iter().filter(|result| result.is_ok()).count(),
                    if conflict { 1 } else { 2 }
                );
                let mut expected = BTreeMap::new();
                for (batch, result) in batches.iter().zip(&results) {
                    if result.is_ok() {
                        for source in batch {
                            expected.insert(
                                CredentialRef {
                                    id: source.id.0.clone(),
                                    revision: u64::try_from(source.version).unwrap(),
                                },
                                CredentialObservationState::Available,
                            );
                        }
                        resolver
                            .add_sources(batch.clone())
                            .expect("accepted replay");
                    } else {
                        assert!(
                            resolver.add_sources(batch.clone()).is_err(),
                            "rejected replay"
                        );
                    }
                }
                assert_eq!(observed_bindings(&resolver).await, expected);
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
