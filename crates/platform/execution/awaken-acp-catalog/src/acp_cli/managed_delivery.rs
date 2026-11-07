// @formal surface obligation=acp.permission_tool_identity_normalization_is_exact
// @formal surface obligation=credential.http_effect_usage_binds_exact_fields_and_placements
// @formal reason: Managed ACP credential delivery boundary.
pub use awaken_acp_contract::managed_credential::{
    CredentialArtifactCodec, CredentialArtifactRequirement, CredentialArtifactSpec,
    ManagedCredentialDelivery,
};

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

impl super::AcpCli {
    #[must_use]
    pub fn supports_model_api_dialect(self, dialect: &str) -> bool {
        self.model_api_dialects.contains(&dialect)
    }
}
