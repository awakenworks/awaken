//! Immutable Worker capability selection and standard manifest derivation.

use std::collections::BTreeSet;

use awaken_runtime_contract::execution::NATIVE_RUNTIME_CAPABILITY;
use awaken_runtime_host::InferenceExecutorMaterializer;
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
    max_concurrent: u32,
}

impl Default for StandardManifestConfig {
    fn default() -> Self {
        Self {
            build_digest: env!("CARGO_PKG_VERSION").to_string(),
            zone: None,
            extra_capabilities: Default::default(),
            max_concurrent: std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(1),
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
        self.max_concurrent = max_concurrent;
        self
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
        Option<&awaken_runtime_host::PinnedCredentialMaterializer>,
    )> for ResourceManifestSupport
{
    fn from(
        (memory_mounter, credentials): (
            bool,
            Option<&awaken_runtime_host::PinnedCredentialMaterializer>,
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
    pub(crate) deployment: &'a awaken_runtime_host::DeploymentConfig,
    pub(crate) materializer: Option<&'a dyn InferenceExecutorMaterializer>,
    pub(crate) credential_materializer: Option<CredentialMaterializerSupport>,
    /// An exact Worker-local observation/revalidation resolver is installed.
    /// This is independent of whether that resolver returns secret material.
    pub(crate) worker_local_credentials: bool,
    pub(crate) remote_credential_realization:
        Option<&'a awaken_runtime_contract::CredentialRealizationCapabilities>,
    pub(crate) sandbox_override:
        Option<(awaken_provisioning_contract::SandboxCapabilities, &'a str)>,
    pub(crate) resource_support: ResourceManifestSupport,
    pub(crate) application_capabilities: BTreeSet<String>,
    pub(crate) config: &'a StandardManifestConfig,
}

#[derive(Clone)]
pub(crate) struct CredentialMaterializerSupport {
    pub(crate) provider_adapter: awaken_runtime_contract::CredentialRealizationCapabilities,
    pub(crate) process_secret: awaken_runtime_contract::CredentialRealizationCapabilities,
    pub(crate) worker_relay: awaken_runtime_contract::CredentialRealizationCapabilities,
}

impl From<&awaken_runtime_host::PinnedCredentialMaterializer> for CredentialMaterializerSupport {
    fn from(materializer: &awaken_runtime_host::PinnedCredentialMaterializer) -> Self {
        Self {
            provider_adapter: materializer.provider_adapter_capabilities(),
            process_secret: materializer.process_secret_capabilities(),
            worker_relay: materializer.worker_relay_capabilities(),
        }
    }
}

pub(crate) fn derive_standard_manifest(inputs: StandardManifestInputs<'_>) -> WorkerManifest {
    let (sandbox, backend) = inputs
        .sandbox_override
        .unwrap_or_else(|| inputs.deployment.sandbox_support());
    let mut capabilities = BTreeSet::from([NATIVE_RUNTIME_CAPABILITY.to_string()]);
    capabilities.extend(
        inputs
            .materializer
            .into_iter()
            .flat_map(InferenceExecutorMaterializer::supported_access_schemes)
            .map(|capability| (*capability).to_string()),
    );
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
    if inputs.worker_local_credentials {
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
        .is_some_and(|materializer| !materializer.provider_adapter.material_sources.is_empty())
    {
        capabilities.insert(PROVIDER_CREDENTIAL_SOURCE_CAPABILITY.to_string());
    }
    let mut credential_profiles = Vec::new();
    if let Some(materializer) = inputs.materializer {
        credential_profiles.push(materializer.credential_realization_capabilities());
    }
    if let Some(remote) = inputs.remote_credential_realization {
        credential_profiles.push(remote.clone());
    }
    if let Some(materializer) = inputs.credential_materializer.as_ref() {
        credential_profiles.push(materializer.provider_adapter.clone());
    }
    if let Some(materializer) = inputs
        .credential_materializer
        .as_ref()
        .filter(|_| inputs.deployment.acp.is_some())
    {
        credential_profiles.push(materializer.process_secret.clone());
    }
    if let Some(materializer) = inputs
        .credential_materializer
        .as_ref()
        .filter(|_| sandbox.supports_secret_egress_without_bypass())
    {
        credential_profiles.push(materializer.worker_relay.clone());
    }
    let mut credential_realization =
        awaken_runtime_contract::CredentialRealizationCapabilities::alternatives(
            credential_profiles,
        );
    if credential_realization.alternatives.len() == 1 {
        credential_realization = credential_realization
            .alternatives
            .pop()
            .expect("one credential profile exists");
    }
    if let Some(capability) = credential_realization
        .manifest_capability()
        .expect("credential realization capability serializes")
    {
        capabilities.insert(capability);
    }
    WorkerManifest {
        build_digest: inputs.config.build_digest.clone(),
        capabilities,
        zone: inputs.config.zone.clone(),
        sandbox,
        sandbox_backends: BTreeSet::from([backend.to_string()]),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        checkpoint_formats: BTreeSet::from(["stream-v1".to_string()]),
        capacity: WorkerCapacity {
            max_concurrent: inputs.config.max_concurrent,
            ..WorkerCapacity::default()
        },
        ..WorkerManifest::default()
    }
}
