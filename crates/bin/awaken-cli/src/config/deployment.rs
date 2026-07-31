use std::path::PathBuf;

use super::Role;

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

/// One backend family for the complete resource plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourcePlaneStoreBackend {
    Embedded(PathBuf),
    Postgres(String),
}

impl ResourcePlaneStoreBackend {
    pub fn is_shared(&self) -> bool {
        matches!(self, Self::Postgres(_))
    }

    pub fn validate_runtime_shape(
        &self,
        shared_runtime: bool,
        shared_resource_catalog: bool,
    ) -> Result<(), &'static str> {
        if shared_runtime && !self.is_shared() {
            Err("a shared Postgres runtime requires resource_database_url")
        } else if shared_runtime && !shared_resource_catalog {
            Err("a shared Postgres runtime requires admin_db to use Postgres")
        } else {
            Ok(())
        }
    }
}
