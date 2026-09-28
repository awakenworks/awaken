// @formal surface obligation=acp.permission_tool_identity_normalization_is_exact
// @formal surface obligation=credential.http_effect_usage_binds_exact_fields_and_placements
// @formal reason: Managed ACP credential delivery boundary.
use super::*;

/// How Awaken-managed model coordinates reach a CLI through its provider
/// environment. Backend-owned selection uses [`super::BackendModelInterface`]
/// instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDelivery {
    /// Env key for the endpoint base URL (e.g. `ANTHROPIC_BASE_URL`).
    pub base_url: &'static str,
    /// Env key for the model name (e.g. `ANTHROPIC_MODEL`).
    pub model: &'static str,
    /// Allowlisted process-secret environment variables, ordered with the
    /// default API-key delivery first.
    pub credential_env: &'static [&'static str],
    /// Extra model-name env keys the CLI reads as tier aliases, all set to the
    /// same resolved model.
    pub aliases: &'static [&'static str],
}

/// Adapter-owned serialization for managed provider route configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedProviderConfigCodec {
    Codex,
    OpenCode {
        provider_package: &'static str,
        credential_env: &'static str,
    },
}

/// Public, secret-free provider configuration projected into one adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedProviderConfigDelivery {
    pub config_env: &'static str,
    pub provider_env: &'static str,
    pub provider_id: &'static str,
    pub wire_api: &'static str,
    pub requires_openai_auth: bool,
    pub codec: ManagedProviderConfigCodec,
}

impl ModelDelivery {
    #[must_use]
    pub fn default_credential_env(self) -> Option<&'static str> {
        self.credential_env.first().copied()
    }

    #[must_use]
    pub fn supports_credential_env(self, name: &str) -> bool {
        self.credential_env.contains(&name)
    }
}

/// Provider-owned credential artifact codecs supported by the managed launch
/// boundary. The codec is catalog data; generic Host code never branches on a
/// CLI id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialArtifactCodec {
    CodexAuthJson,
}

/// One managed artifact selected from a CLI profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialArtifactSpec {
    pub codec: CredentialArtifactCodec,
    pub relative_path: &'static str,
}

/// How an isolated, Awaken-managed launch receives provider credentials. This
/// does not describe backend-owned local login; that mutually exclusive mode
/// never materializes provider material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedCredentialDelivery {
    ProcessSecret,
    Artifact(CredentialArtifactSpec),
}

impl ManagedCredentialDelivery {
    /// Select an artifact only when this profile and the pinned credential shape
    /// require one.
    #[must_use]
    pub fn credential_artifact(self, _has_refresh: bool) -> Option<CredentialArtifactSpec> {
        match self {
            Self::Artifact(spec) => Some(spec),
            Self::ProcessSecret => None,
        }
    }

    #[must_use]
    pub fn allows_process_secret(self) -> bool {
        matches!(self, Self::ProcessSecret)
    }
}

pub(super) fn project_acp_session(
    cli_id: &str,
    acp: Option<&awaken_runtime_contract::resolved::AcpExecutionProfile>,
) -> (
    Option<String>,
    Vec<awaken_protocol_acp::SessionConfigOptionSelection>,
    Option<awaken_protocol_acp::AcpCapabilityExpectation>,
) {
    let Some(profile) = acp else {
        return (None, Vec::new(), None);
    };
    (
        profile.session_configuration.mode.clone(),
        profile
            .session_configuration
            .options
            .iter()
            .map(
                |(config_id, value)| awaken_protocol_acp::SessionConfigOptionSelection {
                    config_id: config_id.clone(),
                    value: value.clone(),
                },
            )
            .collect(),
        Some(awaken_protocol_acp::AcpCapabilityExpectation {
            adapter_id: cli_id.to_string(),
            adapter_version: profile.capability_adapter_version.clone(),
            fingerprint: profile.capability_fingerprint.clone(),
        }),
    )
}

impl super::AcpCli {
    #[must_use]
    pub fn supports_model_api_dialect(self, dialect: &str) -> bool {
        self.model_api_dialects.contains(&dialect)
    }
}

/// One already-selected process-secret requirement. The opaque reference is
/// resolved only by the final launch provider through `SecretBroker`; this type
/// carries no material, policy, or credential-selection behavior.
#[derive(Clone, PartialEq, Eq)]
pub struct ProcessSecretRequirement {
    reference: String,
    environment_variable: Option<String>,
}

/// One claim-fenced provider credential artifact. The executor carries only the
/// broker reference and provider-owned relative path; bytes are materialized by
/// the sandbox immediately before the ACP process starts.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialArtifactRequirement {
    reference: String,
    relative_path: String,
}

impl CredentialArtifactRequirement {
    #[must_use]
    pub fn new(reference: impl Into<String>, relative_path: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            relative_path: relative_path.into(),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
}

impl std::fmt::Debug for CredentialArtifactRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialArtifactRequirement(***)")
    }
}

impl ProcessSecretRequirement {
    #[must_use]
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            environment_variable: None,
        }
    }

    #[must_use]
    pub fn for_environment(
        reference: impl Into<String>,
        environment_variable: impl Into<String>,
    ) -> Self {
        Self {
            reference: reference.into(),
            environment_variable: Some(environment_variable.into()),
        }
    }

    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    #[must_use]
    pub fn environment_variable(&self) -> Option<&str> {
        self.environment_variable.as_deref()
    }
}

impl std::fmt::Debug for ProcessSecretRequirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessSecretRequirement(***)")
    }
}

/// Mutually exclusive model ownership handed to the launch projection. Managed
/// launches carry endpoint and broker requirements; backend-owned launches carry
/// only an explicit default/exact policy and never have fields for material.
#[derive(Debug, Clone)]
pub enum ResolvedModel {
    Managed {
        base_url: String,
        model: String,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
        acp: Option<awaken_runtime_contract::resolved::AcpExecutionProfile>,
        provider_server_tools: Vec<awaken_runtime_contract::resolved::ProviderServerTool>,
    },
    BackendOwned {
        model_selection: BackendModelSelection,
        model: String,
        capability: awaken_protocol_acp::AcpCapabilityExpectation,
        session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration,
    },
}

impl ResolvedModel {
    /// Cloud-managed egress (D-R2, ADR-0021 §9/R2). An ACP CLI runs inside the
    /// untrusted sandbox, so it must never hold a raw provider key: its egress is
    /// mediated by the gateway. `base_url` is the **gateway**, and the CLI's "API
    /// key" is a brokered requirement for a short-lived **lease token** — the gateway
    /// injects the real provider credential out of the sandbox's address space. Build
    /// the ACP model this way from a resolved cloud-managed gateway (base URL + lease
    /// reference) so neither the lease nor raw key enters planning state as plaintext.
    #[must_use]
    pub fn cloud_managed_gateway(
        gateway_base_url: impl Into<String>,
        model: impl Into<String>,
        lease_reference: impl Into<String>,
    ) -> Self {
        Self::Managed {
            base_url: gateway_base_url.into(),
            model: model.into(),
            process_secret: Some(ProcessSecretRequirement::new(lease_reference)),
            credential_artifact: None,
            acp: None,
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn managed(
        base_url: impl Into<String>,
        model: impl Into<String>,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
    ) -> Self {
        Self::Managed {
            base_url: base_url.into(),
            model: model.into(),
            process_secret,
            credential_artifact,
            acp: None,
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn managed_with_acp(
        base_url: impl Into<String>,
        model: impl Into<String>,
        process_secret: Option<ProcessSecretRequirement>,
        credential_artifact: Option<CredentialArtifactRequirement>,
        acp: awaken_runtime_contract::resolved::AcpExecutionProfile,
    ) -> Self {
        Self::Managed {
            base_url: base_url.into(),
            model: model.into(),
            process_secret,
            credential_artifact,
            acp: Some(acp),
            provider_server_tools: Vec::new(),
        }
    }

    #[must_use]
    pub fn backend_owned(
        model_selection: BackendModelSelection,
        model: impl Into<String>,
        adapter_id: impl Into<String>,
        adapter_version: impl Into<String>,
        fingerprint: impl Into<String>,
        session_configuration: awaken_runtime_contract::resolved::AcpSessionConfiguration,
    ) -> Self {
        Self::BackendOwned {
            model_selection,
            model: model.into(),
            capability: awaken_protocol_acp::AcpCapabilityExpectation {
                adapter_id: adapter_id.into(),
                adapter_version: adapter_version.into(),
                fingerprint: fingerprint.into(),
            },
            session_configuration,
        }
    }

    #[must_use]
    pub fn credential_artifact(&self) -> Option<&CredentialArtifactRequirement> {
        match self {
            Self::Managed {
                credential_artifact,
                ..
            } => credential_artifact.as_ref(),
            Self::BackendOwned { .. } => None,
        }
    }

    /// Attach the exact provider-owned tools selected by the immutable Agent
    /// publication. The ACP adapter still validates that its CLI, API dialect,
    /// and provider route can realize every entry before launch.
    #[must_use]
    pub fn with_provider_server_tools(
        mut self,
        tools: impl IntoIterator<Item = awaken_runtime_contract::resolved::ProviderServerTool>,
    ) -> Self {
        if let Self::Managed {
            provider_server_tools,
            ..
        } = &mut self
        {
            provider_server_tools.extend(tools);
        }
        self
    }
}
