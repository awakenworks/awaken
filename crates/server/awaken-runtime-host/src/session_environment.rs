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

    fn config_home(&self) -> &'static str;

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
        hand_process: Arc<dyn pc::ProcessHandle>,
        hand: Arc<dyn ToolExecutor>,
        skills: Arc<ContainerSkillCache>,
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
            Self::Container {
                sandbox,
                hand_process,
                ..
            } => {
                let _ = hand_process.signal(pc::Signal::Term).await;
                let _ = hand_process.wait().await;
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
        hand_factory: &dyn HandExecutorFactory,
    ) -> Result<Self, pc::SandboxError> {
        let hand_bin = std::env::var("AWAKEN_CONTAINER_HAND_BIN")
            .unwrap_or_else(|_| "/usr/local/bin/awaken-sandbox".to_string());
        let process = sandbox
            .spawn_agent_process(pc::Command {
                argv: vec![hand_bin, "hand".into(), "--stdio".into()],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await?;
        let skills = Arc::new(ContainerSkillCache::default());
        let hand: Arc<dyn ToolExecutor> = Arc::new(RefreshingHandExecutor {
            inner: hand_factory.bind(process.channel, sandbox.id()),
            sandbox: sandbox.clone(),
            skills: skills.clone(),
        });
        Ok(Self::Container {
            sandbox,
            hand_process: Arc::from(process.process),
            hand,
            skills,
        })
    }

    pub(crate) fn bound_tool_executor(&self) -> Option<Arc<dyn ToolExecutor>> {
        match self {
            Self::Container { hand, .. } => Some(hand.clone()),
            Self::Workdir(_) | Self::Namespace(_) => None,
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

    pub(crate) async fn list_files(
        &self,
        subdir: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => Ok(sandbox.list_files(subdir)),
            Self::Namespace(sandbox) => Ok(sandbox.list_files(subdir)),
            Self::Container { sandbox, .. } => {
                let root = container_files::read_root(subdir, sandbox.outputs_path())?;
                sandbox.read_files(&root).await.map(|files| {
                    files
                        .into_iter()
                        .map(|file| (file.path, file.bytes))
                        .collect()
                })
            }
        }
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
        files: &[(String, Vec<u8>)],
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => sandbox.materialize_read_only_tree(subdir, files),
            Self::Namespace(sandbox) => sandbox.materialize_read_only_tree(subdir, files),
            Self::Container { sandbox, .. } => {
                container_files::materialize_read_only_tree(sandbox.as_ref(), subdir, files).await
            }
        }
    }

    /// Project a resolved immutable file into the already-live Session workspace.
    pub(crate) async fn materialize_workspace_file(
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
            Self::Namespace(sandbox) => sandbox.remove_inline(logical),
            Self::Container { sandbox, .. } => {
                container_files::remove(sandbox.as_ref(), logical).await
            }
        }
    }

    /// Stop only the process bindings created while constructing this wrapper.
    /// Used when an adoption races a resident environment with the same handle;
    /// disposing here would incorrectly destroy the shared underlying container.
    pub(crate) async fn stop_bound_processes(&self) {
        if let Self::Container { hand_process, .. } = self {
            let _ = hand_process.signal(pc::Signal::Term).await;
            let _ = hand_process.wait().await;
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

#[async_trait]
impl pc::RepositoryRealizer for SessionEnvironment {
    async fn realize_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&str>,
    ) -> Result<(), pc::SandboxError> {
        match self {
            Self::Workdir(sandbox) => {
                pc::RepositoryRealizer::realize_repository(sandbox.as_ref(), plan, credential).await
            }
            Self::Namespace(sandbox) => sandbox.provision_repo(
                &plan.mount_path,
                &plan.remote_url,
                plan.initial_branch.as_deref(),
                credential,
            ),
            Self::Container { sandbox, .. } => {
                container_repositories::provision(
                    sandbox.as_ref(),
                    &plan.mount_path,
                    &plan.remote_url,
                    plan.initial_branch.as_deref(),
                    credential,
                )
                .await
            }
        }
    }

    async fn publish_repository(
        &self,
        plan: &pc::RepositoryRealizationPlan,
        credential: Option<&str>,
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

    fn config_home(&self) -> &'static str {
        match self {
            Self::Container { .. } => "/acp-config",
            Self::Workdir(_) | Self::Namespace(_) => ".acp-config",
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
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct FakeContainer {
        renews: Arc<std::sync::atomic::AtomicUsize>,
        shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    }

    struct DoneProcess(String);

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
            &self.0
        }

        async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
            Ok(pc::ExitStatus {
                code: Some(0),
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
            if command.argv.iter().any(|part| part == "--stdio") {
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
            } else {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut theirs = theirs;
                    let mut bytes = Vec::new();
                    let _ = theirs.read_to_end(&mut bytes).await;
                });
            }
            Ok(awaken_sandbox_container::RuntimeAgentProcess {
                process: Box::new(DoneProcess("container-exec".into())),
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
            Ok(Box::new(DoneProcess("native-exec".into())))
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
            Ok(Box::new(DoneProcess(id.into())))
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
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            limits: ResourceLimits::default(),
            lease_ttl_secs: None,
            extra: None,
        }
    }

    #[tokio::test]
    async fn native_process_and_agent_channel_share_one_live_environment() {
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
        let base = tempfile::tempdir().unwrap();
        let mut namespace_spec = spec();
        namespace_spec.scope = "session-namespace".into();
        namespace_spec.isolation = IsolationClass::Namespace;
        let namespace = NamespaceProvider::new(base.path())
            .create_sandbox(&namespace_spec)
            .await
            .unwrap();
        let environment = SessionEnvironment::namespace(namespace);
        assert_eq!(environment.handle().provider_kind, "bwrap");

        let mut native_command =
            pc::Command::new(["/bin/sh", "-c", "printf namespace-state > marker"]);
        native_command.cwd = "/workspace".into();
        let native = environment.sandbox().spawn(native_command).await.unwrap();
        assert_eq!(native.wait().await.unwrap().code, Some(0));

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
        assert_eq!(AgentSandbox::config_home(&environment), "/acp-config");

        AgentSandbox::materialize_inline(&environment, "/workspace/direct.bin", b"direct")
            .await
            .unwrap();
        environment
            .materialize_workspace_file("projected.bin", b"projected")
            .await
            .unwrap();
        environment
            .materialize_read_only_tree(
                "generated-skills",
                &[("skill/SKILL.md".into(), b"generated".to_vec())],
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

        let hand = environment.bound_tool_executor().expect("container hand");
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
        assert!(result.content.contains("bound-hand-ok"));
        let skills = environment.scan_skill_dir("skills");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].id, "authored");
        assert_eq!(skills[0].dir, "skills/authored");
        environment.refresh_skills().await.unwrap();
        environment.stop_bound_processes().await;
        environment.dispose().await.unwrap();
    }

    #[tokio::test]
    async fn adopting_a_container_renews_its_ownership_before_use() {
        let provider = Arc::new(FakeContainerProvider::default());
        let environments = SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
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
        )
        .create(&docker_spec)
        .await
        .unwrap();
        let repository_plan = pc::RepositoryRealizationPlan {
            repository_id: "repo".into(),
            mount_path: "workspace/repo".into(),
            remote_url: remote.to_string_lossy().into_owned(),
            initial_branch: None,
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
        assert_eq!(
            environment.list_files("outputs").await.unwrap(),
            vec![("nested/result.bin".into(), vec![0, 0xff])]
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
