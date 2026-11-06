// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
//! Immutable Worker capability selection and standard manifest derivation.

use std::collections::BTreeSet;

use awaken_runtime_contract::execution::NATIVE_RUNTIME_CAPABILITY;
use awaken_worker_contract::{
    PROVIDER_CREDENTIAL_SOURCE_CAPABILITY, REPOSITORY_CREDENTIALS_CAPABILITY,
    SESSION_RESOURCES_CAPABILITY, VersionRange, WORKER_LOCAL_CREDENTIALS_CAPABILITY,
    WorkerCapacity, WorkerManifest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManifestKind {
    Explicit,
    Standard,
}

impl std::fmt::Display for ManifestKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Explicit => "explicit",
            Self::Standard => "standard",
        })
    }
}

pub(crate) enum ManifestSource {
    Explicit(Box<WorkerManifest>),
    Standard {
        application_capabilities: BTreeSet<String>,
    },
}

impl ManifestSource {
    fn kind(&self) -> ManifestKind {
        match self {
            Self::Explicit(_) => ManifestKind::Explicit,
            Self::Standard { .. } => ManifestKind::Standard,
        }
    }
}

pub(crate) enum ManifestSelection {
    Unset,
    Selected(ManifestSource),
    Conflict {
        first: ManifestKind,
        second: ManifestKind,
    },
}

impl ManifestSelection {
    /// A standard manifest derives its capacity from this Worker's
    /// configuration; on Kubernetes it must be explicit (ADR-0092). The builder
    /// checks this before any cluster connection and again at build. An
    /// explicit manifest carries its own capacity.
    pub(crate) fn require_capacity(
        &self,
        config: &StandardManifestConfig,
        deployment: &awaken_deployment_config::DeploymentConfig,
    ) -> Result<(), crate::WorkerNodeBuildError> {
        if matches!(self, Self::Selected(ManifestSource::Standard { .. })) {
            config
                .max_concurrent_for(deployment.sandbox_tier)
                .map_err(crate::WorkerNodeBuildError)?;
        }
        Ok(())
    }

    pub(crate) fn select(&mut self, next: ManifestSource) {
        let current = std::mem::replace(self, Self::Unset);
        *self = match current {
            Self::Unset => Self::Selected(next),
            Self::Selected(previous) if previous.kind() == next.kind() => Self::Selected(next),
            Self::Selected(previous) => Self::Conflict {
                first: previous.kind(),
                second: next.kind(),
            },
            conflict @ Self::Conflict { .. } => conflict,
        };
    }
}

/// Typed metadata used only by the canonical standard manifest derivation.
///
/// Embedding code gets deterministic defaults. Process adapters resolve their
/// configuration once; the Builder itself never rereads process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandardManifestConfig {
    build_digest: String,
    zone: Option<String>,
    extra_capabilities: BTreeSet<String>,
    /// Operator-set capacity. Absent, sandboxes running on this host default
    /// to its parallelism; a Kubernetes Worker refuses to start (ADR-0092).
    max_concurrent: Option<u32>,
}

impl Default for StandardManifestConfig {
    fn default() -> Self {
        Self {
            build_digest: env!("CARGO_PKG_VERSION").to_string(),
            zone: None,
            extra_capabilities: Default::default(),
            max_concurrent: None,
        }
    }
}

impl StandardManifestConfig {
    #[must_use]
    pub fn new(build_digest: impl Into<String>) -> Self {
        Self {
            build_digest: build_digest.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_zone(mut self, zone: impl Into<String>) -> Self {
        self.zone = Some(zone.into());
        self
    }

    #[must_use]
    pub fn with_extra_capabilities(
        mut self,
        capabilities: impl IntoIterator<Item = String>,
    ) -> Self {
        self.extra_capabilities = capabilities.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_max_concurrent(mut self, max_concurrent: u32) -> Self {
        self.max_concurrent = Some(max_concurrent);
        self
    }

    /// The manifest capacity for `tier`. Kubernetes sandboxes consume cluster
    /// resources, which the Worker's own CPU count says nothing about, so a
    /// Kubernetes Worker must set it explicitly; every other backend runs its
    /// sandboxes on this host and defaults to its parallelism.
    pub(crate) fn max_concurrent_for(
        &self,
        tier: awaken_deployment_config::SandboxTier,
    ) -> Result<u32, String> {
        match (self.max_concurrent, tier) {
            (Some(max_concurrent), _) => Ok(max_concurrent),
            (None, awaken_deployment_config::SandboxTier::K8s) => Err(
                "worker_max_concurrent must be set on a Kubernetes Worker: it bounds the \
                 simultaneous sandbox Pods this Worker may run"
                    .into(),
            ),
            (None, _) => Ok(std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(1)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceManifestSupport {
    None,
    Session,
    SessionWithRepositoryCredentials,
}

impl
    From<(
        bool,
        Option<&awaken_credential_materializer::PinnedCredentialMaterializer>,
    )> for ResourceManifestSupport
{
    fn from(
        (memory_mounter, credentials): (
            bool,
            Option<&awaken_credential_materializer::PinnedCredentialMaterializer>,
        ),
    ) -> Self {
        match (memory_mounter, credentials) {
            (true, Some(_)) => Self::SessionWithRepositoryCredentials,
            (true, None) => Self::Session,
            (false, _) => Self::None,
        }
    }
}

pub(crate) struct StandardManifestInputs<'a> {
    pub(crate) deployment: &'a awaken_deployment_config::DeploymentConfig,
    pub(crate) inference_schemes: &'a BTreeSet<String>,
    pub(crate) inference_credentials:
        &'a awaken_credential_contract::CredentialRealizationCapabilities,
    pub(crate) credential_materializer: Option<CredentialMaterializerSupport>,
    pub(crate) brokered_acp_model_access:
        Option<&'a dyn awaken_run_executor_acp::BrokeredAcpModelAccessMaterializer>,
    /// An exact Worker-local observation/revalidation resolver is installed.
    /// This is independent of whether that resolver returns secret material.
    pub(crate) worker_local_credential_resolver_installed: bool,
    pub(crate) remote_credential_realization:
        Option<&'a awaken_credential_contract::CredentialRealizationCapabilities>,
    pub(crate) sandbox_override:
        Option<(awaken_provisioning_contract::SandboxCapabilities, &'a str)>,
    pub(crate) checkpoint_formats: BTreeSet<String>,
    pub(crate) resource_support: ResourceManifestSupport,
    pub(crate) application_capabilities: BTreeSet<String>,
    pub(crate) config: &'a StandardManifestConfig,
}

#[derive(Clone)]
pub(crate) struct CredentialMaterializerSupport {
    pub(crate) provider_adapter: awaken_credential_contract::CredentialRealizationCapabilities,
    pub(crate) acp: std::collections::BTreeMap<
        String,
        awaken_credential_contract::CredentialRealizationCapabilities,
    >,
    pub(crate) worker_relay: awaken_credential_contract::CredentialRealizationCapabilities,
    pub(crate) repository_process_protocol:
        awaken_credential_contract::CredentialRealizationCapabilities,
}

impl From<&awaken_credential_materializer::PinnedCredentialMaterializer>
    for CredentialMaterializerSupport
{
    fn from(materializer: &awaken_credential_materializer::PinnedCredentialMaterializer) -> Self {
        Self {
            provider_adapter: materializer.provider_adapter_capabilities(),
            acp: awaken_acp_catalog::known_acp_clis()
                .iter()
                .map(|cli| {
                    (
                        cli.id.to_string(),
                        materializer
                            .acp_model_capabilities(cli.id, cli.managed_credential_delivery),
                    )
                })
                .collect(),
            worker_relay: materializer.worker_relay_capabilities(),
            repository_process_protocol: materializer.realization_capabilities(
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
                    .resource_holder,
                awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField,
            ),
        }
    }
}

pub(crate) fn derive_standard_manifest(inputs: StandardManifestInputs<'_>) -> WorkerManifest {
    let (sandbox, backend) = inputs
        .sandbox_override
        .unwrap_or_else(|| inputs.deployment.sandbox_support());
    let mut capabilities = BTreeSet::from([NATIVE_RUNTIME_CAPABILITY.to_string()]);
    // Every backend receives Claimed.request through the same compiled
    // Dispatch decoder. Native/ACP/A2A selection changes execution, not this
    // installed wire support; no operator capability opt-in is needed.
    capabilities.insert(awaken_worker_contract::PREPARED_SESSION_DISPATCH_CAPABILITY.to_owned());
    // Advertise this only with the actual Runtime Returned/KnownAborted codec.
    // Prepared Session command support is an independent capability owned by
    // its wire admission path; neither token substitutes for the other.
    capabilities.insert(awaken_worker_contract::TOOL_RESULT_JOURNAL_CAPABILITY.to_owned());
    // The composition installs this incarnation's Task control (holder,
    // attention and tenure), so it decodes private Task input and executes
    // BackgroundTask (codec demand C1).
    capabilities.insert(awaken_worker_contract::BACKGROUND_TASK_CONTROL_CAPABILITY.to_owned());
    capabilities
        .insert(awaken_worker_contract::SESSION_TERMINAL_SOURCE_ADMISSION_CAPABILITY.to_owned());
    capabilities
        .insert(awaken_runtime_contract::activation::INHERITED_TOOLSETS_CAPABILITY.to_string());
    capabilities.extend(inputs.inference_schemes.iter().cloned());
    match inputs.resource_support {
        ResourceManifestSupport::None => {}
        ResourceManifestSupport::Session => {
            capabilities.insert(SESSION_RESOURCES_CAPABILITY.to_string());
        }
        ResourceManifestSupport::SessionWithRepositoryCredentials => {
            capabilities.insert(SESSION_RESOURCES_CAPABILITY.to_string());
            capabilities.insert(REPOSITORY_CREDENTIALS_CAPABILITY.to_string());
        }
    }
    if inputs.worker_local_credential_resolver_installed {
        capabilities.insert(WORKER_LOCAL_CREDENTIALS_CAPABILITY.to_string());
    }
    capabilities.extend(inputs.application_capabilities);
    if let Some(profile) = &inputs.deployment.acp {
        capabilities.extend(profile.cli_ids().map(|cli| format!("acp:{cli}")));
    }
    capabilities.extend(inputs.config.extra_capabilities.iter().cloned());
    if inputs
        .credential_materializer
        .as_ref()
        .is_some_and(|materializer| !materializer.provider_adapter.is_empty())
    {
        capabilities.insert(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY.to_string());
    }
    let mut credential_profiles = Vec::new();
    if !inputs.inference_credentials.is_empty() {
        credential_profiles.push(inputs.inference_credentials.clone());
    }
    if let Some(remote) = inputs.remote_credential_realization {
        credential_profiles.push(remote.clone());
    }
    if let Some(materializer) = inputs.credential_materializer.as_ref() {
        credential_profiles.push(materializer.provider_adapter.clone());
    }
    if let (Some(materializer), Some(acp)) = (
        inputs.credential_materializer.as_ref(),
        &inputs.deployment.acp,
    ) {
        for cli_id in acp.cli_ids() {
            if let Some(profile) = materializer.acp.get(cli_id) {
                credential_profiles.push(profile.clone());
            }
        }
    }
    if let (Some(materializer), Some(acp)) =
        (inputs.brokered_acp_model_access, &inputs.deployment.acp)
    {
        credential_profiles.extend(acp.cli_ids().filter_map(|cli_id| {
            awaken_acp_catalog::acp_cli(cli_id)
                .map(|cli| materializer.credential_realization_capabilities(*cli))
        }));
    }
    if let Some(materializer) = inputs
        .credential_materializer
        .as_ref()
        .filter(|_| sandbox.supports_secret_egress_without_bypass())
    {
        credential_profiles.push(materializer.worker_relay.clone());
    }
    if let Some(materializer) = inputs.credential_materializer.as_ref().filter(|_| {
        inputs.resource_support == ResourceManifestSupport::SessionWithRepositoryCredentials
    }) {
        credential_profiles.push(materializer.repository_process_protocol.clone());
    }
    let credential_realization =
        awaken_credential_contract::CredentialRealizationCapabilities::alternatives(
            credential_profiles,
        );
    if let Some(capability) = credential_realization
        .manifest_capability()
        .expect("credential realization capability serializes")
    {
        capabilities.insert(capability);
    }
    let mut sandbox_backends = BTreeSet::from([backend.to_string()]);
    let trusted_host_sandbox = inputs.deployment.trusted_host_support();
    if trusted_host_sandbox.is_some() {
        sandbox_backends.insert(awaken_worker_contract::TRUSTED_HOST_SANDBOX_BACKEND.to_string());
    }
    WorkerManifest {
        build_digest: inputs.config.build_digest.clone(),
        capabilities,
        zone: inputs.config.zone.clone(),
        sandbox,
        trusted_host_sandbox,
        sandbox_tool_recovery: inputs
            .deployment
            .sandbox
            .container_hand_residency
            .recovery_capability(),
        sandbox_backends,
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange {
            min: 1,
            max: awaken_worker_contract::CURRENT_RUNTIME_PROTOCOL_VERSION,
        },
        checkpoint_formats: BTreeSet::from(["stream-v1".to_string()])
            .into_iter()
            .chain(inputs.checkpoint_formats)
            .collect(),
        capacity: WorkerCapacity {
            // The builder refuses an unset Kubernetes capacity before deriving;
            // a zero here is rejected again by manifest validation.
            max_concurrent: inputs
                .config
                .max_concurrent_for(inputs.deployment.sandbox_tier)
                .unwrap_or(0),
            ..WorkerCapacity::default()
        },
        ..WorkerManifest::default()
    }
}

/// The local Session tier this Worker proved before registration, and the
/// probe it re-runs on every heartbeat (ADR-0092 decision 5).
#[derive(Clone)]
pub(crate) struct PreparedLocalSandbox {
    pub(crate) capabilities: awaken_provisioning_contract::SandboxCapabilities,
    pub(crate) backend: &'static str,
    pub(crate) readiness: crate::lifecycle::LocalReadinessProbe,
}

impl PreparedLocalSandbox {
    /// Probe the deployment's local tier (an unavailable Namespace degrades to
    /// Workdir only under the explicit fallback opt-in) and keep the effective
    /// tier's manifest projection with its readiness probe.
    pub(crate) async fn probe(
        deployment: &awaken_deployment_config::DeploymentConfig,
    ) -> Result<Self, crate::WorkerNodeBuildError> {
        let base = awaken_deployment_config::session_sandbox_base(deployment);
        let tier = awaken_execution_composition::resolve_sandbox_tier(
            deployment.sandbox_tier,
            deployment.sandbox.allow_local_fallback,
            &base,
        )
        .await
        .map_err(crate::WorkerNodeBuildError)?;
        let readiness = awaken_execution_composition::local_tier_readiness(tier, &base)
            .ok_or_else(|| {
                crate::WorkerNodeBuildError(format!("{tier:?} is not a local Session tier"))
            })?;
        let (capabilities, backend) = deployment.sandbox_support_for_tier(tier);
        Ok(Self {
            capabilities,
            backend,
            readiness: std::sync::Arc::new(move || {
                let provider = readiness.clone();
                Box::pin(async move { provider.probe_ready().await })
            }),
        })
    }
}

/// The local Session provider installed into the Host must still be the one this
/// Worker registered; a change after registration refuses Ready.
pub(crate) fn ensure_registered_local_provider(
    prepared: &Option<PreparedLocalSandbox>,
    manifest: &awaken_worker_contract::WorkerManifest,
    deps: &awaken_session_executor::SessionExecutorBuilder,
) -> std::io::Result<()> {
    let Some(PreparedLocalSandbox { backend, .. }) = prepared else {
        return Ok(());
    };
    if deps
        .installed_session_environment_provider()
        .readiness()
        .capabilities()
        != manifest.sandbox
    {
        return Err(std::io::Error::other(format!(
            "effective local Session provider changed after Worker registration for {backend}; refusing Ready"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod capacity_tests {
    use super::*;
    use awaken_deployment_config::SandboxTier;

    #[test]
    fn kubernetes_capacity_must_be_explicit_and_host_backends_keep_the_default() {
        // Decision table (ADR-0092 decision 4). Causes: C1 sandbox tier; C2
        // `worker_max_concurrent` set. Effects: E1 the configured value; E2 the
        // host's parallelism; E3 refusal naming the setting (fail closed).
        // | Rule | C1 | C2 | effect |
        // | K1 | K8s | unset | E3 |
        // | K2 | K8s | set | E1 |
        // | K3 | Local/Namespace/Docker/Podman | unset | E2 |
        // | K4 | any host tier | set | E1 |
        let parallelism = std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(1);
        let unset = StandardManifestConfig::default();
        let set = StandardManifestConfig::default().with_max_concurrent(4);
        let refusal = unset.max_concurrent_for(SandboxTier::K8s).unwrap_err();
        assert!(refusal.contains("worker_max_concurrent"), "K1: {refusal}");
        assert!(refusal.contains("sandbox Pods"), "K1: {refusal}");
        assert_eq!(set.max_concurrent_for(SandboxTier::K8s), Ok(4), "K2");
        for tier in [
            SandboxTier::Local,
            SandboxTier::Namespace,
            SandboxTier::Docker,
            SandboxTier::Podman,
        ] {
            assert_eq!(
                unset.max_concurrent_for(tier),
                Ok(parallelism),
                "K3 {tier:?}"
            );
            assert_eq!(set.max_concurrent_for(tier), Ok(4), "K4 {tier:?}");
        }
    }

    #[tokio::test]
    async fn a_registered_kubernetes_worker_without_capacity_refuses_to_start() {
        // K1 at the builder: the standard manifest of a Kubernetes Worker
        // without `worker_max_concurrent` is refused before any cluster
        // connection (prepare); an explicit manifest carries its own capacity
        // and is not refused by this rule (K5).
        let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
        deployment.sandbox_tier = SandboxTier::K8s;
        let builder = || {
            crate::WorkerNodeBuilder::new(
                awaken_worker_transport_security::WorkerUpstream::new("https://control.test"),
                awaken_time::system_clock(),
            )
            .with_deployment_config(deployment.clone(), false)
            .with_standard_manifest(Default::default())
        };
        let prepared = builder()
            .prepare_session_environment_from_deployment()
            .await;
        let Err(error) = prepared else {
            panic!("an unset Kubernetes capacity must be refused before connecting");
        };
        assert!(
            error.to_string().contains("worker_max_concurrent"),
            "{error}"
        );
        // The build-time recheck uses the same rule (behind the topology
        // checks, which refuse a Kubernetes build without its provider first).
        let standard = ManifestSelection::Selected(ManifestSource::Standard {
            application_capabilities: Default::default(),
        });
        let unset = StandardManifestConfig::default();
        assert!(
            standard.require_capacity(&unset, &deployment).is_err(),
            "K1"
        );
        let set = StandardManifestConfig::default().with_max_concurrent(2);
        assert!(standard.require_capacity(&set, &deployment).is_ok(), "K2");
        let explicit = ManifestSelection::Selected(ManifestSource::Explicit(Box::default()));
        assert!(
            explicit
                .require_capacity(&StandardManifestConfig::default(), &deployment)
                .is_ok(),
            "K5"
        );
    }
}
