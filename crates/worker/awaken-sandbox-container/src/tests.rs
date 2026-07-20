//! Slice 5 coverage: pure planners (container/pod) and the provider lifecycle over
//! an in-memory fake [`ContainerRuntime`] — process-as-container, no daemon required.

use super::*;
use awaken_agent_channel::AgentTransport;
use awaken_provisioning_contract::SandboxProvider;
use std::collections::HashMap;
use std::sync::Mutex;

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![
            pc::MountRequirement {
                mount_id: "in".into(),
                source: pc::MountSource::File {
                    file_id: "file-1".into(),
                    content_hash: None,
                },
                mount_path: "/data/in.txt".into(),
                access: pc::MountAccess::ReadOnly,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            },
            pc::MountRequirement {
                mount_id: "work".into(),
                source: pc::MountSource::Resource {
                    resource_id: "res-9".into(),
                    content_hash: None,
                },
                mount_path: "/work".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::Session,
                required: true,
            },
        ],
        env: vec![
            pc::EnvVar {
                name: "TZ".into(),
                value: pc::EnvValue::Inline {
                    value: "UTC".into(),
                },
                visibility: pc::EnvVisibility::Process,
            },
            pc::EnvVar {
                name: "API_KEY".into(),
                value: pc::EnvValue::Secret {
                    reference: "broker://k".into(),
                },
                visibility: pc::EnvVisibility::Process,
            },
        ],
        network: pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        },
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(1 << 30),
            pids: Some(256),
            disk_bytes: None,
        },
        lease_ttl_secs: Some(60),
        // Process-as-container: the agent is the container's main command.
        extra: Some(serde_json::json!({ "command": ["claude", "--acp"] })),
    }
}

// ── Pure planners ─────────────────────────────────────────────────────────────

#[test]
fn container_capabilities_are_the_strongest_tier() {
    let c = container_capabilities();
    assert_eq!(c.isolation, pc::IsolationClass::Container);
    assert!(c.tool_transparent && c.enforced_readonly && c.network_isolation);
    assert!(c.resource_limits && c.custom_rootfs);
}

#[test]
fn command_of_reads_the_agent_argv_or_defaults_empty() {
    assert_eq!(
        command_of(&spec("s")),
        vec!["claude".to_string(), "--acp".to_string()]
    );
    let mut bare = spec("s");
    bare.extra = None;
    assert!(command_of(&bare).is_empty());
}

#[test]
fn container_plan_maps_command_image_env_binds_network_and_outputs() {
    let cmd = command_of(&spec("s1"));
    let plan = container_plan(&spec("s1"), "ghcr.io/awaken/sandbox:latest", &cmd);
    assert_eq!(
        plan.command,
        vec!["claude".to_string(), "--acp".to_string()]
    );
    assert_eq!(plan.image, "ghcr.io/awaken/sandbox:latest");
    // Only inline env is planned; the secret ref is resolved at the runtime edge.
    assert_eq!(plan.env, vec![("TZ".into(), "UTC".into())]);
    assert_eq!(plan.binds.len(), 2);
    assert!(plan.binds[0].read_only, "ReadOnly mount → read_only bind");
    assert!(!plan.binds[1].read_only);
    assert_eq!(plan.binds[0].source_ref, "file-1");
    assert_eq!(plan.binds[1].source_ref, "res-9");
    assert_eq!(plan.outputs_volume, "/mnt/session/outputs");
    assert_eq!(
        plan.network,
        NetworkMode::Allowlist(vec!["api.anthropic.com".into()])
    );
    assert_eq!(plan.limits.cpu_millis, Some(2000));
}

#[test]
fn container_plan_honors_an_image_override_and_network_variants() {
    let mut s = spec("s2");
    s.extra = Some(serde_json::json!({ "image": "custom:1" }));
    s.network = pc::NetworkPolicy::None;
    assert_eq!(container_plan(&s, "def", &[]).image, "custom:1");
    assert_eq!(container_plan(&s, "def", &[]).network, NetworkMode::None);

    s.network = pc::NetworkPolicy::Unrestricted;
    assert_eq!(container_plan(&s, "def", &[]).network, NetworkMode::Open);
}

#[test]
fn container_plan_resolves_rootfs_from_a_declared_environment_or_falls_back_to_image() {
    // No `environment` declared → the container runs its resolved image.
    let plan = container_plan(&spec("s"), "def:img", &["x".to_string()]);
    assert_eq!(plan.rootfs, RootfsPlan::Image("def:img".into()));

    // A declared Image environment is honored as the rootfs.
    let mut img = spec("s");
    img.extra = Some(serde_json::json!({
        "command": ["x"],
        "environment": { "kind": "image", "reference": "ghcr.io/x:2" }
    }));
    assert_eq!(
        container_plan(&img, "def:img", &["x".to_string()]).rootfs,
        RootfsPlan::Image("ghcr.io/x:2".into())
    );

    // A declared IsolatedRoot(Dir) becomes a private RootDir the podman adapter honors.
    let mut iso = spec("s");
    iso.extra = Some(serde_json::json!({
        "command": ["x"],
        "environment": {
            "kind": "isolated_root",
            "base": { "source": "dir", "path_template": "/roots/{scope}" },
            "writable_base": true
        }
    }));
    assert_eq!(
        container_plan(&iso, "def:img", &["x".to_string()]).rootfs,
        RootfsPlan::RootDir {
            path_template: "/roots/{scope}".into(),
            writable: true,
        }
    );

    // A non-container environment (Scope) has no container-tier rootfs; it is ignored
    // and falls back to the image — never silently realized as a borrowed userland.
    let mut scope = spec("s");
    scope.extra = Some(serde_json::json!({
        "command": ["x"],
        "environment": { "kind": "scope" }
    }));
    assert_eq!(
        container_plan(&scope, "def:img", &["x".to_string()]).rootfs,
        RootfsPlan::Image("def:img".into())
    );
}

#[test]
fn mount_ref_covers_every_source_kind() {
    let s = |src| BindPlan {
        source_ref: mount_ref(&src),
        mount_path: String::new(),
        read_only: false,
        content: None,
        content_bytes: None,
        secret_content: None,
        secret_writeback: false,
        credential_file_path: None,
    };
    assert_eq!(
        s(pc::MountSource::MemoryStore {
            store_id: "m".into()
        })
        .source_ref,
        "m"
    );
    assert_eq!(
        s(pc::MountSource::Secret {
            reference: "r".into(),
            content_hash: None
        })
        .source_ref,
        "r"
    );
    assert_eq!(
        s(pc::MountSource::Other(serde_json::json!({}))).source_ref,
        ""
    );
}

#[test]
fn memory_store_mounts_are_pulled_out_of_binds_into_memory_mounts() {
    let mut s = spec("mem");
    s.mounts.push(pc::MountRequirement {
        mount_id: "notes".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-42".into(),
        },
        mount_path: "/workspace/.mnt/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let plan = container_plan(&s, "img", &["x".to_string()]);
    // The memory store is NOT a byte bind — binds stay the 2 file/resource mounts.
    assert_eq!(plan.binds.len(), 2);
    assert!(plan.binds.iter().all(|b| b.source_ref != "store-42"));
    // It is realized as a memory mount (→ sidecar downstream).
    assert_eq!(plan.memory_mounts.len(), 1);
    assert_eq!(plan.memory_mounts[0].store_id, "store-42");
    assert_eq!(plan.memory_mounts[0].mount_path, "/workspace/.mnt/notes");
}

#[tokio::test]
async fn memory_store_realizes_as_copy_on_the_container_tier() {
    let rt = Arc::new(FakeRuntime::default());
    let mut s = spec("mem-real");
    s.mounts.push(pc::MountRequirement {
        mount_id: "notes".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-7".into(),
        },
        mount_path: "/workspace/.mnt/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let sandbox = provider(rt).create(&s).await.unwrap();
    let mem = sandbox
        .realized()
        .iter()
        .find(|r| r.mount_id == "notes")
        .expect("memory mount realized");
    // No-FUSE portable default: the container tier reports Copy (the memoryd sidecar
    // materializes + harvests), not a live Fuse mount.
    assert_eq!(mem.realization, pc::Realization::Copy);
}

#[test]
fn pod_plan_is_process_as_container_with_native_gc() {
    let cmd = pc::Command::new(["claude", "--acp"]);
    let plan = pod_plan(&spec("run-7"), &cmd, "img:1", "owner-uid-123");
    assert_eq!(plan.name, "awaken-run-7");
    // The agent argv IS the container command (not exec-into-idle).
    assert_eq!(
        plan.command,
        vec!["claude".to_string(), "--acp".to_string()]
    );
    assert_eq!(plan.owner_uid, "owner-uid-123");
    assert!(
        plan.restart_never,
        "a finished agent pod is reaped, not looped"
    );
    assert_eq!(plan.outputs_volume, "/mnt/session/outputs");
    assert_eq!(plan.binds.len(), 2);
}

// ── Fake runtime + provider lifecycle ───────────────────────────────────────────

#[derive(Default)]
struct FakeState {
    alive: HashMap<String, bool>,
    created_command: HashMap<String, Vec<String>>,
    created_env: HashMap<String, Vec<(String, String)>>,
    exits: HashMap<String, pc::ExitStatus>,
    signals: Vec<(String, pc::Signal)>,
    lease_touches: u32,
    artifacts: Vec<pc::Artifact>,
    blobs: HashMap<String, Vec<u8>>,
    fail_create: bool,
    refreshed_credential: Option<Vec<u8>>,
    live_credential: Option<Vec<u8>>,
    credential_source: Option<std::path::PathBuf>,
}

#[derive(Default)]
struct FakeRuntime {
    st: Mutex<FakeState>,
}

impl FakeRuntime {
    fn with_artifact(self, id: &str, path: &str, bytes: &[u8]) -> Self {
        {
            let mut st = self.st.lock().unwrap();
            st.artifacts.push(pc::Artifact {
                id: id.into(),
                path: path.into(),
                size_bytes: bytes.len() as u64,
                content_hash: id.into(),
            });
            st.blobs.insert(id.into(), bytes.to_vec());
        }
        self
    }

    fn refreshing_credential(self, bytes: &[u8]) -> Self {
        self.st.lock().unwrap().refreshed_credential = Some(bytes.to_vec());
        self
    }

    fn with_live_credential(self, bytes: &[u8]) -> Self {
        self.st.lock().unwrap().live_credential = Some(bytes.to_vec());
        self
    }
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn read_live_file(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        Ok(self.st.lock().unwrap().live_credential.clone())
    }

    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let refreshed_credential = self.st.lock().unwrap().refreshed_credential.clone();
        if let Some(bytes) = refreshed_credential {
            for bind in &plan.binds {
                if !bind.read_only && bind.credential_file_path.is_some() {
                    let filename = bind
                        .credential_file_path
                        .as_deref()
                        .and_then(|path| path.rsplit('/').next())
                        .expect("credential file name");
                    let source = std::path::PathBuf::from(&bind.source_ref).join(filename);
                    self.st.lock().unwrap().credential_source = Some(source.clone());
                    std::fs::write(&source, &bytes)
                        .map_err(|e| RuntimeError::Backend(e.to_string()))?;
                }
            }
        }
        let mut st = self.st.lock().unwrap();
        if st.fail_create {
            return Err(RuntimeError::Backend("image pull failed".into()));
        }
        let cid = format!("cid-{id}");
        st.alive.insert(cid.clone(), true);
        st.created_command.insert(cid.clone(), plan.command.clone());
        st.created_env.insert(cid.clone(), plan.env.clone());
        st.exits.insert(
            cid.clone(),
            pc::ExitStatus {
                code: Some(0),
                signaled: false,
            },
        );
        Ok(cid)
    }
    async fn open_channel(
        &self,
        container_id: &str,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        if !self.st.lock().unwrap().alive.contains_key(container_id) {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        // Stand-in for a bollard attach / network dial: a usable duplex end.
        let (ours, _peer) = tokio::io::duplex(64);
        Ok(Box::new(ours))
    }
    async fn inspect(&self, container_id: &str) -> Result<ContainerState, RuntimeError> {
        match self.st.lock().unwrap().alive.get(container_id) {
            Some(true) => Ok(ContainerState::Running),
            Some(false) => Ok(ContainerState::Gone),
            None => Err(RuntimeError::NotFound(container_id.into())),
        }
    }
    async fn wait(&self, container_id: &str) -> Result<pc::ExitStatus, RuntimeError> {
        self.st
            .lock()
            .unwrap()
            .exits
            .get(container_id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound(container_id.into()))
    }
    async fn poll(&self, container_id: &str) -> Result<Option<pc::ExitStatus>, RuntimeError> {
        Ok(self.st.lock().unwrap().exits.get(container_id).cloned())
    }
    async fn signal(&self, container_id: &str, signal: pc::Signal) -> Result<(), RuntimeError> {
        self.st
            .lock()
            .unwrap()
            .signals
            .push((container_id.into(), signal));
        Ok(())
    }
    async fn artifacts(&self, _container_id: &str) -> Result<Vec<pc::Artifact>, RuntimeError> {
        Ok(self.st.lock().unwrap().artifacts.clone())
    }
    async fn read_artifact(
        &self,
        _container_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, RuntimeError> {
        self.st
            .lock()
            .unwrap()
            .blobs
            .get(artifact_id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound(artifact_id.into()))
    }
    async fn touch_lease(&self, _container_id: &str) -> Result<(), RuntimeError> {
        self.st.lock().unwrap().lease_touches += 1;
        Ok(())
    }
    async fn remove(&self, container_id: &str) -> Result<(), RuntimeError> {
        self.st
            .lock()
            .unwrap()
            .alive
            .insert(container_id.into(), false);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingSecretBroker {
    current: Mutex<Vec<u8>>,
    writes: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl pc::SecretBroker for RecordingSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        *self.current.lock().unwrap() = bytes.clone();
        self.writes.lock().unwrap().push(bytes);
        Ok(())
    }
}

fn provider(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    // The default `spec()` requests Allowlist egress, so the provider is configured
    // with a brokered proxy (as a real allowlist deployment would be), and it seeds the
    // bytes for `spec()`'s required File/Resource mounts (a required mount with no
    // resolvable bytes fails closed at create).
    ContainerProvider::new(runtime, "ghcr.io/awaken/sandbox:latest")
        .with_egress_proxy(EgressProxy {
            url: "http://gw.internal:8888".into(),
        })
        .with_blob("file-1", b"in-bytes".to_vec())
        .with_blob("res-9", b"work-bytes".to_vec())
}

#[tokio::test]
async fn full_lifecycle_create_channel_process_artifacts_lease_dispose() {
    let rt =
        Arc::new(FakeRuntime::default().with_artifact("a1", "/mnt/session/outputs/o.txt", b"hi"));
    let p = provider(rt.clone());
    assert_eq!(p.capabilities().isolation, pc::IsolationClass::Container);

    let sandbox = p.create(&spec("run-1")).await.unwrap();
    assert_eq!(sandbox.id(), "run-1");
    assert_eq!(sandbox.realized().len(), 2);
    assert_eq!(sandbox.realized()[0].realization, pc::Realization::Bind);
    // Process-as-container: create launched the agent argv as the main command.
    assert_eq!(
        rt.st
            .lock()
            .unwrap()
            .created_command
            .get("cid-run-1")
            .unwrap(),
        &vec!["claude".to_string(), "--acp".to_string()]
    );

    // spawn returns a handle to the main process; wait/poll/signal act on it.
    let proc = sandbox.spawn(pc::Command::new(["ignored"])).await.unwrap();
    assert_eq!(proc.id(), "cid-run-1");
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
    assert!(proc.poll().await.unwrap().is_some());
    proc.signal(pc::Signal::Term).await.unwrap();
    assert_eq!(rt.st.lock().unwrap().signals.len(), 1);

    // artifacts out-of-band
    assert_eq!(sandbox.artifacts().await.unwrap().len(), 1);
    assert_eq!(sandbox.read_artifact("a1").await.unwrap(), b"hi");
    assert!(sandbox.read_artifact("nope").await.is_err());

    // lease + status + dispose
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
    sandbox.renew_lease().await.unwrap();
    assert_eq!(rt.st.lock().unwrap().lease_touches, 1);
    sandbox.dispose().await.unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn a_second_node_adopts_a_running_container_over_the_shared_runtime() {
    // Cross-node recovery on the container tier: two provider objects (two workers)
    // over the SAME runtime backend — the container lives in a shared cluster/daemon
    // reachable from both. Node A realizes it; Node A vanishes; Node B adopts it from
    // the persisted handle and takes over its process, artifacts, and lease.
    let rt =
        Arc::new(FakeRuntime::default().with_artifact("a1", "/mnt/session/outputs/o.txt", b"work"));
    let node_a = provider(rt.clone());
    let sandbox_a = node_a.create(&spec("run-x")).await.unwrap();
    let wire = serde_json::to_string(&sandbox_a.handle()).unwrap();
    drop(sandbox_a);
    drop(node_a);

    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    assert_eq!(recovered.provider_kind, "container");
    let node_b = provider(rt.clone());
    let sandbox_b = node_b
        .adopt(&recovered)
        .await
        .expect("a second node adopts the container from its handle");

    assert_eq!(sandbox_b.id(), "run-x");
    assert!(matches!(
        sandbox_b.status().await.unwrap(),
        pc::SandboxStatus::Ready
    ));
    // The adopting node reaches the still-running container's process + out-of-band
    // artifacts, and keeps the lease alive.
    let proc = sandbox_b
        .spawn(pc::Command::new(["ignored"]))
        .await
        .unwrap();
    assert_eq!(proc.id(), "cid-run-x");
    assert_eq!(sandbox_b.read_artifact("a1").await.unwrap(), b"work");
    sandbox_b.renew_lease().await.unwrap();
    assert_eq!(rt.st.lock().unwrap().lease_touches, 1);

    sandbox_b.dispose().await.unwrap();
    assert!(matches!(
        sandbox_b.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn open_channel_is_the_agent_transport_capability() {
    let rt = Arc::new(FakeRuntime::default());
    let sandbox = ContainerSandbox {
        runtime: rt.clone(),
        id: "run-x".into(),
        container_id: "cid-x".into(),
        outputs_path: "/mnt/session/outputs".into(),
        realized: Vec::new(),
        lifecycle: Arc::new(ContainerLifecycle::completed()),
    };
    rt.st.lock().unwrap().alive.insert("cid-x".into(), true);
    // Drive it through the neutral AgentTransport port.
    let transport: &dyn AgentTransport = &sandbox;
    assert!(transport.open_channel().await.is_ok());
    // A gone container fails closed.
    rt.st.lock().unwrap().alive.remove("cid-x");
    assert!(sandbox.open_channel().await.is_err());
}

#[tokio::test]
async fn handle_round_trips_and_adopt_reconnects() {
    let rt = Arc::new(FakeRuntime::default());
    let p = provider(rt.clone());
    let sandbox = p.create(&spec("run-2")).await.unwrap();
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind, "container");

    let wire = serde_json::to_string(&handle).unwrap();
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    let adopted = p.adopt(&recovered).await.unwrap();
    assert_eq!(adopted.id(), "run-2");
    let proc = adopted.process("main").await.unwrap();
    assert_eq!(proc.id(), "cid-run-2");
    // late attach fails closed on this tier
    assert!(adopted.attach(spec("x").mounts.remove(0)).await.is_err());
}

#[tokio::test]
async fn adopt_without_container_id_fails_closed() {
    let p = provider(Arc::new(FakeRuntime::default()));
    let bare = pc::SandboxHandle::new("container", "run-3");
    assert!(p.adopt(&bare).await.is_err());
}

#[tokio::test]
async fn create_fails_closed_on_bad_spec_missing_command_and_backend_error() {
    let p = provider(Arc::new(FakeRuntime::default()));

    // Non-absolute outputs → prepare_environment rejects before the runtime.
    let mut bad = spec("run-4");
    bad.outputs_path = "relative/outputs".into();
    assert!(p.create(&bad).await.is_err());

    // Missing process-as-container command → fail closed.
    let mut no_cmd = spec("run-4b");
    no_cmd.extra = None;
    assert!(p.create(&no_cmd).await.is_err());

    // Backend create failure propagates.
    let rt = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            fail_create: true,
            ..Default::default()
        }),
    });
    assert!(provider(rt).create(&spec("run-5")).await.is_err());
}

#[test]
fn runtime_error_messages_render() {
    assert!(RuntimeError::NotFound("c".into()).to_string().contains('c'));
    assert!(RuntimeError::Backend("x".into()).to_string().contains('x'));
}

#[tokio::test]
async fn allowlist_egress_injects_the_brokered_proxy_env_at_create() {
    let rt = Arc::new(FakeRuntime::default());
    // `provider()` carries a proxy; the Allowlist spec routes through it.
    provider(rt.clone())
        .create(&spec("run-egress"))
        .await
        .unwrap();

    let st = rt.st.lock().unwrap();
    let env = st
        .created_env
        .get("cid-run-egress")
        .expect("container was created");
    assert!(
        env.contains(&("HTTPS_PROXY".into(), "http://gw.internal:8888".into())),
        "the sandbox must route egress through the brokered proxy: {env:?}"
    );
    assert!(env.iter().any(|(k, _)| k == "NO_PROXY"));
    // The spec's own inline env is preserved alongside the injected proxy vars.
    assert!(env.contains(&("TZ".into(), "UTC".into())));
}

#[tokio::test]
async fn allowlist_without_a_proxy_fails_create_closed() {
    // A provider with no configured chokepoint cannot enforce an allowlist, so it
    // rejects the spec rather than silently opening egress.
    let rt = Arc::new(FakeRuntime::default());
    let p = ContainerProvider::new(rt.clone(), "ghcr.io/awaken/sandbox:latest")
        .with_blob("file-1", b"in-bytes".to_vec())
        .with_blob("res-9", b"work-bytes".to_vec());
    assert!(p.create(&spec("run-noproxy")).await.is_err());
    // Fail-closed BEFORE the runtime is touched: nothing was created.
    assert!(rt.st.lock().unwrap().created_env.is_empty());
}

#[tokio::test]
async fn unrestricted_egress_injects_no_proxy_env() {
    let rt = Arc::new(FakeRuntime::default());
    let mut open = spec("run-open");
    open.network = pc::NetworkPolicy::Unrestricted;
    // No proxy needed for unrestricted egress; seed the spec's required mounts.
    let p = ContainerProvider::new(rt.clone(), "ghcr.io/awaken/sandbox:latest")
        .with_blob("file-1", b"in-bytes".to_vec())
        .with_blob("res-9", b"work-bytes".to_vec());
    p.create(&open).await.unwrap();

    let st = rt.st.lock().unwrap();
    let env = st.created_env.get("cid-run-open").unwrap();
    assert!(env.iter().all(|(k, _)| k != "HTTPS_PROXY"));
}

#[tokio::test]
async fn open_agent_creates_the_container_and_returns_its_channel_and_process() {
    // The host-facing seam: realize the container running the ACP agent and hand back
    // its channel + process handle (runtime chosen behind the `dyn` by worker config).
    let rt = Arc::new(FakeRuntime::default());
    let provider: Box<dyn AgentContainerProvider> = Box::new(provider(rt.clone()));
    let session = provider.open_agent(&spec("run-oa")).await.unwrap();

    // The process handle IS the container's main process (process-as-container).
    assert_eq!(session.process.id(), "cid-run-oa");
    assert_eq!(
        session.process.poll().await.unwrap(),
        Some(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    );
    // The durable handle carries the container id for reattach.
    assert_eq!(session.handle.provider_kind, "container");
    assert_eq!(
        session
            .handle
            .extra
            .as_ref()
            .and_then(|v| v.get("container_id"))
            .and_then(|v| v.as_str()),
        Some("cid-run-oa")
    );
    // A live duplex channel was opened (the ACP bridge would drive it).
    let _channel = session.channel;
    // The container was actually created with the agent argv as its command.
    assert_eq!(
        rt.st.lock().unwrap().created_command.get("cid-run-oa"),
        Some(&vec!["claude".to_string(), "--acp".to_string()])
    );
}

#[tokio::test]
async fn durable_writable_secret_is_materialized_and_written_back_after_process_exit() {
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#;
    let rt = Arc::new(FakeRuntime::default().refreshing_credential(refreshed));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"tokens":{"access_token":"old"}}"#.to_vec();
    let provider =
        ContainerProvider::new(rt.clone(), "agent:latest").with_secret_broker(broker.clone());
    let spec = pc::SandboxSpec {
        scope: "credential-refresh".into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "native-auth".into(),
            source: pc::MountSource::Secret {
                reference: "credential://acp/native/codex".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/auth.json".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({"command": ["agent"]})),
    };

    let session = provider.open_agent(&spec).await.unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let source = rt
            .st
            .lock()
            .unwrap()
            .credential_source
            .clone()
            .expect("credential staging source");
        assert_eq!(
            std::fs::metadata(source.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o777
        );
        assert_eq!(
            std::fs::metadata(source.parent().unwrap().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(source).unwrap().permissions().mode() & 0o777,
            0o666
        );
    }
    session.process.wait().await.unwrap();
    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[refreshed.to_vec()]
    );
    // Idempotent poll/wait cannot reseal the same refresh twice.
    session.process.poll().await.unwrap();
    assert_eq!(broker.writes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn remote_runtime_harvests_live_credential_before_signal() {
    let refreshed = br#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"rotated"}}"#;
    let rt = Arc::new(FakeRuntime::default().with_live_credential(refreshed));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"claudeAiOauth":{"accessToken":"old"}}"#.to_vec();
    let provider =
        ContainerProvider::new(rt.clone(), "agent:latest").with_secret_broker(broker.clone());
    let mut spec = spec("remote-credential-refresh");
    spec.network = pc::NetworkPolicy::Unrestricted;
    spec.mounts = vec![pc::MountRequirement {
        mount_id: "native-auth".into(),
        source: pc::MountSource::Secret {
            reference: "credential://acp/native/claude".into(),
            content_hash: None,
        },
        mount_path: "/acp-config/.credentials.json".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    }];

    let session = provider.open_agent(&spec).await.unwrap();
    session.process.signal(pc::Signal::Term).await.unwrap();
    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[refreshed.to_vec()]
    );
    assert_eq!(rt.st.lock().unwrap().signals.len(), 1);
}

// ── BlobSource resolution (File/Resource/Secret by id) ───────────────────────────

/// A minimal single-entry [`pc::BlobSource`] so the store path is exercised without a
/// durable store (the provider links none — A-G17).
struct OneBlob(&'static str, Vec<u8>);

#[async_trait::async_trait]
impl pc::BlobSource for OneBlob {
    async fn get(&self, id: &str) -> Option<Vec<u8>> {
        (id == self.0).then(|| self.1.clone())
    }
}

fn file_mount_spec(scope: &str, source: pc::MountSource, required: bool) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "f".into(),
            source,
            mount_path: "/data/f.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required,
        }],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/out".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

#[tokio::test]
async fn resolve_and_stage_realizes_a_file_from_the_seed() {
    let spec = file_mount_spec(
        "res-file",
        pc::MountSource::File {
            file_id: "blob-1".into(),
            content_hash: None,
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()]);
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), b"resolved-file-bytes".to_vec());

    let guard = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None)
        .await
        .expect("resolve");
    assert!(guard.guard.is_some(), "bytes were staged");
    let bind = &plan.binds[0];
    // `content` is filled so the k8s ConfigMap path projects the resolved File...
    assert_eq!(bind.content.as_deref(), Some("resolved-file-bytes"));
    // ...and a host staging file (bound by docker/podman) holds the same bytes.
    assert_ne!(
        bind.source_ref, "blob-1",
        "source_ref was repointed off the id"
    );
    assert_eq!(
        std::fs::read(&bind.source_ref).unwrap(),
        b"resolved-file-bytes"
    );
}

#[tokio::test]
async fn resolve_and_stage_resolves_a_resource_from_the_injected_store() {
    let spec = file_mount_spec(
        "res-store",
        pc::MountSource::Resource {
            resource_id: "res-9".into(),
            content_hash: None,
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()]);
    let store: Option<std::sync::Arc<dyn pc::BlobSource>> = Some(std::sync::Arc::new(OneBlob(
        "res-9",
        b"from-the-store".to_vec(),
    )));

    resolve_and_stage(&spec, &mut plan.binds, &HashMap::new(), &store, &None)
        .await
        .expect("resolve from store");
    assert_eq!(plan.binds[0].content.as_deref(), Some("from-the-store"));
}

#[tokio::test]
async fn resolve_and_stage_fails_closed_on_a_required_unresolved_mount() {
    let spec = file_mount_spec(
        "res-missing",
        pc::MountSource::File {
            file_id: "absent".into(),
            content_hash: None,
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()]);
    let e = resolve_and_stage(&spec, &mut plan.binds, &HashMap::new(), &None, &None)
        .await
        .expect_err("a required mount with no bytes must fail closed");
    assert!(e.to_string().contains("did not resolve"), "{e}");
}

#[tokio::test]
async fn resolve_and_stage_rejects_a_content_hash_mismatch() {
    let spec = file_mount_spec(
        "res-tamper",
        pc::MountSource::File {
            file_id: "blob-1".into(),
            content_hash: Some("not-the-real-hash".into()),
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()]);
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), b"whatever".to_vec());
    let e = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None)
        .await
        .expect_err("a hash mismatch must fail closed");
    assert!(e.to_string().contains("hash mismatch"), "{e}");
}

#[tokio::test]
async fn resolve_and_stage_verifies_a_matching_content_hash() {
    let bytes = b"pinned-bytes".to_vec();
    let hash = content_fingerprint(&bytes);
    let spec = file_mount_spec(
        "res-pin",
        pc::MountSource::File {
            file_id: "blob-1".into(),
            content_hash: Some(hash),
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()]);
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), bytes);
    resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None)
        .await
        .expect("a matching pin resolves");
    assert_eq!(plan.binds[0].content.as_deref(), Some("pinned-bytes"));
}
