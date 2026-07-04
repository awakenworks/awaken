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
    let plan = container_plan(&spec("s1"), "ghcr.io/awaken/agent:latest", &cmd);
    assert_eq!(
        plan.command,
        vec!["claude".to_string(), "--acp".to_string()]
    );
    assert_eq!(plan.image, "ghcr.io/awaken/agent:latest");
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
fn mount_ref_covers_every_source_kind() {
    let s = |src| BindPlan {
        source_ref: mount_ref(&src),
        mount_path: String::new(),
        read_only: false,
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
    exits: HashMap<String, pc::ExitStatus>,
    signals: Vec<(String, pc::Signal)>,
    lease_touches: u32,
    artifacts: Vec<pc::Artifact>,
    blobs: HashMap<String, Vec<u8>>,
    fail_create: bool,
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
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn create(&self, id: &str, plan: &ContainerPlan) -> Result<String, RuntimeError> {
        let mut st = self.st.lock().unwrap();
        if st.fail_create {
            return Err(RuntimeError::Backend("image pull failed".into()));
        }
        let cid = format!("cid-{id}");
        st.alive.insert(cid.clone(), true);
        st.created_command.insert(cid.clone(), plan.command.clone());
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

fn provider(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    ContainerProvider::new(runtime, "ghcr.io/awaken/agent:latest")
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
async fn open_channel_is_the_agent_transport_capability() {
    let rt = Arc::new(FakeRuntime::default());
    let sandbox = ContainerSandbox {
        runtime: rt.clone(),
        id: "run-x".into(),
        container_id: "cid-x".into(),
        outputs_path: "/mnt/session/outputs".into(),
        realized: Vec::new(),
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
