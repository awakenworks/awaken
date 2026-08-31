use std::collections::BTreeMap;
use std::path::PathBuf;

use super::Role;

#[derive(Debug, Clone)]
pub struct CoordinatorStoreConfig {
    pub sessions: awaken_control::StoreBackend,
    pub captured_content: awaken_control::StoreBackend,
}

/// Fully resolved bootstrap truth shared by the command, server process, and
/// runtime host. Database URLs and key material are intentionally absent from
/// its rendered reports.
#[derive(Debug, Clone)]
pub struct ResolvedDeployment {
    pub role: Role,
    pub mode: OperatingMode,
    pub bind: String,
    pub internal_bind: Option<String>,
    pub data_dir: PathBuf,
    pub expected_platform_workspace_id: Option<String>,
    pub config_path: PathBuf,
    pub config_file_exists: bool,
    pub no_browser: bool,
    pub suite_hub_url: Option<String>,
    pub ai_sdk_browser_cors: awaken_coordinator::AiSdkBrowserCors,
    pub run_local_pool: bool,
    pub worker_server: Option<String>,
    pub worker: super::WorkerBootstrap,
    pub worker_trust_credentials_file: Option<PathBuf>,
    pub identity_mode: awaken_control::ManagementIdentityMode,
    pub cloud_models: CloudModelMode,
    pub org_id: String,
    pub iam_workspaces: Vec<String>,
    pub cloud_iam: super::CloudIamConfig,
    pub executable_agent_registration: super::ExecutableAgentRegistrationConfig,
    pub control_service: super::ControlServiceConfig,
    pub mcp_bearer_token: Option<String>,
    pub admin_listen: Option<String>,
    pub runtime: awaken_runtime_host::DeploymentConfig,
    pub observability: awaken_observability::ObservabilityConfig,
    pub local_acp_observations: Vec<awaken_acp_application::AcpHostObservation>,
    pub configured_acp_clis: Option<Vec<String>>,
    pub control: awaken_control::ControlStoreConfig,
    pub coordinator: CoordinatorStoreConfig,
    pub resources: ResourceStoreBackend,
    pub(crate) workspace_data_lease_guard: Option<super::WorkspaceDataLeaseGuard>,
    pub seal_key: super::SealKeySource,
    pub deprecations: Vec<String>,
    pub origins: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OperatingMode {
    #[default]
    Local,
    Server,
}

impl OperatingMode {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            other => Err(format!("invalid mode={other:?}: expected local or server")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

/// CLI presentation values have the highest precedence. `None` leaves
/// resolution to the typed config file and defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    pub config_path: Option<PathBuf>,
    pub role: Option<Role>,
    pub data_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub no_browser: Option<bool>,
    pub worker_server: Option<String>,
    pub identity_mode: Option<awaken_control::ManagementIdentityMode>,
    pub cloud_models: Option<CloudModelMode>,
}

/// Whether this process may project and execute Awaken Cloud subscription models.
/// Cloud identity remains independent so users may sign in while staying BYOK-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloudModelMode {
    #[default]
    Disabled,
    Enabled,
}

impl CloudModelMode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "false" => Ok(Self::Disabled),
            "enabled" | "on" | "true" => Ok(Self::Enabled),
            other => Err(format!(
                "invalid cloud_models={other:?}: expected disabled or enabled"
            )),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
        }
    }

    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// One backend family for the complete Resources component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceStoreBackend {
    Embedded(PathBuf),
    Postgres(String),
    PostgresObject {
        url: String,
        object: awaken_resource_persistence::ObjectBackingConfig,
    },
}

impl ResourceStoreBackend {
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Postgres(_) | Self::PostgresObject { .. })
    }

    pub(super) fn validate_dispatch_compatibility(
        &self,
        shared_dispatch: bool,
    ) -> Result<(), &'static str> {
        if shared_dispatch && !self.is_shared() {
            Err("a shared runtime requires resource_database_url to use Postgres")
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_and_resource_backends_form_one_compatible_topology() {
        // Causes: local/shared dispatch × embedded/shared Resources. Effects:
        // accept both complete topologies and harmless shared Resources beside
        // local dispatch; reject only shared claims with node-local Resources.
        // S1 local+embedded=accept; S2 local+shared=accept;
        // S3 shared+shared=accept; S4 shared+embedded=reject.
        let embedded = ResourceStoreBackend::Embedded("/data".into());
        let shared = ResourceStoreBackend::Postgres("postgres://resources".into());
        assert!(
            embedded.validate_dispatch_compatibility(false).is_ok(),
            "S1"
        );
        assert!(shared.validate_dispatch_compatibility(false).is_ok(), "S2");
        assert!(shared.validate_dispatch_compatibility(true).is_ok(), "S3");
        assert!(
            embedded.validate_dispatch_compatibility(true).is_err(),
            "S4"
        );
    }
}
