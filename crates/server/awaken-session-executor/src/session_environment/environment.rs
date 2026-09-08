//! One realized Sandbox shared by every Run attempt in a Session.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::{RawTool, RawToolRegistry, ToolExecutor};
use awaken_sandbox_local::{LocalSandbox, NamespaceSandbox};

use super::HandExecutorFactory;
use super::container_skills::ContainerSkillCache;
use super::session_hand::{HandProjectionUpdate, HandStopProof, SessionHandExecutor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvironmentStopProof {
    NoBoundProcesses,
    Hand(HandStopProof),
    #[cfg(target_os = "linux")]
    ExecutionTreeQuiescent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EnvironmentQuiescenceProof;

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir {
        sandbox: Arc<LocalSandbox>,
        hand: Option<Arc<SessionHandExecutor>>,
    },
    Namespace {
        sandbox: Arc<NamespaceSandbox>,
        hand: Arc<SessionHandExecutor>,
    },
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand: Arc<SessionHandExecutor>,
        skills: Arc<ContainerSkillCache>,
        capabilities: pc::SandboxCapabilities,
    },
}

impl SessionEnvironment {
    /// Resolve the same directory interpretation used by Skill materialization
    /// and discovery. Workdir's lexical root and an isolated workspace are
    /// different physical layouts; neither may be inferred from a model's cwd.
    pub(crate) fn skill_directory_path(&self, directory: &str) -> Result<String, pc::SandboxError> {
        match self {
            Self::Workdir { .. } => super::container_files::sandbox_absolute_path(&format!(
                "/{}",
                directory.trim_start_matches('/')
            )),
            Self::Namespace { .. } | Self::Container { .. } => {
                super::container_files::logical_path(directory)
            }
        }
    }

    pub(crate) fn agent_prompt_path_view(
        &self,
        working_directory: Option<&str>,
        has_acp: bool,
        has_native: bool,
    ) -> Result<crate::managed_resource_projection::AgentPathView, &'static str> {
        if matches!(self, Self::Workdir { .. }) && has_acp && has_native {
            return Err(
                "TrustedHost fallback candidates cannot mix ACP and Native filesystem clients",
            );
        }
        let client = if has_acp {
            super::AgentFilesystemClient::AcpProcess
        } else {
            super::AgentFilesystemClient::NativeTools
        };
        Ok(self
            .agent_filesystem_view(working_directory, client)
            .prompt_paths)
    }

    pub(crate) fn agent_filesystem_view(
        &self,
        working_directory: Option<&str>,
        client: super::AgentFilesystemClient,
    ) -> super::AgentFilesystemView {
        match self {
            Self::Workdir { sandbox, hand } => {
                // The embedded file/Bash tools are rooted at the physical
                // sandbox root, unlike a Hand or ACP process launched inside
                // /workspace. Use that same cwd for all prompt paths; only
                // selected mount prefixes have absolute aliases in these tools.
                if client == super::AgentFilesystemClient::NativeTools && hand.is_none() {
                    return super::AgentFilesystemView {
                        protocol_cwd: sandbox.workspace_path().to_string_lossy().into_owned(),
                        command_cwd: "/".into(),
                        prompt_paths:
                            crate::managed_resource_projection::AgentPathView::workdir_relative("/"),
                    };
                }
                // working_directory is an ACP session option. The Native Hand
                // starts at WorkspaceLayout::ROOT; its Bash tool may cd later,
                // but that must not rewrite the frozen prompt's reference cwd.
                let working_directory = match client {
                    super::AgentFilesystemClient::NativeTools => None,
                    super::AgentFilesystemClient::AcpProcess => working_directory,
                };
                let logical_cwd = working_directory.map_or_else(
                    || pc::WorkspaceLayout::ROOT.to_string(),
                    |relative| {
                        format!(
                            "{}/{}",
                            pc::WorkspaceLayout::ROOT.trim_end_matches('/'),
                            relative
                        )
                    },
                );
                let workspace = sandbox
                    .workspace_path()
                    .join(pc::WorkspaceLayout::ROOT.trim_start_matches('/'));
                let protocol_cwd = working_directory
                    .map_or_else(|| workspace.clone(), |relative| workspace.join(relative));
                super::AgentFilesystemView {
                    protocol_cwd: protocol_cwd.to_string_lossy().into_owned(),
                    command_cwd: working_directory.map_or_else(
                        || pc::WorkspaceLayout::ROOT.to_string(),
                        |relative| {
                            format!(
                                "{}/{}",
                                pc::WorkspaceLayout::ROOT.trim_end_matches('/'),
                                relative
                            )
                        },
                    ),
                    prompt_paths:
                        crate::managed_resource_projection::AgentPathView::workdir_relative(
                            logical_cwd,
                        ),
                }
            }
            Self::Namespace { .. } | Self::Container { .. } => {
                let protocol_cwd = working_directory.map_or_else(
                    || pc::WorkspaceLayout::ROOT.to_string(),
                    |relative| {
                        format!(
                            "{}/{}",
                            pc::WorkspaceLayout::ROOT.trim_end_matches('/'),
                            relative
                        )
                    },
                );
                super::AgentFilesystemView {
                    command_cwd: protocol_cwd.clone(),
                    protocol_cwd,
                    prompt_paths:
                        crate::managed_resource_projection::AgentPathView::sandbox_absolute(),
                }
            }
        }
    }

    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir {
            sandbox: Arc::new(sandbox),
            hand: None,
        }
    }

    pub(super) fn with_workdir_hand(
        mut self,
        config: Option<&super::provider::WorkdirHandConfig>,
    ) -> Self {
        if let (Self::Workdir { sandbox, hand }, Some(config)) = (&mut self, config) {
            *hand = Some(Arc::new(SessionHandExecutor::workdir(
                sandbox.clone(),
                config.factory.clone(),
                &config.binary,
                config.idle_after,
            )));
        }
        self
    }

    #[must_use]
    pub(crate) fn namespace(
        sandbox: NamespaceSandbox,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        Self::namespace_shared(Arc::new(sandbox), hand_factory, hand_bin, hand_idle_after)
    }

    pub(super) fn namespace_shared(
        sandbox: Arc<NamespaceSandbox>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        hand_idle_after: std::time::Duration,
    ) -> Self {
        let hand = Arc::new(SessionHandExecutor::namespace(
            sandbox.clone(),
            hand_factory,
            hand_bin,
            hand_idle_after,
        ));
        Self::Namespace { sandbox, hand }
    }

    pub(super) async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: &str,
        hand_idle_after: std::time::Duration,
        hand_residency: crate::deployment_config::ContainerHandResidency,
        capabilities: pc::SandboxCapabilities,
    ) -> Result<Self, pc::SandboxError> {
        let skills = Arc::new(ContainerSkillCache::default());
        let hand = Arc::new(SessionHandExecutor::container(
            sandbox.clone(),
            skills.clone(),
            hand_factory,
            hand_bin,
            hand_idle_after,
            hand_residency,
        ));
        Ok(Self::Container {
            sandbox,
            hand,
            skills,
            capabilities,
        })
    }

    /// Exact capability evidence of the provider that created this live
    /// environment. Resource hot-plug admission must inspect the resident
    /// environment rather than a process default that may select another tier.
    pub(crate) fn capabilities(&self) -> pc::SandboxCapabilities {
        match self {
            Self::Workdir { .. } => awaken_sandbox_local::LocalProvider::capabilities(),
            Self::Namespace { .. } => awaken_sandbox_local::NamespaceProvider::capabilities(),
            Self::Container { capabilities, .. } => capabilities.clone(),
        }
    }

    /// Validate a complete replacement manifest against the resident backend's
    /// mount guarantees and hot-plug support before any projection is changed.
    pub(crate) fn validate_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        let capabilities = self.capabilities();
        if matches!(self, Self::Workdir { sandbox, .. } if sandbox.has_trusted_host_access()) {
            pc::validate_trusted_host_mount_requirements(next, &capabilities)
        } else {
            pc::validate_mount_requirements(next, &capabilities)
        }
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        if previous != next
            && let Self::Container { sandbox, .. } = self
            && !sandbox.supports_live_mount_replacement(previous, next)
        {
            return Err(pc::SandboxError::new(
                "late mount replacement is unsupported for this container input set",
            ));
        }
        Ok(())
    }

    /// Fence tool dispatch and hibernate the current sandbox-resident Hand before
    /// changing the live resource projection. Workdir has no process-level path
    /// fidelity and retains its existing host-side structured-tool behavior.
    pub(crate) async fn begin_live_projection_update(
        &self,
    ) -> Result<Option<HandProjectionUpdate<'_>>, pc::SandboxError> {
        match self {
            Self::Workdir {
                hand: Some(hand), ..
            }
            | Self::Namespace { hand, .. }
            | Self::Container { hand, .. } => hand.begin_projection_update().await.map(Some),
            Self::Workdir { hand: None, .. } => Ok(None),
        }
    }

    /// Executable Hand for this realized environment. Namespace and Container
    /// execute through one sandbox-resident channel; only the explicitly weaker
    /// Only an explicitly weaker Workdir embedding uses host-side rooted tools;
    /// an installed TrustedHost Hand never falls back to that path.
    pub(crate) fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        match self {
            Self::Workdir {
                hand: Some(hand), ..
            }
            | Self::Namespace { hand, .. }
            | Self::Container { hand, .. } => hand.clone(),
            Self::Workdir { hand: None, .. } => Arc::new(RawToolRegistry::new(self.rooted_tools())),
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir {
                sandbox,
                hand: None,
            } => sandbox.rooted_tools(),
            // Descriptors remain the canonical built-in set; execution is forced
            // through this environment's bound Hand in `SessionCtx`.
            Self::Workdir { hand: Some(_), .. }
            | Self::Namespace { .. }
            | Self::Container { .. } => awaken_ext_builtin_tools::all_hand_tools(),
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir {
                sandbox,
                hand: None,
            } => sandbox.spawn_agent(command).await,
            Self::Workdir {
                hand: Some(hand), ..
            }
            | Self::Namespace { hand, .. }
            | Self::Container { hand, .. } => hand.launcher().spawn_agent(command).await,
        }
    }
}
