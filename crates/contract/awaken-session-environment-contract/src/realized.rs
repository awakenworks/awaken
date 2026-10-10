// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Declares the per-role ports of the one Session-owned environment: fenced disposal and
// @formal reason: checkpoint effects carry the aggregate's fence and authority unchanged to the adapter,
// @formal reason: which revalidates them. The ports add no authority; this link is not a formal-proof claim.
//! The one realized environment a Session owns, as one port per consuming
//! role, and the record of handles the Host carries it as.

use std::any::Any;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_channel::AgentChannel;
use awaken_provisioning_contract as pc;
use awaken_tool_contract::tool::{RawTool, ToolExecutor};
use awaken_tool_contract::tool_batch::ToolExecutionDomain;

use crate::{
    AgentFilesystemClient, AgentFilesystemView, AgentPathView, CapturedArtifact,
    DiscoveredSkillFile, EnvironmentQuiescenceProof, EnvironmentStopProof,
};

/// Durable identity, process stop and physical disposal of the environment.
/// Consumers: the Session environment lifecycle owner, realization, terminal
/// preparation and cleanup, background-task ownership.
#[async_trait]
pub trait EnvironmentLifecycle: Send + Sync {
    /// The durable handle a Session binding persists.
    fn handle(&self) -> pc::SandboxHandle;

    /// Stop only this environment's process bindings, never its filesystem.
    /// Every revocation, terminal harvest and filesystem disposal shares this
    /// edge; both the Hand and any scoped execution tree must be stopped
    /// before a successful proof is returned.
    async fn stop_bound_processes(&self) -> Result<EnvironmentStopProof, pc::SandboxError>;

    /// Prove that no process can still write to the filesystem, refusing a
    /// resident Hand that the sandbox itself retains.
    async fn quiesce(&self) -> Result<EnvironmentQuiescenceProof, pc::SandboxError>;

    /// Reclaim an environment that reached no committed Session ownership: a
    /// capability probe, pre-publication compensation, or a retirement whose
    /// candidate was never published. Owned destruction uses
    /// [`Self::dispose_for_effect`].
    async fn dispose(&self) -> Result<(), pc::SandboxError>;

    /// Forward the aggregate-owned exact Memory evidence acknowledgement to
    /// the concrete sandbox lifecycle owner.
    async fn acknowledge_memory_reconciliation(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        complete_materializations: &[pc::MemoryMaterializationEvidence],
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError>;

    /// Complete every source-dependent disposal participant without deleting
    /// the physical realization.
    async fn prepare_disposal_for_effect(
        &self,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxEffectFence, pc::SandboxError>;

    /// Physically dispose under the aggregate-owned effect fence after the
    /// aggregate durably accepted preparation.
    async fn dispose_for_effect(
        &self,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError>;

    /// Checkpoint the full filesystem under the aggregate-owned effect fence,
    /// or refuse when the backing sandbox cannot checkpoint.
    async fn checkpoint_for_effect(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<pc::SandboxCheckpointRef, pc::SandboxError>;

    /// Remove the checkpoint a terminal Session no longer needs.
    async fn cleanup_checkpoint_for_terminal(
        &self,
        request: &pc::SandboxCheckpointCaptureRequest,
        store: &dyn pc::SandboxCheckpointStore,
        expected_effect_fence: &pc::SandboxEffectFence,
        terminal_effect_fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), pc::SandboxError>;
}

/// Exclusive fence for one live resource-projection replacement. Dropping an
/// uncommitted update leaves the environment's Hand fenced: the physical
/// projection may be partial and only the pending-generation retry can make it
/// authoritative again. Consumer: managed Resource projection.
pub trait LiveProjectionUpdate: Send {
    /// The replacement is complete; reopen tool dispatch.
    fn commit(self: Box<Self>);
}

/// The live Resource projection inside the environment. Consumers: managed
/// Resource projection, realization (owned-path reservation), provisioning
/// (runtime workspace writes).
#[async_trait]
pub trait EnvironmentProjection: Send + Sync {
    /// Exact capability evidence of the provider that created this live
    /// environment; hot-plug admission inspects this, never a process default.
    fn capabilities(&self) -> pc::SandboxCapabilities;

    /// Add a conservative path reservation to the durable handle. The caller
    /// persists that handle before starting the live physical projection.
    fn reserve_owned_path(&self, path: &str) -> Result<(), pc::SandboxError>;

    /// Validate a complete replacement manifest against the resident
    /// backend's mount guarantees and hot-plug support before any change.
    fn validate_live_mount_replacement(
        &self,
        previous: &[pc::MountRequirement],
        next: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError>;

    /// Fence tool dispatch before the live projection changes. `None` when
    /// this environment has no Hand to fence.
    async fn begin_live_projection_update<'a>(
        &'a self,
    ) -> Result<Option<Box<dyn LiveProjectionUpdate + 'a>>, pc::SandboxError>;

    /// Attach a governed mount through the backend's live-injection port;
    /// unsupported tiers fail closed.
    async fn attach_mount(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError>;

    /// Complete an adopted environment from the frozen Session manifest.
    async fn reconcile_adopted_mounts(
        &self,
        requirements: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError>;

    /// Remove one path from the live projection; an absent path succeeds.
    async fn remove_projection_path(&self, logical: &str) -> Result<(), pc::SandboxError>;

    /// Write an ordinary runtime-owned workspace file, distinct from a
    /// governed mount.
    async fn write_workspace_file(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError>;
}

/// Skill directories inside the environment. Consumer: Skill delivery and
/// discovery.
#[async_trait]
pub trait SkillFilesystem: Send + Sync {
    /// Resolve the directory interpretation Skill materialization and
    /// discovery share for this environment's layout.
    fn skill_directory_path(&self, directory: &str) -> Result<String, pc::SandboxError>;

    /// The `SKILL.md`-bearing directories under `subdir`.
    fn scan_skill_dir(&self, subdir: &str) -> Result<Vec<DiscoveredSkillFile>, pc::SandboxError>;

    /// Track `subdir` so a later refresh rescans it.
    fn register_skill_dir(&self, subdir: &str);

    /// Refresh every tracked Skill directory from the live environment.
    async fn refresh_skills(&self) -> Result<(), pc::SandboxError>;

    /// Materialize a read-only file tree under `subdir`; each entry is
    /// `(relative path, bytes, executable)`.
    async fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError>;
}

/// Reading the Agent's output back out of the environment. Consumers:
/// Artifact harvest, terminal preparation.
#[async_trait]
pub trait EnvironmentHarvest: Send + Sync {
    /// Capture Agent-authored output metadata and bytes as one batch.
    async fn capture_artifacts(&self) -> Result<Vec<CapturedArtifact>, pc::SandboxError>;

    /// Read an exact sandbox-absolute mount root obtained from a validated,
    /// frozen Session resource projection.
    async fn list_frozen_mount_files(
        &self,
        mount_path: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError>;
}

/// What a Native Agent run executes against. Consumers: the Session context,
/// the root and child runners, the child substrate, the runtime build, the
/// deferred tool executor and output spiller, the Worker projection
/// synchronizer.
#[async_trait]
pub trait AgentToolEnvironment: Send + Sync {
    /// The executable Hand bound to this environment, or its rooted tools
    /// where the tier explicitly runs them host-side.
    fn tool_executor(&self) -> Arc<dyn ToolExecutor>;

    /// The execution domain a tool batch runs in: the bound Hand's contained
    /// domain, or the neutral default for host-side rooted tools.
    async fn tool_execution_domain(&self) -> Result<ToolExecutionDomain, pc::SandboxError>;

    /// The canonical tool descriptors a Run registers.
    fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>>;

    /// The process, protocol and prompt view of the filesystem for `client`.
    fn agent_filesystem_view(
        &self,
        working_directory: Option<&str>,
        client: AgentFilesystemClient,
    ) -> AgentFilesystemView;

    /// Whether a child runs as the trusted host user.
    fn supports_host_identity(&self) -> bool;

    /// The prompt path view for a Session's candidate clients. A TrustedHost
    /// environment cannot serve ACP and Native filesystem clients at once.
    fn agent_prompt_path_view(
        &self,
        working_directory: Option<&str>,
        has_acp: bool,
        has_native: bool,
    ) -> Result<AgentPathView, &'static str> {
        if self.supports_host_identity() && has_acp && has_native {
            return Err(
                "TrustedHost fallback candidates cannot mix ACP and Native filesystem clients",
            );
        }
        let client = if has_acp {
            AgentFilesystemClient::AcpProcess
        } else {
            AgentFilesystemClient::NativeTools
        };
        Ok(self
            .agent_filesystem_view(working_directory, client)
            .prompt_paths)
    }
}

/// Where an ACP Agent process is launched. Consumers: the ACP channel source,
/// Session MCP attachment.
#[async_trait]
pub trait AgentProcessEnvironment: Send + Sync {
    /// Whether the environment is a container.
    fn is_container(&self) -> bool;

    /// The deployment-selected in-sandbox helper shared with the Session
    /// Hand, when the environment has one.
    fn channel_helper(&self) -> Option<String>;

    /// Whether a child runs as the trusted host user and can consume that
    /// user's PATH/HOME-owned CLI identity.
    fn supports_host_identity(&self) -> bool;

    /// The config home path the Agent process sees.
    fn config_home(&self) -> String;

    /// The jail-relative path the config home is materialized at.
    fn config_home_logical(&self) -> String;

    /// The workspace path the Agent process sees.
    fn workspace_cwd(&self) -> String;

    /// The Agent's filesystem view as an ACP process, derived from the one
    /// validated working directory: sandbox-absolute paths below the
    /// workspace. A tier whose process sees host paths states its own view.
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
            prompt_paths: AgentPathView::sandbox_absolute(),
        }
    }

    /// Write `contents` at a logical path inside the environment.
    async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError>;

    /// Launch the Agent process and open its channel.
    async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannel>), pc::SandboxError>;
}

/// Linux TrustedHost scoped execution: the process tree runs under a
/// renewable Session execution authority. Consumers: execution-authority
/// binding, realization renewal, revocation retirement.
#[async_trait]
pub trait ScopedExecution: Send + Sync {
    /// Whether no authority has been bound yet.
    fn needs_execution_authority(&self) -> bool;

    /// Bind the Session's current execution authority before first spawn.
    fn bind_execution_authority(
        &self,
        authority: Arc<dyn pc::CurrentExecutionAuthority>,
    ) -> Result<bool, pc::SandboxError>;

    /// Refresh every live process's deadline under the bound authority.
    async fn refresh_scoped_execution(&self) -> Result<(), pc::SandboxError>;
}

/// The one realized environment a Session owns, carried as one handle per
/// consuming role over the same adapter. The Host holds this record and each
/// of its modules reaches only the role it names. Consumers: every Host module
/// listed on the roles above.
#[derive(Clone)]
pub struct SessionEnvironment {
    lifecycle: Arc<dyn EnvironmentLifecycle>,
    projection: Arc<dyn EnvironmentProjection>,
    skills: Arc<dyn SkillFilesystem>,
    harvest: Arc<dyn EnvironmentHarvest>,
    agent_tools: Arc<dyn AgentToolEnvironment>,
    agent_process: Arc<dyn AgentProcessEnvironment>,
    repositories: Arc<dyn pc::RepositoryRealizer>,
    scoped_execution: Option<Arc<dyn ScopedExecution>>,
    origin: Arc<dyn Any + Send + Sync>,
}

impl SessionEnvironment {
    /// The handles of one adapter that plays every role; `scoped_execution`
    /// is present only when that adapter runs a scoped execution tree.
    pub fn new<E>(environment: Arc<E>, scoped_execution: Option<Arc<dyn ScopedExecution>>) -> Self
    where
        E: EnvironmentLifecycle
            + EnvironmentProjection
            + SkillFilesystem
            + EnvironmentHarvest
            + AgentToolEnvironment
            + AgentProcessEnvironment
            + pc::RepositoryRealizer
            + Any
            + Send
            + Sync
            + 'static,
    {
        Self {
            lifecycle: environment.clone(),
            projection: environment.clone(),
            skills: environment.clone(),
            harvest: environment.clone(),
            agent_tools: environment.clone(),
            agent_process: environment.clone(),
            repositories: environment.clone(),
            scoped_execution,
            origin: environment,
        }
    }

    #[must_use]
    pub fn lifecycle(&self) -> &Arc<dyn EnvironmentLifecycle> {
        &self.lifecycle
    }

    #[must_use]
    pub fn projection(&self) -> &Arc<dyn EnvironmentProjection> {
        &self.projection
    }

    #[must_use]
    pub fn skills(&self) -> &Arc<dyn SkillFilesystem> {
        &self.skills
    }

    #[must_use]
    pub fn harvest(&self) -> &Arc<dyn EnvironmentHarvest> {
        &self.harvest
    }

    #[must_use]
    pub fn agent_tools(&self) -> &Arc<dyn AgentToolEnvironment> {
        &self.agent_tools
    }

    #[must_use]
    pub fn agent_process(&self) -> &Arc<dyn AgentProcessEnvironment> {
        &self.agent_process
    }

    #[must_use]
    pub fn repositories(&self) -> &Arc<dyn pc::RepositoryRealizer> {
        &self.repositories
    }

    /// The scoped execution tree, present only on a Linux TrustedHost
    /// environment that runs one.
    #[must_use]
    pub fn scoped_execution(&self) -> Option<&Arc<dyn ScopedExecution>> {
        self.scoped_execution.as_ref()
    }

    /// The adapter behind the handles, for the provider that realized it to
    /// rebind or observe its own environment. The Host never reads it.
    #[must_use]
    pub fn origin(&self) -> &(dyn Any + Send + Sync) {
        self.origin.as_ref()
    }
}
