use super::*;
use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::{
    IsolationClass, NetworkPolicy, ResourceLimits, Sandbox, SandboxProvider, SandboxSpec,
};
use awaken_run_executor_acp::AgentChannelType;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::ToolExecutor;
use awaken_sandbox_container::{
    PublishedSandboxControlService, SandboxControlPublishError, SandboxControlService,
    SandboxControlServiceKind, SandboxControlServicePublisher,
};
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::AsyncReadExt;

#[derive(Default)]
struct FakeContainerProvider {
    creates: std::sync::atomic::AtomicUsize,
    disposals: Arc<std::sync::atomic::AtomicUsize>,
    specs: std::sync::Mutex<Vec<pc::SandboxSpec>>,
    renews: Arc<std::sync::atomic::AtomicUsize>,
    hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

struct FakeContainer {
    handle: pc::SandboxHandle,
    disposals: Arc<std::sync::atomic::AtomicUsize>,
    renews: Arc<std::sync::atomic::AtomicUsize>,
    hand_spawns: Arc<std::sync::atomic::AtomicUsize>,
    resident_channel_opens: Arc<std::sync::atomic::AtomicUsize>,
    fail_hand_spawn_at: Arc<std::sync::atomic::AtomicUsize>,
    shared: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

struct DoneProcess {
    id: String,
    code: i32,
}

struct UnreapableProcess;

struct SlowReapProcess;

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
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        Arc::new(FakeHandExecutor)
    }
}

#[async_trait]
impl ToolExecutor for FakeHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
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
impl pc::ProcessHandle for UnreapableProcess {
    fn id(&self) -> &str {
        "unreapable-hand"
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        std::future::pending().await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(None)
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new("scripted signal failure"))
    }
}

#[async_trait]
impl pc::ProcessHandle for SlowReapProcess {
    fn id(&self) -> &str {
        "slow-reap-hand"
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        std::future::pending().await
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(None)
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
}

#[async_trait]
impl awaken_sandbox_container::ContainerEnvironmentProvider for FakeContainerProvider {
    async fn probe_ready(&self) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn create_environment(
        &self,
        spec: &pc::SandboxSpec,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        self.creates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.specs.lock().unwrap().push(spec.clone());
        Ok(Arc::new(FakeContainer {
            handle: current_fake_container_handle(spec),
            disposals: self.disposals.clone(),
            renews: self.renews.clone(),
            hand_spawns: self.hand_spawns.clone(),
            resident_channel_opens: self.resident_channel_opens.clone(),
            fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
            shared: self.shared.clone(),
        }))
    }

    async fn adopt_environment(
        &self,
        adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
    ) -> Result<Arc<dyn awaken_sandbox_container::ContainerEnvironment>, pc::SandboxError> {
        Ok(Arc::new(FakeContainer {
            handle: adoption.handle.clone(),
            disposals: self.disposals.clone(),
            renews: self.renews.clone(),
            hand_spawns: self.hand_spawns.clone(),
            resident_channel_opens: self.resident_channel_opens.clone(),
            fail_hand_spawn_at: self.fail_hand_spawn_at.clone(),
            shared: self.shared.clone(),
        }))
    }
}

#[async_trait]
impl awaken_sandbox_container::ContainerEnvironment for FakeContainer {
    fn record_owned_path(&self, path: &str) -> Result<(), pc::SandboxError> {
        self.shared
            .lock()
            .unwrap()
            .entry(format!("__owned_path:{path}"))
            .or_default();
        Ok(())
    }

    async fn open_agent_channel(&self) -> Result<Box<dyn AgentChannelType>, pc::SandboxError> {
        self.resident_channel_opens
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut sink = Vec::new();
            let _ = theirs.read_to_end(&mut sink).await;
        });
        Ok(Box::new(ours))
    }

    async fn spawn_agent_process(
        &self,
        command: pc::Command,
    ) -> Result<awaken_sandbox_container::RuntimeAgentProcess, pc::SandboxError> {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let repository_export = command
            .argv
            .iter()
            .any(|part| part == "bundle" || part.contains("bundle create"));
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
        let unreapable = command.argv.iter().any(|part| part == "--stdio")
            && self
                .shared
                .lock()
                .unwrap()
                .contains_key("__unreapable_hand");
        let slow_reap = command.argv.iter().any(|part| part == "--stdio")
            && self.shared.lock().unwrap().contains_key("__slow_reap_hand");
        let process: Box<dyn pc::ProcessHandle> = if unreapable {
            Box::new(UnreapableProcess)
        } else if slow_reap {
            Box::new(SlowReapProcess)
        } else {
            Box::new(DoneProcess::exited("container-exec", exit_code))
        };
        Ok(awaken_sandbox_container::RuntimeAgentProcess {
            process,
            channel: Box::new(ours),
        })
    }

    async fn read_files(
        &self,
        _root: &str,
    ) -> Result<Vec<awaken_sandbox_container::EnvironmentFile>, pc::SandboxError> {
        if self
            .shared
            .lock()
            .unwrap()
            .contains_key("__fail_skill_read")
        {
            return Err(pc::SandboxError::new("scripted Skill read outage"));
        }
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
impl SandboxControlServicePublisher for FakeContainer {
    async fn publish_sandbox_control_service(
        &self,
        _kind: SandboxControlServiceKind,
        _service: Arc<dyn SandboxControlService>,
    ) -> Result<Box<dyn PublishedSandboxControlService>, SandboxControlPublishError> {
        // This fixture declares no control-service topology, so publication must
        // fail closed instead of manufacturing a successful provider lease.
        Err(SandboxControlPublishError)
    }
}

#[async_trait]
impl pc::Sandbox for FakeContainer {
    fn id(&self) -> &str {
        &self.handle.sandbox_id
    }

    fn handle(&self) -> pc::SandboxHandle {
        self.handle.clone()
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
        self.disposals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        requests: Default::default(),
        limits: ResourceLimits::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        control_services: Default::default(),
        lease_ttl_secs: None,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn current_fake_container_handle(spec: &pc::SandboxSpec) -> pc::SandboxHandle {
    let realization_fingerprint = pc::SandboxRealizationFingerprint::from_spec(spec);
    let owned_paths = spec
        .mounts
        .iter()
        .map(|mount| mount.mount_path.clone())
        .collect::<Vec<_>>();
    pc::SandboxHandle::container_v2(
        "session-container",
        pc::ContainerSandboxHandleV2 {
            previous: pc::ContainerSandboxHandleV1 {
                container_id: "container-session-container".into(),
                outputs_path: spec.outputs_path.clone(),
                base_env: spec.env.clone(),
                live_input_projection: false,
                continuation_excluded_paths: owned_paths.clone(),
                runtime_handle: None,
                sandbox_control_incarnation: None,
                control_services: spec.control_services.clone(),
            },
            adoption_fingerprint: realization_fingerprint.clone(),
            realization_fingerprint,
            owned_paths,
        },
    )
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
    // Namespace Workspace-path cause/effect graph: C1 the admitted provider
    // offers tool transparency and path fidelity; C2 a sandbox process writes
    // through the canonical absolute `/workspace`; C3 the typed file surface
    // reads/writes that Session-owned tree; C4 an ACP process reads the same
    // canonical absolute paths. Effects: E1 the typed read sees C2's bytes and
    // E2 the ACP process sees both C2 and C3 without an alias or path rewrite.
    //
    // | Rule | provider admitted | process write | typed file I/O | ACP read | Effect |
    // |---|---|---|---|---|---|
    // | N1 | yes | yes | read process bytes | no | E1 |
    // | N2 | yes | yes | write typed bytes | yes | E1+E2 |
    // | N0 | no path fidelity | any | any | any | fail before environment creation (provisioning decision table) |
    //
    // Constraint: WorkspaceLayout plus Sandbox capability admission remain the
    // sole path authority. This test must not add a host-path alias, symlink, or
    // shell-command classifier to make split paths appear consistent.
    let base = tempfile::tempdir().unwrap();
    let mut namespace_spec = spec();
    namespace_spec.scope = "session-namespace".into();
    namespace_spec.isolation = IsolationClass::Namespace;
    let provider = NamespaceProvider::new(base.path());
    if let Err(probe_error) = provider.probe_ready().await {
        let rejection = crate::sandbox_source::resolve_sandbox_tier(
            crate::deployment_config::SandboxTier::Namespace,
            false,
            base.path(),
        )
        .await
        .expect_err("an unavailable namespace must not silently degrade");
        assert!(rejection.contains("OS-native sandbox unavailable"));
        assert!(rejection.contains(&probe_error.to_string()));
        return;
    }
    let capabilities = provider.capabilities();
    if !capabilities.path_fidelity {
        let requirements = pc::SandboxRequirements::from_spec(&namespace_spec, true);
        assert!(requirements.path_fidelity, "N0 path requirement");
        assert!(
            !capabilities.satisfies_requirements(&requirements),
            "N0 a split-path Namespace must fail closed before environment creation"
        );
        return;
    }
    let namespace = provider.create_sandbox(&namespace_spec).await.unwrap();
    let environment = SessionEnvironment::namespace(
        namespace,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    );
    #[cfg(target_os = "macos")]
    assert_eq!(environment.handle().provider_kind(), "seatbelt");
    #[cfg(not(target_os = "macos"))]
    assert_eq!(environment.handle().provider_kind(), "bwrap");

    let mut native_command = pc::Command::new([
        "/bin/sh",
        "-c",
        "printf namespace-state > /workspace/process-marker",
    ]);
    native_command.cwd = pc::WorkspaceLayout::ROOT.into();
    let native = environment.sandbox().spawn(native_command).await.unwrap();
    assert_eq!(native.wait().await.unwrap().code, Some(0));

    let typed_files = environment.list_workspace_files("").await.unwrap();
    assert!(
        typed_files.iter().any(|(path, bytes)| {
            path.ends_with("process-marker") && bytes.as_slice() == b"namespace-state"
        }),
        "N1/E1 typed file reads the process-authored /workspace bytes: {typed_files:?}"
    );
    environment
        .write_workspace_file("typed-marker", b"typed-state")
        .await
        .unwrap();

    let hand_output = environment
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "namespace-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({"path": "marker"}),
        })
        .await
        .unwrap();
    assert_eq!(hand_output.text(), "bound-hand-ok", "N1 Hand binding");

    let mut agent_command = pc::Command::new([
        "/bin/sh",
        "-c",
        "cat /workspace/process-marker; printf '|'; cat /workspace/typed-marker",
    ]);
    agent_command.cwd = pc::WorkspaceLayout::ROOT.into();
    let (agent, mut channel) = environment.spawn_agent(agent_command).await.unwrap();
    let mut output = String::new();
    channel.read_to_string(&mut output).await.unwrap();
    assert_eq!(agent.wait().await.unwrap().code, Some(0));
    assert_eq!(output, "namespace-state|typed-state", "N2/E2");

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
    assert_eq!(environment.handle().provider_kind(), "container");
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
        .remove_projection_path("projected.bin")
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
    let skills = environment.scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    assert_eq!(skills[0].dir, "skills/authored");
    environment.refresh_skills().await.unwrap();
    environment.stop_bound_processes().await.unwrap();
    environment.dispose().await.unwrap();
}

#[tokio::test]
#[should_panic(
    expected = "KNOWN GAP: a container Skill read failure must not become an empty registry"
)]
async fn managed_skill_refresh_propagates_container_read_failure() {
    /* Temporary expected-failure regression; remove `should_panic` when the
     * production fix lands.
     *
     * Cause/effect graph: C1 a Managed repository Skill root is admitted;
     * C2 the container read backing the registered-root refresh succeeds or
     * fails. Effects: E1 success may build the one repository registry (the
     * neighboring container test owns that row); E2 failure aborts registry
     * construction with the original diagnostic; E3 no empty catalog/prompt
     * is published as if the root legitimately contained no Skills.
     * Decision table: SR1 C1+read-ok => E1; SR2 C1+read-error => E2+E3.
     * This case owns SR2 through the existing `SessionEnvironment` refresh and
     * `build_skill_registry(Result)` boundaries; it adds no fake registry or
     * alternate discovery path. */
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__fail_skill_read".into(), Vec::new());
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    let roots = vec!["workspace/repository/.claude/skills".to_owned()];

    let error = match crate::skills::build_skill_registry(
        &[],
        Vec::new(),
        None,
        Some(environment.clone()),
        crate::skills::MANAGED_SKILLS_SUBDIR,
        Some(&roots),
        true,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => {
            panic!("KNOWN GAP: a container Skill read failure must not become an empty registry")
        }
    };
    assert!(error.contains("scripted Skill read outage"), "SR2: {error}");
    environment.dispose().await.unwrap();
}

#[tokio::test]
#[should_panic(
    expected = "KNOWN GAP: invalid UTF-8 from container Skill refresh must not become an empty registry"
)]
async fn managed_skill_refresh_rejects_invalid_container_skill_bytes() {
    /* Temporary expected-failure regression; remove `should_panic` when the
     * production fix lands.
     *
     * Cause/effect graph: C1 the container refresh returns an exact
     * `<id>/SKILL.md`; C2 its bytes are valid or invalid UTF-8. Effects: E1
     * valid bytes enter the one repository snapshot; E2 invalid bytes surface
     * a construction error; E3 invalid bytes never disappear into a healthy
     * empty registry. Decision table: SU1 C1+UTF8 => E1 (owned by
     * `container_native_tools_and_acp_share_one_environment_and_bound_hand`);
     * SU2 C1+!UTF8 => E2+E3. */
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("broken/SKILL.md".into(), vec![0xff, 0xfe]);
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider,
            Vec::new(),
            Arc::new(FakeHandExecutorFactory),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    let roots = vec!["workspace/repository/.claude/skills".to_owned()];

    let error = match crate::skills::build_skill_registry(
        &[],
        Vec::new(),
        None,
        Some(environment.clone()),
        crate::skills::MANAGED_SKILLS_SUBDIR,
        Some(&roots),
        true,
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!(
            "KNOWN GAP: invalid UTF-8 from container Skill refresh must not become an empty registry"
        ),
    };
    assert!(error.contains("UTF-8"), "SU2: {error}");
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn prepared_image_is_the_only_environment_to_container_image_conversion() {
    // FMECA: F1 every authored Environment is implicitly converted to an
    // image (S6/O5/D4, RPN120) -> self-hosted/package-free paths become
    // dependent on a builder; F2 a ready immutable image is omitted from the
    // provider request (S9/O3/D5, RPN135) -> packages are resolved again at
    // runtime; F3 both prepared image and mutable packages reach the provider
    // (S8/O3/D4, RPN96) -> two realization tracks can diverge. Mitigation is
    // the canonical Snapshot projection: `prepared_image=Some` selects one
    // Image environment and clears packages; `None` retains package inputs.
    //
    // Cause/effect decision table:
    // | Rule | prepared image | packages | final provider SandboxSpec |
    // | I1 | none | non-empty | no image; exact packages retained |
    // | I2 | ready digest | non-empty | Image(digest); packages empty |
    // This test crosses the final `SessionEnvironmentProvider::create`
    // boundary, so it verifies the actual container adapter input rather
    // than only an intermediate projection helper.
    fn snapshot(prepared_image: Option<String>) -> awaken_session_contract::EnvironmentSnapshot {
        awaken_session_contract::EnvironmentSnapshot {
            environment_id: "image-flow".into(),
            revision: awaken_session_contract::EnvironmentRevision(7),
            self_hosted: false,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint(
                "image-flow-v7".into(),
            ),
            sandbox: Default::default(),
            sandbox_provisioning: Default::default(),
            idle_retention: Default::default(),
            packages: awaken_session_contract::EnvironmentPackages {
                npm: vec!["tsx@4".into()],
                ..Default::default()
            },
            prepared_image,
            network: awaken_session_contract::SessionNetworkPolicy::None,
            credential_realization:
                awaken_runtime_contract::CredentialRealizationProfile::self_hosted_native(),
        }
    }

    let provider = Arc::new(FakeContainerProvider::default());
    let environments = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        Arc::new(FakeHandExecutorFactory),
        "/usr/local/bin/awaken-sandbox",
    );
    let unprepared =
        crate::provisioning::environment_capacity_projection(&snapshot(None), true).spec;
    let environment = environments.create(&unprepared).await.unwrap();
    environment.dispose().await.unwrap();

    let digest = "registry.example/awaken@sha256:0123456789abcdef";
    let prepared =
        crate::provisioning::environment_capacity_projection(&snapshot(Some(digest.into())), true)
            .spec;
    let environment = environments.create(&prepared).await.unwrap();
    environment.dispose().await.unwrap();

    let specs = provider.specs.lock().unwrap();
    assert_eq!(specs.len(), 2, "I1/I2 provider boundary");
    assert_eq!(
        specs[0].packages.managers.get("npm"),
        Some(&vec!["tsx@4".to_string()]),
        "I1"
    );
    assert!(specs[0].environment.is_none(), "I1");
    assert!(specs[1].packages.managers.is_empty(), "I2/F3");
    assert_eq!(
        specs[1].environment,
        Some(awaken_provisioning_contract::EnvironmentKind::Image {
            reference: digest.into()
        }),
        "I2"
    );
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
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
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

struct BlockingHandFactory {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    block_once: Arc<std::sync::atomic::AtomicBool>,
}

struct BlockingHandExecutor {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    block_once: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl ToolExecutor for BlockingHandExecutor {
    async fn invoke(
        &self,
        call: &ToolCall,
    ) -> Result<awaken_runtime_contract::tool::ToolOutput, awaken_runtime_contract::tool::ToolError>
    {
        if self.block_once.swap(false, Ordering::SeqCst) {
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(awaken_runtime_contract::tool::ToolOutput::ok(
            &call.call_id,
            "completed",
        ))
    }
}

impl HandExecutorFactory for BlockingHandFactory {
    fn bind(
        &self,
        _channel: Box<dyn AgentChannelType>,
        _operation_scope: &str,
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
    ) -> Arc<dyn ToolExecutor> {
        Arc::new(BlockingHandExecutor {
            started: self.started.clone(),
            release: self.release.clone(),
            block_once: self.block_once.clone(),
        })
    }
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
        _recovery: awaken_runtime_contract::tool::ToolRecoveryCapability,
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
async fn resident_hand_survives_worker_detach_and_is_reopened_on_adoption() {
    /*
     * Resident-Hand HA cause/effect graph and decision table.
     * Causes: C1 residency=resident; C2 Session Pod exists; C3 original
     * Worker attachment closes; C4 replacement Worker adopts the handle;
     * C5 Pod/Hand itself fails. Constraints: C4 requires C2; C5 excludes
     * healthy adoption. Effects: E1 no attached `hand --stdio` child; E2
     * lazily open one provider-owned Pod channel per Worker binding on first
     * tool demand; E3 advertise DurableRequest recovery before that bind; E4
     * detaching does not signal the resident process; E5 Pod/Hand failure is
     * not masked as Worker recovery.
     * Rules: RH1 C1+C2=>E1+E3; RH2 RH1+tool demand=>E2;
     * RH3 C1+C2+C3+C4+tool demand=>E2+E4;
     * RH4 C5=>E5 (covered by channel/open failure tests).
     * FMECA: Worker crash loses only the ephemeral channel (severity 2,
     * detectable by channel failure); re-adoption/open-channel is the
     * mitigation. A Pod/Hand crash remains severity 4 and requires workload
     * recovery, not a duplicate Worker-side Hand.
     */
    let provider = Arc::new(FakeContainerProvider::default());
    let factory =
        ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
    let environments = SessionEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
        provider.clone(),
        None,
        Vec::new(),
        factory,
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::ZERO,
        crate::deployment_config::ContainerHandResidency::Resident,
    );

    let original = environments.create(&spec()).await.unwrap();
    let handle = original.handle();
    assert_eq!(
        original.tool_executor().recovery_capability("write"),
        awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest
    );
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 0);
    original
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "original-resident-bind".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 1);

    original.stop_bound_processes().await.unwrap();
    let adopted = environments.adopt(&spec(), &handle).await.unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 0);
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 1);
    assert_eq!(provider.renews.load(Ordering::SeqCst), 1);
    assert_eq!(
        adopted.tool_executor().recovery_capability("bash"),
        awaken_runtime_contract::tool::ToolRecoveryCapability::DurableRequest
    );
    adopted
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "adopted-resident-bind".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(provider.resident_channel_opens.load(Ordering::SeqCst), 2);
    adopted.dispose().await.unwrap();
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
     * replacement-start failure without a loop; E7 never restart after close;
     * E8 idle hibernation and its stale-timer races are covered separately.
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
    tokio::task::yield_now().await;
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
    environment.stop_bound_processes().await.unwrap();
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
async fn live_projection_update_hibernates_and_fences_the_session_hand() {
    /*
     * Projection/Hand cause-effect graph and decision table.
     * Causes: C1=a bound Hand exists; C2=projection update begins; C3=the
     * update commits; C4=the update guard drops without commit; C5=a tool
     * arrives during/after the update. Effects: E1=wait for the in-flight
     * binding and reap it once; E2=reject C5 before dispatch while fenced;
     * E3=after C3 lazily launch exactly one Hand over the new projection;
     * E4=after C4 remain fenced until the authoritative retry commits.
     * Rules: U1 C1+C2=>E1+E2; U2 U1+C3+C5=>E3;
     * U3 U1+C4+C5=>E2+E4.
     */
    let provider = Arc::new(FakeContainerProvider::default());
    let factory = ScriptedHandFactory::new([
        ScriptedHandOutcome::Success,
        ScriptedHandOutcome::Success,
        ScriptedHandOutcome::Success,
    ]);
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        factory,
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.tool_executor();
    hand.invoke(&ToolCall {
        call_id: "before-projection-update".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "U1/C1");

    let committed = environment
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("container Hand update");
    assert!(
        matches!(
            hand.invoke(&ToolCall {
                call_id: "during-update".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "U1/E2"
    );
    committed.commit();
    hand.invoke(&ToolCall {
        call_id: "after-commit".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U2/E3");

    let uncommitted = environment
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("container Hand update");
    drop(uncommitted);
    assert!(
        matches!(
            hand.invoke(&ToolCall {
                call_id: "after-failed-update".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
            Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
        ),
        "U3/E2,E4"
    );
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "U3/E4");

    environment
        .begin_live_projection_update()
        .await
        .unwrap()
        .expect("retry update")
        .commit();
    hand.invoke(&ToolCall {
        call_id: "after-retry".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 3, "U3 retry");
    environment.dispose().await.unwrap();
}

#[tokio::test]
async fn live_projection_update_rejects_an_unreapable_hand() {
    // Projection-reap cause/effect rule. C1=a lazily bound Hand exists;
    // C2=its provider cannot prove reap; C3=projection update begins;
    // C4=the owning Environment is terminally disposed. E1=reject the update;
    // E2=fence later dispatch; E3=retain one process owner and never launch a
    // replacement; E4=still invoke the one physical Environment disposal
    // boundary exactly once. Rules UR1=C1+C2+C3=>E1+E2+E3;
    // UR2=UR1+C4=>E4 because physical disposal, unlike Hand replacement,
    // terminates the complete process substrate.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.tool_executor();
    hand.invoke(&ToolCall {
        call_id: "bind-unreapable-hand".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();

    let error = match environment.begin_live_projection_update().await {
        Err(error) => error,
        Ok(_) => panic!("an unknown old-process outcome must reject the projection update"),
    };
    assert!(
        error
            .to_string()
            .contains("failed to reap Session hand before projection update")
    );
    assert!(matches!(
        hand.invoke(&ToolCall {
            call_id: "after-projection-reap-failure".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await,
        Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
    ));
    assert_eq!(
        provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "a failed projection reap never permits a replacement Hand"
    );
    environment.dispose().await.unwrap();
    assert_eq!(provider.disposals.load(Ordering::SeqCst), 1, "UR2/E4");
}

#[tokio::test]
async fn cancelled_projection_reap_retains_the_tracked_hand_owner() {
    // Cancelled-reap cause/effect rule. C1=a lazily bound Hand exists;
    // C2=projection retirement is in flight; C3=the update Future is
    // cancelled before wait proves exit. E1=retain the binding as the sole
    // process owner; E2=keep dispatch fenced; E3=do not spawn a replacement.
    // Rule CR1=C1+C2+C3=>E1+E2+E3.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__slow_reap_hand".into(), Vec::new());
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    environment
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-slow-reap-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    let updating = tokio::spawn({
        let environment = environment.clone();
        async move {
            let update = environment
                .begin_live_projection_update()
                .await?
                .expect("Container projection has a Hand fence");
            update.commit();
            Ok::<_, pc::SandboxError>(())
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    updating.abort();
    let _ = updating.await;

    let SessionEnvironment::Container { hand, .. } = environment.as_ref() else {
        panic!("test uses a Container environment")
    };
    assert!(
        hand.has_tracked_binding().await,
        "cancellation must retain the only known process owner"
    );
    assert!(hand.projection_is_updating());
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn checkpoint_quiescence_fails_closed_when_attached_hand_cannot_be_reaped() {
    // Cause/effect graph: C2=attached Hand is tracked; C3=Supervisor cannot
    // prove reap. C2+C3 -> E1=no quiescence proof, E2=Ready owner retained,
    // E3=closed lifecycle prevents replacement. Decision rule QH1 covers
    // the unreapable terminal branch; the successful attached branch is
    // covered by the ordinary environment lifecycle tests above.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
    let environment = SessionEnvironmentProvider::container(
        provider.clone(),
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    environment
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-unreapable-checkpoint-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();

    assert!(environment.quiesce().await.is_err(), "QH1/E1");
    let SessionEnvironment::Container { hand, .. } = &environment else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QH1/E2");
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "QH1/E3");
}

#[tokio::test]
async fn checkpoint_quiescence_rejects_a_retained_resident_hand() {
    // Cause/effect graph: C2=resident Hand belongs to the Session Pod;
    // C3=checkpoint-and-release requests process quiescence; C4=the same
    // durable operation retries; C5=terminal disposal follows. C2+C3 ->
    // E1=first attempt has no proof while the resident binding and sandbox
    // remain live; C2+C3+C4 -> E2=retry remains Retained, never Vacant;
    // C5 -> E3=terminal stop may drop the channel before sandbox disposal.
    // Rules QR1/QR2/QR3 distinguish retry-stable checkpoint safety from the
    // existing terminal owner.
    let provider = Arc::new(FakeContainerProvider::default());
    let environment = SessionEnvironmentProvider::container_with_capacity_hand_idle_and_residency(
        provider.clone(),
        None,
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::ZERO,
        crate::deployment_config::ContainerHandResidency::Resident,
    )
    .create(&spec())
    .await
    .unwrap();

    assert!(environment.quiesce().await.is_err(), "QR1/E1");
    let SessionEnvironment::Container { hand, .. } = &environment else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QR1/E1");
    assert_eq!(
        environment.status().await.unwrap(),
        pc::SandboxStatus::Ready,
        "QR1/E1"
    );
    assert!(environment.quiesce().await.is_err(), "QR2/E2");
    assert!(hand.has_tracked_binding().await, "QR2/E2");
    assert_eq!(
        provider.resident_channel_opens.load(Ordering::SeqCst),
        1,
        "QR2/E2"
    );
    environment.dispose().await.unwrap();
    assert!(!hand.has_tracked_binding().await, "QR3/E3");
}

#[tokio::test]
async fn cancelled_checkpoint_quiescence_retains_the_attached_hand_owner() {
    // Cause/effect graph: C2=attached process; C6=quiescence Future is
    // cancelled during bounded reap. C2+C6 -> E1=no receipt and E2=the same
    // binding remains tracked for retry. Rule QC1 proves cancellation cannot
    // manufacture Vacant or admit a replacement.
    let provider = Arc::new(FakeContainerProvider::default());
    provider
        .shared
        .lock()
        .unwrap()
        .insert("__slow_reap_hand".into(), Vec::new());
    let environment = Arc::new(
        SessionEnvironmentProvider::container(
            provider.clone(),
            Vec::new(),
            ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
            "/usr/local/bin/awaken-sandbox",
        )
        .create(&spec())
        .await
        .unwrap(),
    );
    environment
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-slow-checkpoint-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    let quiescing = tokio::spawn({
        let environment = environment.clone();
        async move { environment.quiesce().await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    quiescing.abort();
    let _ = quiescing.await;

    let SessionEnvironment::Container { hand, .. } = environment.as_ref() else {
        panic!("test uses a Container environment")
    };
    assert!(hand.has_tracked_binding().await, "QC1/E2");
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "QC1/E2");
}

/// Worker-local Hand inactivity cause/effect decision table.
/// C1=idle policy enabled; C2=deadline reached; C3=a newer invocation touches
/// the generation; C4=policy is zero; C5=invocation follows hibernation.
/// E1=keep one binding before the deadline; E2=stale deadline cannot stop a
/// newer generation; E3=deadline releases the Hand; E4=next call lazily
/// creates exactly one replacement; E5=zero disables hibernation; C6=the
/// provider cannot reap the expired process; E6=close the owner and never
/// launch a possibly concurrent replacement; C7=the Environment is terminally
/// disposed after C6; E7=invoke its one physical disposal boundary once. Rules:
/// I1 C1+!C2=>E1; I2 C1+C2+C3=>E2; I3 C1+C2+!C3=>E3;
/// I4 I3+C5=>E4; I5 C4=>E5; I6 C6=>E6; I7=I6+C7=>E7. This executes in the Runtime Host
/// without a Coordinator or durable Session scan, covering split deployment
/// ownership.
#[tokio::test(start_paused = true)]
async fn container_hand_hibernates_on_worker_local_inactivity_and_reacquires_once() {
    let provider = Arc::new(FakeContainerProvider::default());
    let factory =
        ScriptedHandFactory::new([ScriptedHandOutcome::Success, ScriptedHandOutcome::Success]);
    let environment = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
        provider.clone(),
        None,
        Vec::new(),
        factory.clone(),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .create(&spec())
    .await
    .unwrap();
    let hand = environment.tool_executor();
    hand.invoke(&ToolCall {
        call_id: "start-idle-window".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();

    tokio::time::advance(std::time::Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I1");

    hand.invoke(&ToolCall {
        call_id: "refresh-deadline".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 1, "I2");

    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    hand.invoke(&ToolCall {
        call_id: "after-idle".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(provider.hand_spawns.load(Ordering::SeqCst), 2, "I3+I4");
    assert_eq!(factory.binds.load(Ordering::SeqCst), 2, "I4");
    environment.dispose().await.unwrap();

    let disabled_provider = Arc::new(FakeContainerProvider::default());
    let disabled = SessionEnvironmentProvider::container(
        disabled_provider.clone(),
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
    )
    .create(&spec())
    .await
    .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(3_600)).await;
    tokio::task::yield_now().await;
    disabled
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "disabled".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    assert_eq!(
        disabled_provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "I5"
    );
    disabled.dispose().await.unwrap();

    let failed_provider = Arc::new(FakeContainerProvider::default());
    failed_provider
        .shared
        .lock()
        .unwrap()
        .insert("__unreapable_hand".into(), Vec::new());
    let failed = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
        failed_provider.clone(),
        None,
        Vec::new(),
        ScriptedHandFactory::new([ScriptedHandOutcome::Success]),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .create(&spec())
    .await
    .unwrap();
    failed
        .tool_executor()
        .invoke(&ToolCall {
            call_id: "bind-unreapable-idle-hand".into(),
            tool_id: "read".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        failed
            .tool_executor()
            .invoke(&ToolCall {
                call_id: "after-reap-failure".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await,
        Err(awaken_runtime_contract::tool::ToolError::UnavailableBeforeDispatch(_))
    ));
    assert_eq!(failed_provider.hand_spawns.load(Ordering::SeqCst), 1, "I6");
    failed.dispose().await.unwrap();
    assert_eq!(failed_provider.disposals.load(Ordering::SeqCst), 1, "I7/E7");
}

/// Idle/invoke race rule I7: C1=the old deadline fires while a Hand call owns
/// the lifecycle mutex; C2=the call completes and advances its generation;
/// E1=the waiting timer observes the new generation and cannot reap the live
/// binding; E2=the next call reuses that same Hand without a second spawn.
#[tokio::test(start_paused = true)]
async fn an_idle_deadline_cannot_reap_a_concurrent_hand_invocation() {
    let provider = Arc::new(FakeContainerProvider::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let environment = SessionEnvironmentProvider::container_with_capacity_and_hand_idle(
        provider.clone(),
        None,
        Vec::new(),
        Arc::new(BlockingHandFactory {
            started: started.clone(),
            release: release.clone(),
            block_once: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }),
        "/usr/local/bin/awaken-sandbox",
        std::time::Duration::from_secs(60),
    )
    .create(&spec())
    .await
    .unwrap();
    tokio::task::yield_now().await;
    let hand = environment.tool_executor();
    let running_hand = hand.clone();
    let running = tokio::spawn(async move {
        running_hand
            .invoke(&ToolCall {
                call_id: "running-at-deadline".into(),
                tool_id: "read".into(),
                arguments: serde_json::json!({}),
            })
            .await
    });
    started.notified().await;
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    release.notify_one();
    assert_eq!(running.await.unwrap().unwrap().text(), "completed");
    tokio::task::yield_now().await;

    hand.invoke(&ToolCall {
        call_id: "reuse-after-race".into(),
        tool_id: "read".into(),
        arguments: serde_json::json!({}),
    })
    .await
    .unwrap();
    assert_eq!(
        provider.hand_spawns.load(Ordering::SeqCst),
        1,
        "I7: a stale timer never forces replacement"
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
    let spec = spec();
    let effective_spec = environments.effective_spec(&spec).unwrap();
    let handle = current_fake_container_handle(&effective_spec);
    let adopted = environments
        .adopt(&spec, &handle)
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
        handle: current_fake_container_handle(&spec()),
        disposals: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        renews: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        hand_spawns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        resident_channel_opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_hand_spawn_at: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        shared: shared.clone(),
    };

    // Container Repository path rule: C1 a plan carries `/repo`; E1 reject
    // before host bundle acquisition/container exec, E2 never reinterpret it
    // as `/workspace/repo`.
    let unsupported_plan = pc::RepositoryRealizationPlan {
        repository_id: "unsupported".into(),
        mount_path: "/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let unsupported = container_repositories::provision(
        &container,
        "/opt/custom/awaken-sandbox",
        &unsupported_plan,
        None,
    )
    .await
    .expect_err("C1/E1 unsupported path");
    assert!(
        unsupported
            .to_string()
            .contains("current providers require")
    );
    assert!(shared.lock().unwrap().is_empty(), "E1 no container effect");

    shared
        .lock()
        .unwrap()
        .insert("__fail_repository_import".into(), Vec::new());
    let import_plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let import_error = container_repositories::provision(
        &container,
        "/opt/custom/awaken-sandbox",
        &import_plan,
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
    let plan = pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: source.to_string_lossy().into_owned(),
        transport_url: source.to_string_lossy().into_owned(),
        initial_branch: None,
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    };
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "main".into(),
        commit: "0000000000000000000000000000000000000000".into(),
        expected_prior_commit: None,
    };
    let export_error = container_repositories::push(&container, &plan, &expectation, None)
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
    // Provider-projection cause/effect rules. C1=a provider is rebased;
    // C2=a current fenced Namespace handle is adopted; C3=a container mount
    // escapes its jail; C4=aggregate disposal preparation is not yet/is durably
    // accepted; C5=the closed observation carries the exact/foreign physical
    // incarnation. E1=preserve the provider kind/configuration; E2=preparation
    // quiesces and binds the exact owner but does not remove it; E3=physical
    // disposal removes only the exact evidenced root after C4; E4=reject C3
    // before provider create; E5=accept only exact terminal evidence while
    // retaining the filesystem-absence rule. Rules: P1=C1=>E1;
    // P2=C1+C2+!C4+exact-C5=>E1+E2+E5; P3=C1+C2+C4=>E3;
    // P4=C1+C3=>E4; P5=C1+C2+foreign-C5=>reject before disposal.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();

    let workdir = SessionEnvironmentProvider::workdir(first.path()).at_root(second.path());
    assert!(matches!(workdir, SessionEnvironmentProvider::Workdir(_)));

    let namespace = SessionEnvironmentProvider::namespace_with_agent_stderr(
        first.path(),
        true,
        Arc::new(FakeHandExecutorFactory),
        "/bin/sh",
        std::time::Duration::ZERO,
    )
    .at_root(second.path());
    assert!(matches!(
        &namespace,
        SessionEnvironmentProvider::Namespace { provider, .. }
            if provider.inherits_agent_stderr()
    ));
    let mut namespace_spec = spec();
    namespace_spec.scope = "provider-namespace-adopt".into();
    let namespace_spec = namespace.effective_spec(&namespace_spec).unwrap();
    let create_fence =
        pc::SandboxEffectFence::new("create", "provider-test", "runtime", 1, u64::MAX).unwrap();
    let created = namespace
        .create_effective_for_effect(
            &namespace_spec,
            Some(&create_fence),
            None,
            awaken_sandbox_container::ContainerRealizationIntent::Create,
        )
        .await
        .unwrap();
    let handle = created.handle();
    drop(created);
    let adopted = namespace
        .adopt_effective_for_effect(&namespace_spec, &handle, Some(&create_fence))
        .await
        .unwrap();
    assert_eq!(adopted.handle(), handle);
    let terminal_fence =
        pc::SandboxEffectFence::new("terminal", "provider-test", "runtime", 1, u64::MAX).unwrap();
    adopted
        .prepare_disposal_for_effect(&terminal_fence)
        .await
        .unwrap();
    let terminal_observation = namespace
        .observe_effective_for_effect(&namespace_spec, &handle, &terminal_fence)
        .await
        .unwrap();
    assert!(
        matches!(
            terminal_observation,
            pc::SandboxObservation::Terminal { .. }
        ),
        "P2 preparation retains the exact physical root without forging aggregate disposal"
    );
    namespace
        .validate_closed_observation(&handle, &terminal_observation)
        .expect("P2/P5 exact marker incarnation is the sole closed evidence");
    assert!(
        namespace
            .validate_closed_observation(
                &handle,
                &pc::SandboxObservation::Terminal {
                    physical_incarnation: "foreign-physical-incarnation".into(),
                },
            )
            .is_err(),
        "P5 foreign terminal evidence fails closed",
    );
    assert!(
        namespace
            .validate_closed_observation(
                &handle,
                &pc::SandboxObservation::DefinitivelyUnavailable {
                    physical_incarnation: handle
                        .filesystem_physical_incarnation()
                        .unwrap()
                        .map(str::to_owned),
                },
            )
            .is_err(),
        "P5 filesystem absence cannot reuse a once-live incarnation",
    );
    let preparation_fingerprint = "provider-test-preparation";
    let disposal_preparation =
        pc::SandboxDisposalPreparation::new(terminal_fence.clone(), preparation_fingerprint)
            .unwrap();
    let disposal_id = disposal_preparation.operation_id().unwrap();
    let disposal_fence =
        pc::SandboxEffectFence::new(disposal_id, "provider-test", "runtime", 1, u64::MAX).unwrap();
    let authorization = disposal_preparation.authorize(disposal_fence).unwrap();
    adopted.dispose_for_effect(&authorization).await.unwrap();

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
// Container Repository transfer rules: C1 a non-default Agent work branch
// has one committed change -> publish that exact branch without changing
// the base branch; C2 replay the unchanged container state -> no second
// push; C3 outputs and authored Skills remain independently harvestable.
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
        mount_path: "/workspace/repo".into(),
        source_remote_url: remote.to_string_lossy().into_owned(),
        transport_url: remote.to_string_lossy().into_owned(),
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
        environment.list_workspace_files(".mnt").await.unwrap(),
        vec![("live.txt".into(), b"live".to_vec())]
    );
    environment
        .remove_projection_path(".mnt/live.txt")
        .await
        .unwrap();
    assert!(
        environment
            .list_workspace_files(".mnt")
            .await
            .unwrap()
            .is_empty()
    );

    let change = environment
        .sandbox()
        .spawn(pc::Command::new([
            "sh",
            "-c",
            concat!(
                "git -C /workspace/repo config user.name agent && ",
                "git -C /workspace/repo config user.email agent@example.invalid && ",
                "git -C /workspace/repo checkout -b awf/work && ",
                "printf changed > /workspace/repo/README.md && ",
                "git -C /workspace/repo add README.md && ",
                "git -C /workspace/repo commit -m changed && ",
                "git -C /workspace/repo rev-parse HEAD > /workspace/.mnt/commit.txt && ",
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
    let skills = environment.scan_skill_dir("skills").unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].id, "authored");
    let commit = environment
        .list_workspace_files(".mnt")
        .await
        .unwrap()
        .into_iter()
        .find_map(|(path, bytes)| (path == "commit.txt").then_some(bytes))
        .expect("container exported its exact commit coordinate");
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "awf/work".into(),
        commit: String::from_utf8(commit).unwrap().trim().into(),
        expected_prior_commit: None,
    };
    environment
        .remove_projection_path(".mnt/commit.txt")
        .await
        .unwrap();
    let first = pc::RepositoryRealizer::publish_repository(
        &environment,
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    let replay = pc::RepositoryRealizer::publish_repository(
        &environment,
        &repository_plan,
        &expectation,
        None,
    )
    .await
    .unwrap();
    assert_eq!(first, replay, "C1/C2 canonical receipt");
    // Artifact cause/effect rule: one regular output file → one canonical
    // content-addressed Artifact and binary-safe read through the same port.
    let artifacts = environment.capture_artifacts().await.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert!(artifacts[0].metadata.path.ends_with("nested/result.bin"));
    assert_eq!(artifacts[0].metadata.id, artifacts[0].metadata.content_hash);
    assert_eq!(artifacts[0].bytes, vec![0, 0xff]);
    environment.dispose().await.unwrap();

    git(
        temp.path(),
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "show-ref",
            "--verify",
            "refs/heads/awf/work",
        ],
    );
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
