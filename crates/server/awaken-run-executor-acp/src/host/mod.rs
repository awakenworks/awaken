// @formal entrypoint worker_credential.host_observation_projection_is_backend_scoped
// @formal entrypoint worker_credential.local_binding_batch_is_atomic
//! Host-side ACP preparation and liveness adapters. No Control repository or secret store.

#[cfg(test)]
mod binding_batch_tests;
#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "loom"))]
use loom::sync::Mutex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(all(test, feature = "loom")))]
use std::sync::Mutex;

use crate::{AcpCli, acp_cli};
use async_trait::async_trait;
pub use awaken_acp_contract::AcpCapabilityNegotiator;
use awaken_acp_contract::{AcpCapabilityObservation, AcpCapabilityObservationSource};
use awaken_lease_contract::{WallClock, WallClockExt};
use awaken_runtime_contract::{
    CredentialMaterialError, CredentialObservation, CredentialObservationSource,
    CredentialObservationState, CredentialRef, WorkerLocalReferenceRevalidator,
};

mod capability_probe;
mod host_discovery;
mod projected_bindings;
pub use capability_probe::{
    AcpCapabilityState, EffectiveAcpCapabilityProfile, HostAcpCapabilityNegotiator,
};
pub use host_discovery::{AcpDetectionState, AcpDiscovery, AcpHostDiscovery, AcpHostObservation};
pub use projected_bindings::{AcpLocalCredentialBinding, ProjectedAcpCredentials};

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
        let crate::AcpAcquisition::PinnedNpmWrapper {
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

/// Worker-owned inputs for discovery/acquisition, without any authoring store.
pub struct HostAcpPreparation {
    pub probe_cwd: PathBuf,
    pub wrapper_root: PathBuf,
    pub selected_cli_ids: Option<BTreeSet<String>>,
    pub clock: Arc<dyn WallClock>,
}

/// One production assembly shared by CLI, Coordinator and standalone Worker.
/// The composition root supplies the neutral protocol handshake implementation.
pub struct HostAcpAdapters {
    pub discovery: Arc<dyn AcpDiscovery>,
    pub installer: Arc<dyn AcpWrapperInstaller>,
    pub negotiator: Arc<dyn AcpCapabilityNegotiator>,
}

impl HostAcpAdapters {
    #[must_use]
    pub fn production(handshake: Arc<dyn awaken_acp_contract::AcpCapabilityHandshake>) -> Self {
        Self {
            discovery: Arc::new(AcpHostDiscovery::local(std::time::Duration::from_secs(3))),
            installer: Arc::new(NpmWrapperInstaller),
            negotiator: Arc::new(HostAcpCapabilityNegotiator::new(
                std::time::Duration::from_secs(10),
                handshake,
            )),
        }
    }
}

/// Secret-free output installed into deployment profile and Worker builder.
pub struct PreparedHostAcpCapabilities {
    pub observations: Vec<AcpHostObservation>,
    pub launch_argv: BTreeMap<String, Vec<String>>,
    pub effective_profiles: BTreeMap<String, EffectiveAcpCapabilityProfile>,
    pub resolver: Arc<AcpLocalCredentialResolver>,
    selected_cli_ids: BTreeSet<String>,
}

/// Prepare the one host profile without registering any Control credential.
/// Standalone Workers consume external, secret-free binding projections instead.
pub async fn prepare_host_acp_capabilities_with(
    input: HostAcpPreparation,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
) -> Result<PreparedHostAcpCapabilities, String> {
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
                    input.clock.now_ms(),
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
        AcpLocalCredentialResolver::from_bindings(discovery, [], input.clock.clone())?
            .with_capability_observations(negotiator, capability_routes, input.probe_cwd),
    );
    Ok(PreparedHostAcpCapabilities {
        observations,
        launch_argv,
        effective_profiles,
        resolver,
        selected_cli_ids,
    })
}

impl PreparedHostAcpCapabilities {
    pub fn selected_cli_ids(&self) -> impl Iterator<Item = &str> {
        self.selected_cli_ids.iter().map(String::as_str)
    }

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
}

const fn host_observation_is_routable(
    selected_backend: bool,
    detected: bool,
    credential_available: bool,
    capability_verified: bool,
) -> bool {
    selected_backend && detected && (!credential_available || capability_verified)
}

#[cfg(kani)]
/// @obligation worker_credential.host_observation_projection_is_backend_scoped
/// @proof-input-debt tautology host_observation_is_routable
#[kani::proof]
fn host_observation_projection_is_exactly_backend_scoped() {
    // Causes: C1 backend selected; C2 executable detected; C3 current login is
    // available; C4 the protocol capability was verified. Effects: E1 project
    // a routable backend observation or E2 exclude it. When C3 is false, the
    // route remains visible for execution-time login revalidation.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // |---|---|---|---|---|---|
    // | R1 | no | any | any | any | E2 |
    // | R2 | yes | no | any | any | E2 |
    // | R3 | yes | yes | no | any | E1 |
    // | R4 | yes | yes | yes | no | E2 |
    // | R5 | yes | yes | yes | yes | E1 |
    let selected_backend: bool = kani::any();
    let detected: bool = kani::any();
    let credential_available: bool = kani::any();
    let capability_verified: bool = kani::any();
    assert_eq!(
        host_observation_is_routable(
            selected_backend,
            detected,
            credential_available,
            capability_verified,
        ),
        selected_backend && detected && (!credential_available || capability_verified)
    );
}

fn routable_cli_ids(
    observations: &[AcpHostObservation],
    selected_cli_ids: &BTreeSet<String>,
) -> Vec<String> {
    observations
        .iter()
        .filter(|observation| {
            host_observation_is_routable(
                selected_cli_ids.contains(&observation.cli_id),
                observation.detected(),
                observation.credential_state == Some(CredentialObservationState::Available),
                observation.capability_state == Some(AcpCapabilityState::Verified),
            )
        })
        .map(|observation| observation.cli_id.clone())
        .collect()
}

#[derive(Clone)]
struct LocalBinding {
    credential: CredentialRef,
    cli: &'static AcpCli,
    enabled: bool,
}

/// Per-entry admission used while the resolver holds the one publication lock.
/// The caller evaluates every addition before extending the map, so this closed
/// decision is the only way an existing identity can admit a batch update.
const fn local_binding_update_admitted(existing: bool, exact_credential: bool) -> bool {
    !existing || exact_credential
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalBindingBatchAdmission {
    every_member_admitted: bool,
}

impl LocalBindingBatchAdmission {
    const fn empty() -> Self {
        Self {
            every_member_admitted: true,
        }
    }

    const fn include(self, member_admitted: bool) -> Self {
        Self {
            every_member_admitted: self.every_member_admitted && member_admitted,
        }
    }

    const fn permits_publication(self) -> bool {
        self.every_member_admitted
    }
}

/// An accepted batch owns the complete replacement map. The live map remains
/// immutably borrowed throughout validation and staging; only this value can
/// cross the later single-assignment publication edge while the mutex is held.
struct LocalBindingPublication(BTreeMap<String, LocalBinding>);

fn stage_local_binding_batch(
    current: &BTreeMap<String, LocalBinding>,
    additions: BTreeMap<String, LocalBinding>,
) -> Result<LocalBindingPublication, String> {
    let admission = additions.iter().fold(
        LocalBindingBatchAdmission::empty(),
        |admission, (id, binding)| {
            let existing = current.get(id);
            admission.include(local_binding_update_admitted(
                existing.is_some(),
                existing.is_none_or(|existing| existing.credential == binding.credential),
            ))
        },
    );
    if !admission.permits_publication() {
        let conflicting_id = additions
            .iter()
            .find_map(|(id, binding)| {
                current
                    .get(id)
                    .is_some_and(|existing| existing.credential != binding.credential)
                    .then_some(id)
            })
            .expect("a rejected binding batch has a conflicting identity");
        return Err(format!(
            "conflicting WorkerLocal credential id `{conflicting_id}`"
        ));
    }

    let mut publication = current.clone();
    publication.extend(additions);
    Ok(LocalBindingPublication(publication))
}

#[cfg(kani)]
/// @obligation worker_credential.local_binding_batch_is_atomic
#[kani::proof]
fn local_binding_batch_publication_is_complete_or_stutters() {
    // Causes: C1-C3 are the exact admission results for three arbitrary batch
    // members; C4 is the prior publication bit for each member. Effects: E1 an
    // accepted batch publishes all three requested bits; E2 any rejected
    // member preserves all three prior bits. The production staging function
    // borrows the current map immutably and returns its complete replacement,
    // so this reducer is the only gate to its single assignment.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | R1 | yes | yes | yes | E1 complete publication |
    // | R2 | no  | any | any | E2 exact stutter |
    // | R3 | any | no  | any | E2 exact stutter |
    // | R4 | any | any | no  | E2 exact stutter |
    let existing = [kani::any::<bool>(), kani::any(), kani::any()];
    let exact = [kani::any::<bool>(), kani::any(), kani::any()];
    let admitted = [
        local_binding_update_admitted(existing[0], exact[0]),
        local_binding_update_admitted(existing[1], exact[1]),
        local_binding_update_admitted(existing[2], exact[2]),
    ];
    let before = [kani::any::<bool>(), kani::any(), kani::any()];
    let requested = [kani::any::<bool>(), kani::any(), kani::any()];
    let admission = admitted
        .into_iter()
        .fold(LocalBindingBatchAdmission::empty(), |batch, member| {
            batch.include(member)
        });
    assert_eq!(
        admission.permits_publication(),
        admitted.into_iter().all(|member| member)
    );
    let after = if admission.permits_publication() {
        requested
    } else {
        before
    };
    if admission.permits_publication() {
        assert_eq!(after, requested);
    } else {
        assert_eq!(after, before);
    }
}

/// Liveness-only resolver for every `acp:*` WorkerLocal source on one Worker.
pub struct AcpLocalCredentialResolver {
    discovery: Arc<dyn AcpDiscovery>,
    clock: Arc<dyn WallClock>,
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
    pub fn from_bindings(
        discovery: Arc<dyn AcpDiscovery>,
        projections: impl IntoIterator<Item = AcpLocalCredentialBinding>,
        clock: Arc<dyn WallClock>,
    ) -> Result<Self, String> {
        let mut bindings = BTreeMap::new();
        for projection in projections {
            let binding = projection.to_local_binding()?;
            if bindings
                .insert(binding.credential.id.clone(), binding)
                .is_some()
            {
                return Err("duplicate WorkerLocal credential id".to_string());
            }
        }
        Ok(Self {
            discovery,
            clock,
            bindings: Mutex::new(bindings),
            last_host_observations: Mutex::new(BTreeMap::new()),
            capability: None,
        })
    }

    pub fn add_bindings(
        &self,
        projections: impl IntoIterator<Item = AcpLocalCredentialBinding>,
    ) -> Result<(), String> {
        let additions =
            Self::from_bindings(self.discovery.clone(), projections, self.clock.clone())?;
        let additions = additions
            .bindings
            .into_inner()
            .expect("new ACP bindings are not poisoned");
        let mut bindings = self.bindings.lock().expect("ACP bindings poisoned");
        // The immutable borrow and owned replacement make partial publication
        // unrepresentable: rejection drops only the staged value, while success
        // performs one assignment under the same lock used for validation.
        let LocalBindingPublication(publication) = stage_local_binding_batch(&bindings, additions)?;
        *bindings = publication;
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
        let observed_at_ms = self.clock.now_ms();
        if !binding.enabled {
            return CredentialObservation {
                credential: binding.credential.clone(),
                state: CredentialObservationState::Disabled,
                observed_at_ms,
                reason_code: Some("worker_local_source_disabled".to_string()),
            };
        }
        let observation =
            host_discovery::discover_scoped(self.discovery.as_ref(), binding.cli).await;
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
            let observed_at_ms = self.clock.now_ms();
            let cached = self
                .last_host_observations
                .lock()
                .expect("ACP host-observation cache poisoned")
                .get(cli.id)
                .cloned();
            let host = match cached {
                Some(observation) => observation,
                None => host_discovery::discover_scoped(self.discovery.as_ref(), cli).await,
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
                        observed_at_ms,
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
            let observation = if binding.enabled {
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
