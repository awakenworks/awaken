//! ACP-facing capabilities of the one Session environment.

use async_trait::async_trait;
use awaken_provisioning_contract as pc;

use super::SessionEnvironment;

/// One derived view of the process, ACP, and prompt-visible filesystem.
///
/// `command_cwd` remains sandbox-logical so every provider consumes the same
/// command contract. `protocol_cwd` is the path visible to the ACP process.
/// `prompt_paths` deliberately omits the Workdir provider's private host root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentFilesystemView {
    pub protocol_cwd: String,
    pub command_cwd: String,
    pub prompt_paths: crate::managed_resource_projection::AgentPathView,
}

/// The component that will interpret a model-visible resource path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentFilesystemClient {
    /// Awaken Hand tools consume provider-neutral sandbox-absolute paths.
    NativeTools,
    /// An ACP CLI consumes paths relative to its protocol working directory on
    /// TrustedHost and sandbox-absolute paths in Namespace/Container.
    AcpProcess,
}

/// Segregated capability needed by the ACP channel adapter. Keeping it beside
/// the Session owner avoids teaching the neutral provisioning contract about
/// async byte channels.
#[async_trait]
pub(crate) trait AgentSandbox: Send + Sync {
    fn is_container(&self) -> bool;

    /// Whether a child runs as the trusted host user and can consume that user's
    /// PATH/HOME-owned CLI identity. Namespace and container environments cannot
    /// truthfully provide this without mounting/copying credentials.
    fn supports_host_identity(&self) -> bool;

    fn config_home(&self) -> String;

    /// Jail-relative/interior path used to materialize the config home. This is
    /// distinct from [`Self::config_home`] on the Workdir tier, where the child
    /// sees an absolute host path but the file writer accepts only a logical
    /// path below the Session root.
    fn config_home_logical(&self) -> String;

    /// Workspace path understood by the ACP agent inside this environment.
    fn workspace_cwd(&self) -> String;

    /// Derive every cwd/path projection from one validated Agent configuration.
    fn filesystem_view(&self, working_directory: Option<&str>) -> AgentFilesystemView {
        let workspace = self.workspace_cwd();
        let (protocol_cwd, command_cwd) = working_directory.map_or_else(
            || (workspace.clone(), workspace),
            |relative| {
                let logical = format!(
                    "{}/{}",
                    pc::WorkspaceLayout::ROOT.trim_end_matches('/'),
                    relative
                );
                (logical.clone(), logical)
            },
        );
        AgentFilesystemView {
            protocol_cwd,
            command_cwd,
            prompt_paths: crate::managed_resource_projection::AgentPathView::sandbox_absolute(),
        }
    }

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError>;

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    >;
}

#[async_trait]
impl AgentSandbox for SessionEnvironment {
    fn is_container(&self) -> bool {
        matches!(self, Self::Container { .. })
    }

    fn supports_host_identity(&self) -> bool {
        matches!(self, Self::Workdir(_))
    }

    fn config_home(&self) -> String {
        match self {
            // A bound container is already running and its root filesystem may
            // be read-only. Late ACP config therefore lives in the one writable,
            // Session-owned workspace rather than the creation-time /acp-config
            // mount used by the legacy one-shot source.
            Self::Container { .. } | Self::Namespace { .. } => {
                pc::WorkspaceLayout::ACP_CONFIG_ROOT.to_string()
            }
            Self::Workdir(sandbox) => sandbox
                .workspace_path()
                .join(pc::WorkspaceLayout::ACP_CONFIG_SUBDIR)
                .to_string_lossy()
                .into_owned(),
        }
    }

    fn config_home_logical(&self) -> String {
        match self {
            Self::Container { .. } | Self::Namespace { .. } => {
                pc::WorkspaceLayout::ACP_CONFIG_ROOT.to_string()
            }
            Self::Workdir(_) => pc::WorkspaceLayout::ACP_CONFIG_SUBDIR.to_string(),
        }
    }

    fn workspace_cwd(&self) -> String {
        match self {
            Self::Workdir(sandbox) => sandbox.workspace_path().to_string_lossy().into_owned(),
            Self::Namespace { .. } | Self::Container { .. } => {
                pc::WorkspaceLayout::ROOT.to_string()
            }
        }
    }

    fn filesystem_view(&self, working_directory: Option<&str>) -> AgentFilesystemView {
        self.agent_filesystem_view(working_directory, AgentFilesystemClient::AcpProcess)
    }

    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        self.materialize_inline(logical, contents).await
    }

    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        self.spawn_agent(command).await
    }
}
