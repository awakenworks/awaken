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
