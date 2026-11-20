// @formal entrypoint worker_credential.host_observation_projection_is_backend_scoped
// @formal entrypoint worker_credential.local_binding_batch_is_atomic
//! Host-side ACP preparation and liveness adapters. No Control repository or secret store.
//!
//! Type-ownership rule O2: the retired backend capability enum cannot resolve;
//! consumers use the single `awaken_acp_contract::AcpCapabilityObservationState`.
//! O1's positive example is attached to the contract-owned host observation.
//! These cases check ownership at compilation, not process probe behavior.
//!
//! ```compile_fail
//! use awaken_run_executor_acp::host::AcpCapabilityState;
//! ```
//!
//! Binding rule A4: the existing secret-free binding record lives only in the
//! contract, not behind a host-module compatibility export.
//!
//! ```compile_fail
//! use awaken_run_executor_acp::host::AcpLocalCredentialBinding;
//! ```

#[cfg(test)]
mod binding_batch_tests;
#[cfg(test)]
mod tests;

#[cfg(not(all(test, feature = "loom")))]
use awaken_sync::LeafLock;
/// Under Loom the leaf lock is Loom's mutex, so the model explores its
/// interleavings; acquisition keeps `LeafLock`'s infallible shape.
#[cfg(all(test, feature = "loom"))]
struct LeafLock<T>(loom::sync::Mutex<T>);

#[cfg(all(test, feature = "loom"))]
impl<T> LeafLock<T> {
    fn new(value: T) -> Self {
        Self(loom::sync::Mutex::new(value))
    }

    fn lock(&self) -> loom::sync::MutexGuard<'_, T> {
        self.0
            .lock()
            .expect("a Loom model never poisons the leaf lock")
    }

    fn into_inner(self) -> T {
        self.0
            .into_inner()
            .expect("a Loom model never poisons the leaf lock")
    }
}
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use crate::{AcpCli, acp_cli};
use async_trait::async_trait;
pub use awaken_acp_contract::AcpCapabilityNegotiator;
use awaken_acp_contract::{
    AcpCapabilityObservation, AcpCapabilityObservationSource, AcpCapabilityObservationState,
    AcpCommand, AcpDetectionState, AcpHostObservation, AcpLocalCredentialBinding,
};
use awaken_agent_contract::credential::CredentialRef;
use awaken_credential_contract::{
    CredentialMaterialError, CredentialObservation, CredentialObservationSource,
    CredentialObservationState, WorkerLocalReferenceRevalidator,
};
use awaken_time::{WallClock, WallClockExt};

mod acquisition;
mod capability_probe;
mod command;
mod diagnostics;
mod runtime;
pub use runtime::inspect_host_runtime;
mod host_discovery;
mod npm_launch;
mod process_output;
mod projected_bindings;
pub use capability_probe::{EffectiveAcpCapabilityProfile, HostAcpCapabilityNegotiator};
pub use diagnostics::diagnostic_summary;
pub use host_discovery::{AcpDiscovery, AcpHostDiscovery};
pub use projected_bindings::ProjectedAcpCredentials;

/// Apply the configured selection before starting any host process and bind
/// every reply to the exact requested catalog identity.
pub async fn discover_selected(
    discovery: &dyn AcpDiscovery,
    selected: Option<&BTreeSet<String>>,
) -> Vec<AcpHostObservation> {
    let mut observations = Vec::new();
    for cli in crate::known_acp_clis()
        .iter()
        .filter(|cli| selected.is_none_or(|selected| selected.contains(cli.id)))
    {
        observations.push(host_discovery::discover_scoped(discovery, cli).await);
    }
    observations
}

pub use acquisition::{
    AcpWrapperInstaller, NpmWrapperInstaller, prepared_host_argv, prepared_host_command,
};
#[cfg(test)]
use acquisition::{canonical_launch_argv, installed_wrapper_matches};

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
            discovery: Arc::new(AcpHostDiscovery::local(std::time::Duration::from_secs(10))),
            installer: Arc::new(NpmWrapperInstaller),
            negotiator: Arc::new(
                // Use the same per-step budget as actual ACP turns. Cold
                // session/new may populate the CLI's model/config caches.
                HostAcpCapabilityNegotiator::new(std::time::Duration::from_secs(70), handshake)
                    .with_phase_timeout(std::time::Duration::from_secs(30)),
            ),
        }
    }
}

/// Secret-free output installed into deployment profile and Worker builder.
pub struct PreparedHostAcpCapabilities {
    pub observations: Vec<AcpHostObservation>,
    pub launch_argv: BTreeMap<String, Vec<String>>,
    pub launch_commands: BTreeMap<String, AcpCommand>,
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
    let mut observations =
        discover_selected(discovery.as_ref(), input.selected_cli_ids.as_ref()).await;
    let mut launch_argv = BTreeMap::new();
    let mut launch_commands = BTreeMap::new();
    for observation in observations.iter_mut().filter(|observation| {
        observation.detected()
            && input
                .selected_cli_ids
                .as_ref()
                .is_none_or(|selected| selected.contains(&observation.cli_id))
    }) {
        let cli = acp_cli(&observation.cli_id).expect("discovery returns catalog ids");
        match installer.prepared_command(cli, &input.wrapper_root).await {
            Ok(Some(command)) => {
                launch_argv.insert(observation.cli_id.clone(), command.argv.clone());
                launch_commands.insert(observation.cli_id.clone(), command);
            }
            Ok(None) => {}
            Err(error) => {
                warn_preparation(cli.id, "wrapper preparation failed", &error);
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
        let command = launch_commands
            .get(&observation.cli_id)
            .cloned()
            .unwrap_or_else(|| AcpCommand::new(cli.acquisition.local_argv()));
        let mut negotiated = negotiator
            .negotiate_command(&command, &input.probe_cwd, cli.auth_method_id)
            .await;
        if let Err(error) = &negotiated {
            warn_preparation(cli.id, "capability probe failed; retrying once", error);
            // A fresh, prompt-free process may recover a transient cold-start
            // failure. Never retry a model turn or dispatch any tool here.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            negotiated = negotiator
                .negotiate_command(&command, &input.probe_cwd, cli.auth_method_id)
                .await;
        }
        match negotiated {
            Ok(negotiated) => {
                let mut profile = EffectiveAcpCapabilityProfile::verified(
                    &observation.cli_id,
                    command
                        .runtime
                        .as_ref()
                        .map(|runtime| runtime.cli_version.as_str())
                        .or(observation.version.as_deref())
                        .unwrap_or("unknown"),
                    negotiated,
                    input.clock.now_ms(),
                );
                profile.runtime_identity = command.runtime.clone();
                observation.capability_state = Some(AcpCapabilityObservationState::Verified);
                observation.capability_fingerprint = Some(profile.fingerprint.clone());
                observation.capability_reason_code = None;
                effective_profiles.insert(observation.cli_id.clone(), profile);
            }
            Err(error) => {
                warn_preparation(cli.id, "capability negotiation failed", &error);
                observation.capability_state = Some(AcpCapabilityObservationState::ProbeFailed);
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
            let command = launch_commands
                .get(cli_id)
                .cloned()
                .unwrap_or_else(|| AcpCommand::new(cli.acquisition.local_argv()));
            (cli_id.clone(), command)
        })
        .collect();
    let resolver = Arc::new(
        AcpLocalCredentialResolver::from_bindings(discovery, [], input.clock.clone())?
            .with_capability_commands(negotiator, capability_routes, input.probe_cwd),
    );
    Ok(PreparedHostAcpCapabilities {
        observations,
        launch_argv,
        launch_commands,
        effective_profiles,
        resolver,
        selected_cli_ids,
    })
}

fn warn_preparation(cli: &str, stage: &str, error: &str) {
    let summary = diagnostic_summary(error);
    tracing::warn!(cli, stage, error = %summary, "Host ACP preparation failed");
    // AllInOne prepares local executors before the service tracing subscriber
    // starts. Preserve a bounded, redacted diagnostic at that startup boundary.
    if !tracing::dispatcher::has_been_set() {
        eprintln!("ACP {cli}: {stage}: {summary}");
    }
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

    #[must_use]
    pub fn routable_launch_commands(&self) -> BTreeMap<String, AcpCommand> {
        let routable: BTreeSet<_> = self.routable_cli_ids().into_iter().collect();
        self.launch_commands
            .iter()
            .filter(|(id, _)| routable.contains(*id))
            .map(|(id, command)| (id.clone(), command.clone()))
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
                observation.capability_state == Some(AcpCapabilityObservationState::Verified),
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

/// Liveness-only resolver for every `acp:*` WorkerLocal source on one Worker.
pub struct AcpLocalCredentialResolver {
    discovery: Arc<dyn AcpDiscovery>,
    clock: Arc<dyn WallClock>,
    bindings: LeafLock<BTreeMap<String, LocalBinding>>,
    last_host_observations: LeafLock<BTreeMap<String, AcpHostObservation>>,
    capability: Option<CapabilityObservationConfig>,
}

struct CapabilityObservationConfig {
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
    launch_commands: BTreeMap<String, AcpCommand>,
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
            let binding = projected_bindings::to_local_binding(&projection)?;
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
            bindings: LeafLock::new(bindings),
            last_host_observations: LeafLock::new(BTreeMap::new()),
            capability: None,
        })
    }

    pub fn add_bindings(
        &self,
        projections: impl IntoIterator<Item = AcpLocalCredentialBinding>,
    ) -> Result<(), String> {
        let additions =
            Self::from_bindings(self.discovery.clone(), projections, self.clock.clone())?;
        let additions = additions.bindings.into_inner();
        let mut bindings = self.bindings.lock();
        // The immutable borrow and owned replacement make partial publication
        // unrepresentable: rejection drops only the staged value, while success
        // performs one assignment under the same lock used for validation.
        let LocalBindingPublication(publication) = stage_local_binding_batch(&bindings, additions)?;
        *bindings = publication;
        Ok(())
    }

    #[must_use]
    pub fn with_capability_observations(
        self,
        negotiator: Arc<dyn AcpCapabilityNegotiator>,
        launch_argv: BTreeMap<String, Vec<String>>,
        cwd: PathBuf,
    ) -> Self {
        self.with_capability_commands(
            negotiator,
            launch_argv
                .into_iter()
                .map(|(id, argv)| (id, AcpCommand::new(argv)))
                .collect(),
            cwd,
        )
    }

    #[must_use]
    pub fn with_capability_commands(
        mut self,
        negotiator: Arc<dyn AcpCapabilityNegotiator>,
        launch_commands: BTreeMap<String, AcpCommand>,
        cwd: PathBuf,
    ) -> Self {
        self.capability = Some(CapabilityObservationConfig {
            negotiator,
            launch_commands,
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
        for (cli_id, command) in &capability.launch_commands {
            let cli = acp_cli(cli_id)
                .ok_or_else(|| format!("ACP capability route names unknown CLI `{cli_id}`"))?;
            let observed_at_ms = self.clock.now_ms();
            let cached = self.last_host_observations.lock().get(cli.id).cloned();
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
            let mut command = command.clone();
            let result = async {
                if command.runtime.is_some() {
                    command.runtime = Some(inspect_host_runtime(cli, &command).await?);
                }
                capability
                    .negotiator
                    .negotiate_command(&command, &capability.cwd, cli.auth_method_id)
                    .await
            }
            .await;
            match result {
                Ok(negotiated) => {
                    let mut profile = EffectiveAcpCapabilityProfile::verified(
                        cli.id,
                        command
                            .runtime
                            .as_ref()
                            .map(|runtime| runtime.cli_version.as_str())
                            .or(host.version.as_deref())
                            .unwrap_or("unknown"),
                        negotiated,
                        observed_at_ms,
                    );
                    profile.runtime_identity = command.runtime.clone();
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
        self.last_host_observations.lock().clear();
        let mut host_observations: BTreeMap<&str, CredentialObservation> = BTreeMap::new();
        let bindings = self.bindings.lock().values().cloned().collect::<Vec<_>>();
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
            .get(&credential.id)
            .filter(|binding| binding.credential.revision == credential.revision)
            .cloned()
            .ok_or(CredentialMaterialError::Unavailable)?;
        awaken_credential_contract::require_exact_available_worker_reference(
            credential,
            self.observe(&binding).await,
        )
    }
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive bool(2) x bool(2) x bool(2) x bool(2) = 16
    /// @obligation worker_credential.host_observation_projection_is_backend_scoped
    #[test]
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
        for selected_backend in [false, true] {
            for detected in [false, true] {
                for credential_available in [false, true] {
                    for capability_verified in [false, true] {
                        assert_eq!(
                            host_observation_is_routable(
                                selected_backend,
                                detected,
                                credential_available,
                                capability_verified,
                            ),
                            selected_backend
                                && detected
                                && (!credential_available || capability_verified)
                        );
                    }
                }
            }
        }
    }

    /// @exhaustive [bool; 3](8) x [bool; 3](8) x [bool; 3](8) x [bool; 3](8) = 4096
    /// @obligation worker_credential.local_binding_batch_is_atomic
    #[test]
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
        for bits in 0_u16..1 << 12 {
            let bit = |index: u16| bits & (1 << index) != 0;
            let existing = [bit(0), bit(1), bit(2)];
            let exact = [bit(3), bit(4), bit(5)];
            let admitted = [
                local_binding_update_admitted(existing[0], exact[0]),
                local_binding_update_admitted(existing[1], exact[1]),
                local_binding_update_admitted(existing[2], exact[2]),
            ];
            let before = [bit(6), bit(7), bit(8)];
            let requested = [bit(9), bit(10), bit(11)];
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
    }
}
