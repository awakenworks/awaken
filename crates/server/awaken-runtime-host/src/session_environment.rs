//! The single live execution environment owned by a Session.
//!
//! Host code depends on this capability object instead of retaining a concrete
//! sandbox in `SessionCtx`. Workdir is the first adapter; Namespace and Container
//! plug into the same owner without adding another Native/ACP lifecycle.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::tool::{RawTool, ToolExecutor};
use awaken_sandbox_local::{DiscoveredSkillFile, LocalSandbox, NamespaceSandbox};

mod container_files;
mod container_repositories;
mod container_skills;
mod provider;
use container_skills::{ContainerSkillCache, RefreshingHandExecutor};
pub(crate) use provider::SessionEnvironmentProvider;

/// Composition port that binds a live hand channel to the runtime's neutral tool
/// executor. The framing implementation belongs to an outer composition crate;
/// this host owns only the Session lifecycle and never imports the relay adapter.
pub trait HandExecutorFactory: Send + Sync {
    fn bind(
        &self,
        channel: Box<dyn AgentChannelType>,
        operation_scope: &str,
    ) -> Arc<dyn ToolExecutor>;
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
    ///
    /// This is the host-realized absolute root for Workdir and the stable interior
    /// path for transparent Namespace/Container tiers. It is sent on ACP
    /// `session/new`/`session/load`, whose cwd is authoritative for CLI file tools.
    fn workspace_cwd(&self) -> String;

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

/// One realized sandbox shared by every Run attempt in a Session.
pub(crate) enum SessionEnvironment {
    Workdir(Arc<LocalSandbox>),
    Namespace(Arc<NamespaceSandbox>),
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand: Arc<RefreshingHandExecutor>,
        skills: Arc<ContainerSkillCache>,
        capabilities: pc::SandboxCapabilities,
    },
}

impl SessionEnvironment {
    fn sandbox(&self) -> &dyn pc::Sandbox {
        match self {
            Self::Workdir(sandbox) => sandbox.as_ref(),
            Self::Namespace(sandbox) => sandbox.as_ref(),
            Self::Container { sandbox, .. } => sandbox.as_ref(),
        }
    }

    pub(crate) fn handle(&self) -> pc::SandboxHandle {
        self.sandbox().handle()
    }

    pub(crate) async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
        self.sandbox().status().await
    }

    pub(crate) async fn dispose(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::dispose(sandbox.as_ref()).await,
            Self::Container { sandbox, hand, .. } => {
                hand.stop().await;
                sandbox.dispose().await
            }
        }
    }

    #[must_use]
    pub(crate) fn workdir(sandbox: LocalSandbox) -> Self {
        Self::Workdir(Arc::new(sandbox))
    }

    #[must_use]
    pub(crate) fn namespace(sandbox: NamespaceSandbox) -> Self {
        Self::Namespace(Arc::new(sandbox))
    }

    async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        hand_factory: Arc<dyn HandExecutorFactory>,
        hand_bin: &str,
        capabilities: pc::SandboxCapabilities,
    ) -> Result<Self, pc::SandboxError> {
        let skills = Arc::new(ContainerSkillCache::default());
        let hand = Arc::new(
            RefreshingHandExecutor::new(sandbox.clone(), skills.clone(), hand_factory, hand_bin)
                .await?,
        );
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
            Self::Workdir(_) => awaken_sandbox_local::LocalProvider::capabilities(),
            Self::Namespace(_) => awaken_sandbox_local::NamespaceProvider::capabilities(),
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
        pc::validate_mount_requirements(next, &self.capabilities())
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

    /// Executable Hand for this realized environment. Container environments
    /// already own a channel-backed executor; local/namespace environments use
    /// their rooted tool implementations behind the same neutral port.
    pub(crate) fn tool_executor(&self) -> Arc<dyn ToolExecutor> {
        match self {
            Self::Container { hand, .. } => hand.clone(),
            Self::Workdir(_) | Self::Namespace(_) => Arc::new(EnvironmentToolExecutor {
                tools: self
                    .rooted_tools()
                    .into_iter()
                    .map(|tool| (tool.id().to_string(), tool))
                    .collect(),
            }),
        }
    }

    pub(crate) fn rooted_tools(&self) -> Vec<Arc<dyn RawTool>> {
        match self {
            Self::Workdir(sandbox) => sandbox.rooted_tools(),
            Self::Namespace(sandbox) => sandbox.rooted_tools(),
            // Descriptors remain the canonical built-in set; execution is forced
            // through this environment's bound remote hand in `SessionCtx`.
            Self::Container { .. } => awaken_ext_builtin_tools::executable_hand_tools(),
        }
    }

    /// Enumerate Agent-authored outputs through the provisioning contract's
    /// canonical Artifact port. Container backends expose the same contract over
    /// their output-file transport instead of creating a second host-side scanner.
    pub(crate) async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Namespace(sandbox) => pc::Sandbox::artifacts(sandbox.as_ref()).await,
            Self::Container { sandbox, .. } => sandbox
                .read_files(sandbox.outputs_path())
                .await
                .map(|files| {
                    files
                        .into_iter()
                        .map(|file| {
                            let id = awaken_file_store::content_id(&file.bytes);
                            pc::Artifact {
                                id: id.clone(),
                                path: file.path,
                                size_bytes: file.bytes.len() as u64,
                                content_hash: id,
                            }
                        })
                        .collect()
                }),
        }
    }

    pub(crate) async fn read_artifact(&self, id: &str) -> Result<Vec<u8>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
            Self::Namespace(sandbox) => pc::Sandbox::read_artifact(sandbox.as_ref(), id).await,
            Self::Container { sandbox, .. } => sandbox
                .read_files(sandbox.outputs_path())
                .await?
                .into_iter()
                .find_map(|file| {
                    (awaken_file_store::content_id(&file.bytes) == id).then_some(file.bytes)
                })
                .ok_or_else(|| pc::SandboxError::new(format!("artifact `{id}` not found"))),
        }
    }

    pub(crate) async fn list_files(
        &self,
        subdir: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => Ok(sandbox.list_files(subdir)),
            Self::Namespace(sandbox) => Ok(sandbox.list_files(subdir)),
            Self::Container { sandbox, .. } => {
                let root = if subdir.starts_with('/') {
                    subdir.to_string()
                } else {
                    container_files::read_root(subdir, sandbox.outputs_path())?
                };
                sandbox.read_files(&root).await.map(|files| {
                    files
                        .into_iter()
                        .map(|file| (file.path, file.bytes))
                        .collect()
                })
            }
        }
    }

    pub(crate) fn needs_recovered_memory_reconciliation(&self) -> bool {
        matches!(self, Self::Container { sandbox, .. } if sandbox.is_recovered())
    }

    pub(crate) fn scan_skill_dir(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        match self {
            Self::Workdir(sandbox) => sandbox.scan_skill_dir(subdir),
            Self::Namespace(sandbox) => sandbox.scan_skill_dir(subdir),
            Self::Container { skills, .. } => skills.get(subdir),
        }
    }

    pub(crate) fn register_skill_dir(&self, subdir: &str) {
        if let Self::Container { skills, .. } = self {
            skills.register(subdir);
        }
    }

    pub(crate) async fn refresh_skills(&self) -> Result<(), pc::SandboxError> {
        match self {
            Self::Container {
                sandbox, skills, ..
            } => skills.refresh(sandbox.as_ref()).await,
            Self::Workdir(_) | Self::Namespace(_) => Ok(()),
        }
    }

    pub(crate) async fn materialize_inline(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Namespace(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Container { sandbox, .. } => {
                container_files::write(sandbox.as_ref(), logical, contents).await
            }
        }
    }

    pub(crate) async fn materialize_read_only_tree(
        &self,
        subdir: &str,
        files: &[(String, Vec<u8>, bool)],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_read_only_tree(subdir, files),
            Self::Namespace(sandbox) => sandbox.materialize_read_only_tree(subdir, files),
            Self::Container { sandbox, .. } => {
                container_files::materialize_read_only_tree(sandbox.as_ref(), subdir, files).await
            }
        }
    }

    /// Attach a governed mount through the backend's canonical live-injection
    /// port. Unsupported tiers fail closed instead of receiving a writable copy.
    pub(crate) async fn attach_mount(
        &self,
        requirement: pc::MountRequirement,
    ) -> Result<pc::RealizedMount, pc::SandboxError> {
        self.sandbox().attach(requirement).await
    }

    /// Rebuild the dynamic bind layout of an adopted Namespace from the frozen
    /// Session manifest. Workdir paths survive directly and container runtimes
    /// retain their own mount namespace across process ownership changes.
    pub(crate) async fn reconcile_adopted_mounts(
        &self,
        requirements: &[pc::MountRequirement],
    ) -> Result<(), pc::SandboxError> {
        if let Self::Namespace(sandbox) = self {
            for requirement in requirements {
                pc::Sandbox::attach(sandbox.as_ref(), requirement.clone()).await?;
            }
        }
        Ok(())
    }

    /// Write an ordinary runtime-owned workspace file. This is intentionally
    /// distinct from [`Self::attach_mount`], which carries access guarantees.
    pub(crate) async fn write_workspace_file(
        &self,
        logical: &str,
        contents: &[u8],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Namespace(sandbox) => sandbox.materialize_inline(logical, contents),
            Self::Container { sandbox, .. } => {
                let path = container_files::logical_path(logical)?;
                container_files::write(sandbox.as_ref(), &path, contents).await
            }
        }
    }

    /// Remove one path from the live resource projection. Every backend applies
    /// the same lexical jail and treats an absent path as an idempotent success.
    pub(crate) async fn remove_workspace_path(
        &self,
        logical: &str,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.remove_inline(logical),
            Self::Namespace(sandbox) => sandbox.remove_mount(logical),
            Self::Container { sandbox, .. } => {
                if awaken_sandbox_container::live_input_relative_path(logical).is_some() {
                    sandbox.remove_live_input_path(logical).await
                } else {
                    container_files::remove(sandbox.as_ref(), logical).await
                }
            }
        }
    }

    /// Stop only the process bindings created while constructing this wrapper.
    /// Used when an adoption races a resident environment with the same handle;
    /// disposing here would incorrectly destroy the shared underlying container.
    pub(crate) async fn stop_bound_processes(&self) {
        if let Self::Container { hand, .. } = self {
            hand.stop().await;
        }
    }

    pub(crate) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<(Box<dyn pc::ProcessHandle>, Box<dyn AgentChannelType>), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
            Self::Namespace(sandbox) => sandbox.spawn_agent(command).await,
            Self::Container { sandbox, .. } => sandbox
                .spawn_agent_process(command)
                .await
                .map(|process| (process.process, process.channel)),
        }
    }
}

struct EnvironmentToolExecutor {
    tools: std::collections::HashMap<String, Arc<dyn RawTool>>,
}

#[async_trait]
impl ToolExecutor for EnvironmentToolExecutor {
    fn recovery_capability(
        &self,
        tool_id: &str,
    ) -> awaken_runtime_contract::tool::ToolRecoveryCapability {
        self.tools.get(tool_id).map_or(
            awaken_runtime_contract::tool::ToolRecoveryCapability::NonRecoverable,
            |tool| tool.recovery_capability(),
        )
    }

    async fn invoke(
        &self,
        call: &awaken_runtime_contract::tool::ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        self.tools
            .get(&call.tool_id)
            .ok_or_else(|| awaken_runtime_contract::tool::ToolError::Unknown(call.tool_id.clone()))?
            .invoke(call.clone())
            .await
    }
}

#[async_trait]
impl pc::RepositoryRealizer for SessionEnvironment {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => {
                pc::RepositoryRealizer::realize_repository(sandbox.as_ref(), plan, credential).await
            }
            Self::Namespace(sandbox) => sandbox.provision_repo(
                &plan.mount_path,
                &plan.remote_url,
                plan.initial_branch.as_deref(),
                plan.initial_commit.as_deref(),
                credential,
            ),
            Self::Container { sandbox, .. } => {
                container_repositories::provision(
                    sandbox.as_ref(),
                    &plan.mount_path,
                    &plan.remote_url,
                    plan.initial_branch.as_deref(),
                    plan.initial_commit.as_deref(),
                    credential,
                )
                .await
            }
        }
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Result<bool, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => {
                pc::RepositoryRealizer::publish_repository(sandbox.as_ref(), plan, credential).await
            }
            Self::Namespace(sandbox) => sandbox.push_repo(&plan.mount_path, credential),
            Self::Container { sandbox, .. } => {
                container_repositories::push(
                    sandbox.as_ref(),
                    &plan.mount_path,
                    &plan.remote_url,
                    credential,
                )
                .await
            }
        }
    }
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
            Self::Container { .. } => "/workspace/.acp-config".to_string(),
            Self::Namespace(_) => "/workspace/.acp-config".to_string(),
            Self::Workdir(sandbox) => sandbox
                .workspace_path()
                .join(".acp-config")
                .to_string_lossy()
                .into_owned(),
        }
    }

    fn config_home_logical(&self) -> String {
        match self {
            Self::Container { .. } => "/workspace/.acp-config".to_string(),
            Self::Namespace(_) => "/workspace/.acp-config".to_string(),
            Self::Workdir(_) => ".acp-config".to_string(),
        }
    }

    fn workspace_cwd(&self) -> String {
        match self {
            Self::Workdir(sandbox) => sandbox.workspace_path().to_string_lossy().into_owned(),
            Self::Namespace(_) | Self::Container { .. } => "/workspace".to_string(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxSpec,
    };
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
    use tokio::io::AsyncReadExt;

    #[derive(Default)]
    struct FakeContainerProvider {
        creates: std::sync::atomic::AtomicUsize,
        renews: Arc<std::sync::atomic::AtomicUsize>,
        hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
        fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct FakeContainer {
        renews: Arc<std::sync::atomic::AtomicUsize>,
        hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
        fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct DoneProcess {
        id: String,
        code: i32,
    }

    impl DoneProcess {
        fn success(id: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                code: 0,
            }
        }

        fn exited(id: impl Into<String>, code: i32) -> Self {
            Self {
                id: id.into(),
                code,
            }
        }
    }

    struct FakeHandExecutorFactory;

    struct FakeHandExecutor;

    impl HandExecutorFactory for FakeHandExecutorFactory {
        fn bind(
            &self,
            _channel: Box<dyn AgentChannelType>,
            _operation_scope: &str,
        ) -> Arc<dyn ToolExecutor> {
            Arc::new(FakeHandExecutor)
        }
    }

    #[async_trait]
    impl ToolExecutor for FakeHandExecutor {
        async fn invoke(
            &self,
            call: &ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                call.call_id.clone(),
                "bound-hand-ok",
            ))
        }
    }

    #[async_trait]
    impl pc::ProcessHandle for DoneProcess {
        fn id(&self) -> &str {
            &self.id
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(self.code),
                signaled: false,
            })
        }

        async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
            Ok(Some(self.wait().await?))
        }

        async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironmentProvider for FakeContainerProvider {
        async fn create_environment(
            &self,
            _spec: &pc::SandboxSpec,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            self.creates
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::new(FakeContainer {
                renews: self.renews.clone(),
                hand_spawns: self.hand_spawns.clone(),
                fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
                shared: self.shared.clone(),
            }))
        }

        async fn adopt_environment(
            &self,
            _handle: &pc::SandboxHandle,
        ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError>
        {
            Ok(Arc::new(FakeContainer {
                renews: self.renews.clone(),
                hand_spawns: self.hand_spawns.clone(),
                fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
                shared: self.shared.clone(),
            }))
        }
    }

    #[async_trait]
    impl awaken_sandbox_container::ContainerEnvironment for FakeContainer {
        async fn spawn_agent_process(
            &self,
            command: pc::Command,
        ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
            let (ours, theirs) = tokio::io::duplex(64 * 1024);
            let repository_export = command.argv.iter().any(|part| part == "bundle");
            if command.argv.iter().any(|part| part == "--stdio") {
                let spawn = self
                    .hand_spawns
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                if self
                    .fail_hand_spawn_at
                    .load(std::sync::atomic::Ordering::SeqCst)
                    == spawn
                {
                    return Err(pc::SandboxError::new("scripted Hand spawn failure"));
                }
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut sink = Vec::new();
                    let _ = theirs.read_to_end(&mut sink).await;
                });
            } else if command.argv.iter().any(|part| part.contains("cat")) {
                let bytes = self
                    .shared
                    .lock()
                    .unwrap()
                    .get("marker")
                    .cloned()
                    .unwrap_or_default();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mut theirs = theirs;
                    let _ = theirs.write_all(&bytes).await;
                });
            } else if repository_export {
                drop(theirs);
            } else {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut bytes = Vec::new();
                    let _ = theirs.read_to_end(&mut bytes).await;
                });
            }
            let exit_code = if repository_export
                && self
                    .shared
                    .lock()
                    .unwrap()
                    .contains_key("__fail_repository_export")
            {
                19
            } else {
                0
            };
            Ok(awaken_sandbox_container::RuntimeAgentProcess {
                process: Box::new(DoneProcess::exited("container-exec", exit_code)),
                channel: Box::new(ours),
            })
        }

        async fn read_files(
            &self,
            _root: &str,
        ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
            Ok(self
                .shared
                .lock()
                .unwrap()
                .iter()
                .map(|(path, bytes)| awaken_sandbox_container::EnvironmentFile {
                    path: path.clone(),
                    bytes: bytes.clone(),
                })
                .collect())
        }
    }

    #[async_trait]
    impl pc::Sandbox for FakeContainer {
        fn id(&self) -> &str {
            "session-container"
        }

        fn handle(&self) -> pc::SandboxHandle {
            pc::SandboxHandle::new("container", self.id())
        }

        async fn spawn(
            &self,
            command: pc::Command,
        ) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
            if command.argv.iter().any(|part| part.contains("marker")) {
                self.shared
                    .lock()
                    .unwrap()
                    .insert("marker".into(), b"shared-container-state".to_vec());
            }
            let repository_import = command.argv.iter().any(|part| part == "awaken-repo-import");
            let exit_code = if repository_import
                && self
                    .shared
                    .lock()
                    .unwrap()
                    .contains_key("__fail_repository_import")
            {
                23
            } else {
                0
            };
            Ok(Box::new(DoneProcess::exited("native-exec", exit_code)))
        }

        async fn attach(
            &self,
            _requirement: pc::MountRequirement,
        ) -> Result<pc::RealizedMount, pc::SandboxError> {
            Err(pc::SandboxError::new("unsupported"))
        }

        async fn artifacts(&self) -> Result<Vec<pc::Artifact>, pc::SandboxError> {
            Ok(Vec::new())
        }

        async fn read_artifact(&self, _id: &str) -> Result<Vec<u8>, pc::SandboxError> {
            Err(pc::SandboxError::new("missing"))
        }

        fn realized(&self) -> &[pc::RealizedMount] {
            &[]
        }

        async fn process(&self, id: &str) -> Result<Box<dyn pc::ProcessHandle>, pc::SandboxError> {
            Ok(Box::new(DoneProcess::success(id)))
        }

        async fn status(&self) -> Result<pc::SandboxStatus, pc::SandboxError> {
            Ok(pc::SandboxStatus::Ready)
        }

        async fn renew_lease(&self) -> Result<(), pc::SandboxError> {
            self.renews
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }

        async fn dispose(&self) -> Result<(), pc::SandboxError> {
            Ok(())
        }
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "session-env".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[tokio::test]
    async fn native_process_and_agent_channel_share_one_live_environment() {
        // Cause/effect graph: C1=Workdir environment; C2=Native Hand tool;
        // C3=ACP child process. Effects: E1/E2 both observe state written inside
        // the same Session owner. Decision rule W1=C1+C2+C3 -> one marker value;
        // a host-global or provider-selected Hand would fail E1.
        let base = tempfile::tempdir().unwrap();
        let local = LocalProvider::new(base.path())
            .create_sandbox(&spec())
            .await
            .unwrap();
        let original = Sandbox::handle(&local);
        let environment = SessionEnvironment::workdir(local);
        assert_eq!(environment.handle(), original);

        let native = environment
            .sandbox()
            .spawn(pc::Command::new([
                "/bin/sh",
                "-c",
                "printf shared-state > marker",
            ]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

        let hand_output = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "workdir-hand".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "marker"}),
            })
            .await
            .unwrap();
        assert!(
            hand_output.text().contains("shared-state"),
            "W1 Native Hand"
        );

        let (agent, mut channel) = environment
            .spawn_agent(pc::Command::new(["/bin/sh", "-c", "cat marker"]))
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "shared-state");

        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn namespace_native_process_and_agent_channel_share_one_live_environment() {
        // Decision rule N1 mirrors W1 at the Namespace tier: opaque ACP paths and
        // cooperative Native Hand calls must converge on one transparent workspace.
        let base = tempfile::tempdir().unwrap();
        let mut namespace_spec = spec();
        namespace_spec.scope = "session-namespace".into();
        namespace_spec.isolation = IsolationClass::Namespace;
        let namespace = NamespaceProvider::new(base.path())
            .create_sandbox(&namespace_spec)
            .await
            .unwrap();
        let environment = SessionEnvironment::namespace(namespace);
        #[cfg(target_os = "macos")]
        assert_eq!(environment.handle().provider_kind, "seatbelt");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(environment.handle().provider_kind, "bwrap");

        let mut native_command =
            pc::Command::new(["/bin/sh", "-c", "printf namespace-state > marker"]);
        native_command.cwd = "/workspace".into();
        let native = environment.sandbox().spawn(native_command).await.unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

        let hand_output = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "namespace-hand".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({"path": "marker"}),
            })
            .await
            .unwrap();
        assert!(hand_output.text().contains("namespace-state"), "N1");

        let mut agent_command = pc::Command::new(["/bin/sh", "-c", "cat marker"]);
        agent_command.cwd = "/workspace".into();
        let (agent, mut channel) = environment.spawn_agent(agent_command).await.unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "namespace-state");

        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn container_native_tools_and_acp_share_one_environment_and_bound_hand() {
        let provider = Arc::new(FakeContainerProvider::default());
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        assert_eq!(
            provider.creates.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(environment.handle().provider_kind, "container");
        assert!(AgentSandbox::is_container(&environment));
        // Config-home placement decision table:
        // C1=environment already exists; C2=root may be read-only; C3=workspace
        // is the provider's writable Session boundary. C1+C2+C3 requires both
        // the exposed and materialization paths to stay under /workspace.
        assert_eq!(
            AgentSandbox::config_home(&environment),
            "/workspace/.acp-config"
        );
        assert_eq!(
            AgentSandbox::config_home_logical(&environment),
            "/workspace/.acp-config"
        );

        AgentSandbox::materialize_inline(&environment, "/workspace/direct.bin", b"direct")
            .await
            .unwrap();
        environment
            .write_workspace_file("projected.bin", b"projected")
            .await
            .unwrap();
        environment
            .materialize_read_only_tree(
                "generated-skills",
                &[("skill/SKILL.md".into(), b"generated".to_vec(), false)],
            )
            .await
            .unwrap();
        environment
            .remove_workspace_path("projected.bin")
            .await
            .unwrap();

        let native = environment
            .sandbox()
            .spawn(pc::Command::new(["sh", "-c", "write marker"]))
            .await
            .unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));
        let (agent, mut channel) = environment
            .spawn_agent(pc::Command::new(["sh", "-c", "cat marker"]))
            .await
            .unwrap();
        let mut output = String::new();
        channel.read_to_string(&mut output).await.unwrap();
        assert_eq!(agent.wait().await.unwrap().code, Some(0));
        assert_eq!(output, "shared-container-state");

        let hand = environment.tool_executor();
        environment.register_skill_dir("skills");
        provider.shared.lock().unwrap().insert(
            "authored/SKILL.md".into(),
            b"---\ndescription: authored\n---\nbody".to_vec(),
        );
        let result = hand
            .invoke(&ToolCall {
                call_id: "bound-hand".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({"command": "printf bound-hand-ok"}),
            })
            .await
            .unwrap();
        assert!(result.text().contains("bound-hand-ok"));
        let skills = environment.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "authored");
        assert_eq!(skills[0].dir, "skills/authored");
        environment.refresh_skills().await.unwrap();
        environment.stop_bound_processes().await;
        environment.dispose().await.unwrap();
    }

    #[derive(Clone, Copy)]
    enum ScriptedHandOutcome {
        Success,
        UnavailableBeforeDispatch,
        Indeterminate,
    }

    struct ScriptedHandExecutor {
        outcome: ScriptedHandOutcome,
    }

    #[async_trait]
    impl ToolExecutor for ScriptedHandExecutor {
        async fn invoke(
            &self,
            call: &ToolCall,
        ) -> Result<
            awaken_runtime_contract::tool::ToolOutput,
            awaken_runtime_contract::tool::ToolError,
        > {
            match self.outcome {
                ScriptedHandOutcome::Success => Ok(awaken_runtime_contract::tool::ToolOutput::ok(
                    &call.call_id,
                    "recovered",
                )),
                ScriptedHandOutcome::UnavailableBeforeDispatch => Err(
                    awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(
                        "expired attached exec".into(),
                    ),
                ),
                ScriptedHandOutcome::Indeterminate => {
                    Err(awaken_runtime_contract::tool::ToolError::Execution(
                        "indeterminate: hand connection lost during dispatch".into(),
                    ))
                }
            }
        }
    }

    struct ScriptedHandFactory {
        outcomes: std::sync::Mutex<std::collections::VecDeque<ScriptedHandOutcome>>,
        binds: std::sync::atomic::AtomicUsize,
    }

    impl ScriptedHandFactory {
        fn new(outcomes: impl IntoIterator<Item = ScriptedHandOutcome>) -> Arc<Self> {
            Arc::new(Self {
                outcomes: std::sync::Mutex::new(outcomes.into_iter().collect()),
                binds: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }

    impl HandExecutorFactory for ScriptedHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn AgentChannelType>,
            _operation_scope: &str,
        ) -> Arc<dyn ToolExecutor> {
            self.binds.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Arc::new(ScriptedHandExecutor {
                outcome: self
                    .outcomes
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or(ScriptedHandOutcome::Success),
            })
        }
    }

    #[tokio::test]
    async fn container_hand_reacquires_only_for_a_proven_pre_dispatch_failure() {
        /*
         * Container-Hand recovery cause/effect decision table.
         * Causes: C1 first binding succeeds or is unavailable before dispatch;
         * C2 one or two calls arrive; C3 replacement succeeds or is also
         * unavailable; C4 failure occurs after dispatch (indeterminate); C5 the
         * replacement process cannot start; C6 the owner is already closed.
         * Effects: E1 use the resident Hand without spawning; E2 stop the expired
         * binding, spawn exactly one replacement, and safely retry; E3 serialize
         * concurrent recovery behind that one replacement; E4 return after one
         * bounded retry; E5 never replay an indeterminate call; E6 propagate a
         * replacement-start failure without a loop; E7 never restart after close.
         * Rules: H1 success=>E1; H2 unavailable+C2+C3(success)=>E2+E3;
         * H3 unavailable+C3(unavailable)=>E4; H4 C4=>E5; H5 C5=>E6;
         * H6 C6=>E7.
         */
        let provider = Arc::new(FakeContainerProvider::default());
        let stable = ScriptedHandFactory::new([ScriptedHandOutcome::Success]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            stable.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();
        assert_eq!(
            hand.invoke(&ToolCall {
                call_id: "stable".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .unwrap()
            .text(),
            "recovered"
        );
        assert_eq!(stable.binds.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        environment.stop_bound_processes().await;
        assert!(matches!(
            hand.invoke(&ToolCall {
                call_id: "closed".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ));
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a closed owner never launches another Hand"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let recover = ScriptedHandFactory::new([
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::Success,
        ]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            recover.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let hand = environment.tool_executor();
        let left = ToolCall {
            call_id: "left".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        };
        let right = ToolCall {
            call_id: "right".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        };
        let (left, right) = tokio::join!(hand.invoke(&left), hand.invoke(&right));
        assert_eq!(left.unwrap().text(), "recovered");
        assert_eq!(right.unwrap().text(), "recovered");
        assert_eq!(recover.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "concurrent callers share one replacement"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        provider
            .fail_hand_spawn_at
            .store(2, std::sync::atomic::Ordering::SeqCst);
        let failed_replacement =
            ScriptedHandFactory::new([ScriptedHandOutcome::UnavailableBeforeDispatch]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            failed_replacement.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "replacement-spawn-failure".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a replacement spawn failure is propagated");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(ref message)
                if message.contains("failed to reacquire Session hand")
        ));
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one initial spawn plus one failed replacement attempt"
        );
        assert_eq!(
            failed_replacement
                .binds
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a failed process spawn never creates an executor"
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let bounded = ScriptedHandFactory::new([
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::UnavailableBeforeDispatch,
            ScriptedHandOutcome::Success,
        ]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            bounded.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "bounded".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a second dead channel ends the bounded retry");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_)
        ));
        assert_eq!(bounded.binds.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        environment.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let indeterminate = ScriptedHandFactory::new([ScriptedHandOutcome::Indeterminate]);
        let environment = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            indeterminate.clone(),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap();
        let error = environment
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "indeterminate".into(),
                tool_id: "write".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect_err("a possibly executed call is never replayed");
        assert!(matches!(
            error,
            awaken_runtime_contract::tool::ToolError::Execution(_)
        ));
        assert_eq!(
            indeterminate
                .binds
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            provider
                .hand_spawns
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn adopting_a_container_renews_its_ownership_before_use() {
        let provider = Arc::new(FakeContainerProvider::default());
        let environments = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        );
        let adopted = environments
            .adopt(&pc::SandboxHandle::new("container", "session-container"))
            .await
            .expect("adopt environment");
        assert_eq!(
            provider.renews.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "adoption refreshes ownership before starting the replacement hand"
        );
        adopted.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn container_repository_transfer_reports_import_and_export_failures() {
        let source_root = tempfile::tempdir().expect("source root");
        let source = source_root.path().join("source");
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .status()
                .expect("run git fixture command");
            assert!(status.success(), "git fixture command failed: {args:?}");
        };
        git(&["init", "-q", source.to_str().unwrap()]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "config",
            "user.name",
            "fixture",
        ]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        std::fs::write(source.join("README.md"), "fixture").unwrap();
        git(&["-C", source.to_str().unwrap(), "add", "README.md"]);
        git(&[
            "-C",
            source.to_str().unwrap(),
            "commit",
            "-q",
            "-m",
            "fixture",
        ]);

        let shared = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let container = FakeContainer {
            renews: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            hand_spawns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_hand_spawn_at: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            shared: shared.clone(),
        };
        shared
            .lock()
            .unwrap()
            .insert("__fail_repository_import".into(), Vec::new());
        let import_error = container_repositories::provision(
            &container,
            "repo",
            source.to_str().unwrap(),
            None,
            None,
            None,
        )
        .await
        .expect_err("a failed container import is not reported as provisioned");
        assert!(
            import_error
                .to_string()
                .contains("container repository import exited Some(23)")
        );

        {
            let mut state = shared.lock().unwrap();
            state.remove("__fail_repository_import");
            state.insert("__fail_repository_export".into(), Vec::new());
        }
        let export_error =
            container_repositories::push(&container, "repo", source.to_str().unwrap(), None)
                .await
                .expect_err("a failed container export is not pushed");
        assert!(
            export_error
                .to_string()
                .contains("container repository export exited Some(19)")
        );
    }

    #[tokio::test]
    async fn provider_rebasing_adoption_and_container_mount_jail_cover_every_tier() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();

        let workdir = SessionEnvironmentProvider::workdir(first.path()).at_root(second.path());
        assert!(matches!(workdir, SessionEnvironmentProvider::Workdir(_)));

        let namespace = SessionEnvironmentProvider::namespace_with_agent_stderr(first.path(), true)
            .at_root(second.path());
        assert!(matches!(
            &namespace,
            SessionEnvironmentProvider::Namespace(provider)
                if provider.inherits_agent_stderr()
        ));
        let mut namespace_spec = spec();
        namespace_spec.scope = "provider-namespace-adopt".into();
        let created = namespace.create(&namespace_spec).await.unwrap();
        let handle = created.handle();
        drop(created);
        let adopted = namespace.adopt(&handle).await.unwrap();
        assert_eq!(adopted.handle(), handle);
        adopted.dispose().await.unwrap();

        let provider = Arc::new(FakeContainerProvider::default());
        let unsafe_mount = pc::MountRequirement {
            mount_id: "unsafe".into(),
            source: pc::MountSource::Inline {
                contents: "value".into(),
            },
            mount_path: "../escape".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        };
        let container = SessionEnvironmentProvider::container(
            provider,
            vec![unsafe_mount],
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .at_root(second.path());
        assert!(matches!(
            container,
            SessionEnvironmentProvider::Container { .. }
        ));
        assert!(container.create(&spec()).await.is_err());
    }

    #[cfg(feature = "container-docker")]
    #[tokio::test]
    async fn docker_environment_transfers_repo_and_harvests_files_without_exposing_token() {
        let Ok(image) = std::env::var("AWAKEN_TEST_SESSION_IMAGE") else {
            eprintln!("skipping: AWAKEN_TEST_SESSION_IMAGE is not set");
            return;
        };
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        let seed = temp.path().join("seed");
        let git = |cwd: &std::path::Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(cwd)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(
            temp.path(),
            &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
        );
        git(&seed, &["config", "user.name", "seed"]);
        git(&seed, &["config", "user.email", "seed@example.invalid"]);
        std::fs::write(seed.join("README.md"), "base").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-m", "base"]);
        git(&seed, &["push", "-u", "origin", "HEAD"]);

        let runtime =
            Arc::new(awaken_sandbox_container::docker::DockerRuntime::connect_local(8080).unwrap());
        let provider = Arc::new(awaken_sandbox_container::ContainerProvider::new(
            runtime, image,
        ));
        let mut docker_spec = spec();
        docker_spec.scope = format!("host-repo-real-{}", std::process::id());
        docker_spec.outputs_path = "/mnt/session/outputs".into();
        let environment = SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&docker_spec)
        .await
        .unwrap();
        let repository_plan = pc::RepositoryRealizationPlan {
            repository_id: "repo".into(),
            mount_path: "workspace/repo".into(),
            remote_url: remote.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        };
        pc::RepositoryRealizer::realize_repository(&environment, &repository_plan, None)
            .await
            .unwrap();
        environment
            .materialize_inline("/workspace/.mnt/live.txt", b"live")
            .await
            .unwrap();
        assert_eq!(
            environment.list_files(".mnt").await.unwrap(),
            vec![("live.txt".into(), b"live".to_vec())]
        );
        environment
            .remove_workspace_path(".mnt/live.txt")
            .await
            .unwrap();
        assert!(environment.list_files(".mnt").await.unwrap().is_empty());

        let change = environment
            .sandbox()
            .spawn(pc::Command::new([
                "sh",
                "-c",
                concat!(
                    "git -C /workspace/repo config user.name agent && ",
                    "git -C /workspace/repo config user.email agent@example.invalid && ",
                    "printf changed > /workspace/repo/README.md && ",
                    "git -C /workspace/repo add README.md && ",
                    "git -C /workspace/repo commit -m changed && ",
                    "mkdir -p /workspace/outputs/nested && ",
                    "printf '\\000\\377' > /workspace/outputs/nested/result.bin && ",
                    "mkdir -p /workspace/skills/authored && ",
                    "printf '%s' '---\ndescription: authored\n---\nbody' > ",
                    "/workspace/skills/authored/SKILL.md"
                ),
            ]))
            .await
            .unwrap();
        assert_eq!(change.wait().await.unwrap().code, Some(0));
        environment.register_skill_dir("skills");
        environment.refresh_skills().await.unwrap();
        let skills = environment.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "authored");
        assert!(
            pc::RepositoryRealizer::publish_repository(&environment, &repository_plan, None)
                .await
                .unwrap()
        );
        assert!(
            !pc::RepositoryRealizer::publish_repository(&environment, &repository_plan, None)
                .await
                .unwrap()
        );
        // Artifact cause/effect rule: one regular output file → one canonical
        // content-addressed Artifact and binary-safe read through the same port.
        let artifacts = environment.artifacts().await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert!(artifacts[0].path.ends_with("nested/result.bin"));
        assert_eq!(artifacts[0].id, artifacts[0].content_hash);
        assert_eq!(
            environment.read_artifact(&artifacts[0].id).await.unwrap(),
            vec![0, 0xff]
        );
        environment.dispose().await.unwrap();

        let count = std::process::Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-list",
                "--count",
                "--all",
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
    }
}
