//! Slice 5 coverage: pure planners (container/pod) and the provider lifecycle over
//! an in-memory fake [`ContainerRuntime`] — Session environment plus attempt execs,
//! no daemon required.

use super::*;
use awaken_provisioning_contract::SandboxProvider;
use std::collections::HashMap;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::AsyncWriteExt;

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: vec!["claude".into(), "--acp".into()],
        deny_tool_egress: false,
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
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits {
            cpu_millis: Some(2000),
            memory_bytes: Some(1 << 30),
            pids: Some(256),
            disk_bytes: None,
        },
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: Some(60),
        control_services: Default::default(),
    }
}

// ── Pure planners ─────────────────────────────────────────────────────────────

#[test]
fn container_capabilities_are_the_strongest_tier() {
    let c = container_capabilities(true, false, false, Default::default());
    assert_eq!(c.isolation, pc::IsolationClass::Container);
    assert!(c.tool_transparent && c.enforced_readonly && c.network_isolation);
    assert!(c.resource_limits && c.custom_rootfs);
    assert!(!c.enforced_network_allowlist);
    assert!(
        !c.secret_egress_substitution,
        "the current container provider must not advertise an unimplemented secret guarantee"
    );
}

#[test]
fn current_container_provider_rejects_egress_only_secret_injection() {
    // Cause graph/table:
    // | EgressOnly requested | provider substitution | launch/result |
    // | T                    | F                     | reject before launch |
    // | T                    | T                     | admit (future provider suite) |
    // The current provider occupies only the first row and must never claim the
    // second merely because it has container network isolation.
    let mut requested = spec("egress-only");
    requested.env[1].visibility = pc::EnvVisibility::EgressOnly;
    assert_eq!(
        pc::prepare_environment(
            &requested,
            &container_capabilities(true, false, false, Default::default()),
        ),
        Err(pc::PrepareError::EgressSecretUnsupported("API_KEY".into())),
        "an unsupported provider must fail before materializing or launching the sandbox"
    );
}

#[test]
fn container_runtime_without_package_builder_rejects_before_launch() {
    let mut requested = spec("packages");
    requested.packages = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    assert_eq!(
        pc::prepare_environment(
            &requested,
            &container_capabilities(true, false, false, Default::default()),
        ),
        Err(pc::PrepareError::PackageProvisioningUnsupported),
        "Kubernetes and out-of-tree runtimes without immutable image builds must fail before a workload exists"
    );
}

#[derive(Default)]
struct RecordingPackageProvisioner {
    calls: Mutex<Vec<(String, pc::PackageRequirements, pc::NetworkPolicy)>>,
}

#[async_trait]
impl PackageImageProvisioner for RecordingPackageProvisioner {
    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &pc::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        self.calls.lock().unwrap().push((
            base_image.to_string(),
            packages.clone(),
            network.clone(),
        ));
        Ok("registry.test/awaken-packages@sha256:prepared".into())
    }
}

#[tokio::test]
async fn an_external_package_provisioner_enables_a_non_building_runtime() {
    let runtime = Arc::new(FakeRuntime::default());
    let builder = Arc::new(RecordingPackageProvisioner::default());
    let provider = provider(runtime.clone()).with_package_provisioner(builder.clone());
    assert!(provider.capabilities().package_provisioning);

    let mut requested = spec("external-package-builder");
    requested.packages = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    provider
        .create_container(&requested)
        .await
        .expect("external builder prepares the immutable image before create");

    let calls = builder.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "ghcr.io/awaken/sandbox:latest");
    assert_eq!(calls[0].2, pc::NetworkPolicy::Unrestricted);
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .created_images
            .get("cid-external-package-builder")
            .map(String::as_str),
        Some("registry.test/awaken-packages@sha256:prepared")
    );
}

#[test]
fn command_of_reads_the_agent_argv_or_defaults_empty() {
    assert_eq!(
        command_of(&spec("s")),
        vec!["claude".to_string(), "--acp".to_string()]
    );
    let mut bare = spec("s");
    bare.command.clear();
    assert!(command_of(&bare).is_empty());
}

#[test]
fn container_plan_maps_command_image_env_binds_network_and_outputs() {
    let cmd = command_of(&spec("s1"));
    let plan = container_plan(&spec("s1"), "ghcr.io/awaken/sandbox:latest", &cmd, None).unwrap();
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
    assert_eq!(plan.network, NetworkMode::Open);
    assert_eq!(plan.limits.cpu_millis, Some(2000));
}

#[test]
fn container_plan_honors_an_image_override_and_network_variants() {
    /* Container-image authority cause/effect decision table. Causes: C1 the
     * canonical Environment declares an OCI image; C2 it declares a non-image
     * Environment; C3 it declares no Environment.
     * Effects: E1 Docker/Kubernetes `plan.image` and Podman `plan.rootfs` select
     * the same canonical reference; E2 the configured base is the final fallback.
     * Rules: I1 C1=>E1; I2 C2=>E2; I3 C3=>E2. This prevents a prepared
     * package Environment from silently falling back to the package-free image. */
    let mut s = spec("s2");
    s.environment = Some(pc::EnvironmentKind::Image {
        reference: "custom:1".into(),
    });
    s.network = pc::NetworkPolicy::None;
    assert_eq!(
        container_plan(&s, "def", &[], None).unwrap().image,
        "custom:1"
    );
    assert_eq!(
        container_plan(&s, "def", &[], None).unwrap().network,
        NetworkMode::None
    );

    s.network = pc::NetworkPolicy::Unrestricted;
    assert_eq!(
        container_plan(&s, "def", &[], None).unwrap().network,
        NetworkMode::Open
    );

    s.environment = Some(pc::EnvironmentKind::Image {
        reference: "registry.example/prepared@sha256:exact".into(),
    });
    let prepared = container_plan(&s, "def", &[], None).unwrap();
    assert_eq!(
        prepared.image, "registry.example/prepared@sha256:exact",
        "I1"
    );
    assert_eq!(
        prepared.rootfs,
        RootfsPlan::Image("registry.example/prepared@sha256:exact".into()),
        "I1/E1"
    );

    s.environment = Some(pc::EnvironmentKind::Sandbox);
    assert_eq!(
        container_plan(&s, "def", &[], None).unwrap().image,
        "def",
        "I2"
    );

    s.environment = None;
    assert_eq!(
        container_plan(&s, "def", &[], None).unwrap().image,
        "def",
        "I3"
    );
}

#[test]
fn container_plan_resolves_rootfs_from_a_declared_environment_or_falls_back_to_image() {
    // No `environment` declared → the container runs its resolved image.
    let plan = container_plan(&spec("s"), "def:img", &["x".to_string()], None).unwrap();
    assert_eq!(plan.rootfs, RootfsPlan::Image("def:img".into()));

    // A declared Image environment is honored as the rootfs.
    let mut img = spec("s");
    img.command = vec!["x".into()];
    img.environment = Some(pc::EnvironmentKind::Image {
        reference: "ghcr.io/x:2".into(),
    });
    assert_eq!(
        container_plan(&img, "def:img", &["x".to_string()], None)
            .unwrap()
            .rootfs,
        RootfsPlan::Image("ghcr.io/x:2".into())
    );

    // A declared IsolatedRoot(Dir) becomes a private RootDir the podman adapter honors.
    let mut iso = spec("s");
    iso.command = vec!["x".into()];
    iso.environment = Some(pc::EnvironmentKind::IsolatedRoot {
        base: pc::RootfsSource::Dir {
            path_template: "/roots/{scope}".into(),
        },
        writable_base: true,
    });
    assert_eq!(
        container_plan(&iso, "def:img", &["x".to_string()], None)
            .unwrap()
            .rootfs,
        RootfsPlan::RootDir {
            path_template: "/roots/{scope}".into(),
            writable: true,
        }
    );

    // A non-container environment (Scope) has no container-tier rootfs; it is ignored
    // and falls back to the image — never silently realized as a borrowed userland.
    let mut scope = spec("s");
    scope.command = vec!["x".into()];
    scope.environment = Some(pc::EnvironmentKind::Scope);
    assert_eq!(
        container_plan(&scope, "def:img", &["x".to_string()], None)
            .unwrap()
            .rootfs,
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
            store_id: "m".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
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
        s(pc::MountSource::Inline {
            contents: String::new()
        })
        .source_ref,
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
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/.mnt/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let plan = container_plan(&s, "img", &["x".to_string()], None).unwrap();
    // The memory store is NOT a byte bind — binds stay the 2 file/resource mounts.
    assert_eq!(plan.binds.len(), 2);
    assert!(plan.binds.iter().all(|b| b.source_ref != "store-42"));
    // It is realized as a distinct Memory plan through the canonical mounter.
    assert_eq!(plan.memory_mounts.len(), 1);
    assert_eq!(plan.memory_mounts[0].store_id, "store-42");
    assert_eq!(plan.memory_mounts[0].mount_path, "/workspace/.mnt/notes");
}

#[test]
fn host_live_input_projection_has_one_stable_read_only_bind_and_atomic_generations() {
    /* Cause/effect graph and decision table — HLI1:
     * C1 Host-bind runtime; C2 initial managed file present/absent; C3 later valid
     * attach/remove; C4 escaped path. C1 => E1 exactly one read-only stable-root
     * bind even when C2 is absent. C1+C2 => E2 initial bytes exist below that root.
     * C1+C3 => E3 replace/add/remove changes the same tree atomically. C1+C4 =>
     * E4 reject without writing outside the root.
     *
     * | Rule | initial | later operation | path valid | effect                    |
     * | H1   | yes     | none            | yes        | root bind + seeded bytes  |
     * | H2   | no      | attach          | yes        | same root gains file      |
     * | H3   | any     | remove          | yes        | file/empty parents gone   |
     * | H4   | any     | attach          | no         | fail closed               |
     */
    let mut guard = None;
    let staging = staging_dir(&mut guard, "host-live-input-test").unwrap();
    let initial = staging.join("initial");
    std::fs::write(&initial, b"generation-a").unwrap();
    let ordinary = staging.join("ordinary");
    std::fs::write(&ordinary, b"ordinary").unwrap();
    let mut binds = vec![
        BindPlan {
            source_ref: initial.to_string_lossy().into_owned(),
            mount_path: "/mnt/session/uploads/workspace/input.txt".into(),
            read_only: true,
            content: Some("generation-a".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
        BindPlan {
            source_ref: ordinary.to_string_lossy().into_owned(),
            mount_path: "/acp-config/config.toml".into(),
            read_only: true,
            content: Some("ordinary".into()),
            content_bytes: None,
            secret_content: None,
            secret_writeback: false,
            credential_file_path: None,
        },
    ];

    live_inputs::stage_host_projection("host-live-input-test", &mut binds, &mut guard).unwrap();
    assert_eq!(
        binds.len(),
        2,
        "managed files collapse; ordinary binds remain"
    );
    let root_bind = binds
        .iter()
        .find(|bind| bind.mount_path == LIVE_INPUTS_ROOT)
        .expect("one stable live-input root bind");
    assert!(root_bind.read_only);
    let root = std::path::Path::new(&root_bind.source_ref);
    assert_eq!(
        std::fs::read(root.join("workspace/input.txt")).unwrap(),
        b"generation-a"
    );

    live_inputs::project_host_input(
        root,
        "/mnt/session/uploads/workspace/next.txt",
        b"generation-b",
    )
    .unwrap();
    assert_eq!(
        std::fs::read(root.join("workspace/next.txt")).unwrap(),
        b"generation-b"
    );
    live_inputs::remove_host_input(root, "/mnt/session/uploads/workspace/next.txt").unwrap();
    assert!(!root.join("workspace/next.txt").exists());
    assert!(
        live_inputs::project_host_input(root, "/mnt/session/uploads/../escaped", b"forbidden")
            .is_err()
    );

    let mut empty_guard = None;
    let mut empty_binds = Vec::new();
    live_inputs::stage_host_projection(
        "host-live-input-empty-test",
        &mut empty_binds,
        &mut empty_guard,
    )
    .unwrap();
    assert_eq!(
        empty_binds.len(),
        1,
        "H2 reserves the root before first attach"
    );
    let empty_root = std::path::Path::new(&empty_binds[0].source_ref);
    live_inputs::project_host_input(
        empty_root,
        "/mnt/session/uploads/workspace/first.txt",
        b"first",
    )
    .unwrap();
    assert_eq!(
        std::fs::read(empty_root.join("workspace/first.txt")).unwrap(),
        b"first"
    );
}

#[tokio::test]
async fn memory_store_realizes_as_copy_on_the_container_tier() {
    // Cause/effect graph: C1 a Memory requirement names an exact protocol path;
    // C2 access is ReadWrite or ReadOnly. E1 the container binds the canonical
    // mounter copy at that exact path, without `/workspace` or hidden `.mnt`;
    // E2 the runtime bind's read-only flag exactly projects C2.
    //
    // | Rule | access | bind flag | effects |
    // | M1   | RW     | false     | E1,E2   |
    // | M2   | RO     | true      | E1,E2   |
    // Constraint: the container runtime enforces access; the Memory mounter is
    // the one content/lifecycle owner and must not be reimplemented here.
    let rt = Arc::new(FakeRuntime::default());
    let mut s = spec("mem-real");
    s.mounts.push(pc::MountRequirement {
        mount_id: "notes".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-7".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    s.mounts.push(pc::MountRequirement {
        mount_id: "archive".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-8".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/archive".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let p = provider(rt);
    let torn_down = Arc::new(AtomicBool::new(false));
    p.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
    }));
    let sandbox = p.create(&s).await.unwrap();
    let mem = sandbox
        .realized()
        .iter()
        .find(|r| r.mount_id == "notes")
        .expect("memory mount realized");
    // Portable default: the container tier reports the canonical mounter's Copy,
    // not a second runtime-local store.
    assert_eq!(mem.realization, pc::Realization::Copy);
    {
        let state = p.runtime.st.lock().unwrap();
        let bind = state.created_binds["cid-mem-real"]
            .iter()
            .find(|bind| bind.mount_path == "/mnt/memory/notes")
            .expect("portable memory copy is bound into the container");
        assert!(!bind.read_only);
        assert_eq!(
            std::fs::read(std::path::Path::new(&bind.source_ref).join("seed.txt")).unwrap(),
            b"seed"
        );
        let read_only = state.created_binds["cid-mem-real"]
            .iter()
            .find(|bind| bind.mount_path == "/mnt/memory/archive")
            .expect("M2 read-only memory copy is bound into the container");
        assert!(read_only.read_only, "M2/E2");
    }
    sandbox.dispose().await.unwrap();
    assert!(torn_down.load(Ordering::SeqCst));
}

fn one_file_archive(path: &str, bytes: &[u8]) -> Vec<u8> {
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o660);
    header.set_cksum();
    archive.append_data(&mut header, path, bytes).unwrap();
    archive.into_inner().unwrap()
}

struct HarvestingMemoryMounter {
    reference: Arc<Mutex<Option<String>>>,
    harvested: Arc<Mutex<Option<Vec<u8>>>>,
}

struct HarvestingMemoryMount {
    root: std::path::PathBuf,
    harvested: Arc<Mutex<Option<Vec<u8>>>>,
}

#[async_trait]
impl pc::MemoryMounter for HarvestingMemoryMounter {
    async fn mount(
        &self,
        store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<Box<dyn pc::MemoryMount>, pc::SandboxError> {
        *self.reference.lock().unwrap() = Some(store_id.to_owned());
        std::fs::create_dir_all(host_path)
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        std::fs::write(host_path.join("seed.txt"), b"seed")
            .map_err(|error| pc::SandboxError::new(error.to_string()))?;
        Ok(Box::new(HarvestingMemoryMount {
            root: host_path.to_path_buf(),
            harvested: self.harvested.clone(),
        }))
    }
}

#[async_trait]
impl pc::MemoryMount for HarvestingMemoryMount {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Copy
    }

    async fn teardown(self: Box<Self>) {
        *self.harvested.lock().unwrap() = std::fs::read(self.root.join("changed.txt")).ok();
    }
}

#[tokio::test]
async fn native_memory_volume_seeds_and_harvests_through_the_same_mounter() {
    /* Native Memory lifecycle cause/effect decision table — NM1:
     * C1 a claim-fenced materialization reference resolves through the installed
     * mounter; C2 the runtime owns a native volume; C3 access is read-write; C4
     * the live tree can be archived. C1+C2 => E1 the runtime receives the seeded
     * tar and no host bind/second store. C1+C2+C3+C4 => E2 disposal replaces the
     * staging copy and the same mount handle harvests it exactly once. !C1 or !C4
     * fails before removal, retaining the environment for retry; read-only access
     * deliberately has no harvest effect (KM1 covers its Pod mount projection).
     */
    let archive = one_file_archive("changed.txt", b"changed-in-pod");
    let runtime = Arc::new(FakeRuntime::default().with_native_memory_archive(archive));
    let mut sandbox_spec = spec("native-memory");
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "mutable-store-id".into(),
            materialization_reference: Some("claim-fenced-reference".into()),
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/.mnt/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let provider = provider(runtime.clone());
    let reference = Arc::new(Mutex::new(None));
    let harvested = Arc::new(Mutex::new(None));
    provider.install_memory_mounter(Arc::new(HarvestingMemoryMounter {
        reference: reference.clone(),
        harvested: harvested.clone(),
    }));

    let sandbox = provider.create(&sandbox_spec).await.expect("NM1 create");
    assert_eq!(
        reference.lock().unwrap().as_deref(),
        Some("claim-fenced-reference"),
        "E1 exact reference"
    );
    {
        let state = runtime.st.lock().unwrap();
        assert!(
            state.created_binds["cid-native-memory"]
                .iter()
                .all(|bind| bind.mount_path != "/workspace/.mnt/memory")
        );
        let mount = &state.created_memory_mounts["cid-native-memory"][0];
        assert!(!mount.snapshot_tar.is_empty(), "E1 seeded snapshot");
    }

    sandbox.dispose().await.expect("NM1 dispose");
    assert_eq!(
        harvested.lock().unwrap().as_deref(),
        Some(b"changed-in-pod".as_slice()),
        "E2 same mounter harvest"
    );
}

struct FakeMemoryMounter {
    torn_down: Arc<AtomicBool>,
}

struct FakeMemoryMount(Arc<AtomicBool>);

#[async_trait]
impl pc::MemoryMounter for FakeMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<Box<dyn pc::MemoryMount>, pc::SandboxError> {
        std::fs::create_dir_all(host_path).map_err(|e| pc::SandboxError::new(e.to_string()))?;
        std::fs::write(host_path.join("seed.txt"), b"seed")
            .map_err(|e| pc::SandboxError::new(e.to_string()))?;
        Ok(Box::new(FakeMemoryMount(self.torn_down.clone())))
    }
}

#[async_trait]
impl pc::MemoryMount for FakeMemoryMount {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Copy
    }

    async fn teardown(self: Box<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// ── Fake runtime + provider lifecycle ───────────────────────────────────────────

type RuntimePathObservation = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

#[derive(Default)]
struct FakeState {
    alive: HashMap<String, bool>,
    created_command: HashMap<String, Vec<String>>,
    created_env: HashMap<String, Vec<(String, String)>>,
    created_binds: HashMap<String, Vec<BindPlan>>,
    created_images: HashMap<String, String>,
    exits: HashMap<String, pc::ExitStatus>,
    signals: Vec<(String, pc::Signal)>,
    lease_touches: u32,
    artifacts: Vec<pc::Artifact>,
    blobs: HashMap<String, Vec<u8>>,
    fail_create: bool,
    refreshed_credential: Option<Vec<u8>>,
    live_credential: Option<Vec<u8>>,
    live_credential_error: Option<String>,
    credential_source: Option<std::path::PathBuf>,
    spawned: Vec<(String, Vec<String>)>,
    runtime_path_observations: Vec<RuntimePathObservation>,
    process_secret_observations: Vec<(bool, bool)>,
    live_input_projection: bool,
    live_inputs: HashMap<String, Vec<u8>>,
    native_memory: bool,
    memory_archive: Vec<u8>,
    created_memory_mounts: HashMap<String, Vec<MemoryMount>>,
    control_enabled: bool,
    control_binding_missing: bool,
    control_binding_calls: Vec<(bool, Option<String>)>,
    control_channel_opens: usize,
    control_peers: Vec<tokio::io::DuplexStream>,
    removals: Vec<(String, usize)>,
}

#[derive(Default)]
struct FakeRuntime {
    st: Mutex<FakeState>,
}

struct FakeExecProcess {
    id: String,
}

#[async_trait]
impl pc::ProcessHandle for FakeExecProcess {
    fn id(&self) -> &str {
        &self.id
    }

    async fn wait(&self) -> Result<pc::ExitStatus, pc::SandboxError> {
        Ok(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    }

    async fn poll(&self) -> Result<Option<pc::ExitStatus>, pc::SandboxError> {
        Ok(Some(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        }))
    }

    async fn signal(&self, _signal: pc::Signal) -> Result<(), pc::SandboxError> {
        Ok(())
    }
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

    fn failing_live_credential(self, message: &str) -> Self {
        self.st.lock().unwrap().live_credential_error = Some(message.into());
        self
    }

    fn with_live_input_projection(self) -> Self {
        self.st.lock().unwrap().live_input_projection = true;
        self
    }

    fn with_native_memory_archive(self, archive: Vec<u8>) -> Self {
        let mut state = self.st.lock().unwrap();
        state.native_memory = true;
        state.memory_archive = archive;
        drop(state);
        self
    }

    fn with_sandbox_control(self) -> Self {
        self.st.lock().unwrap().control_enabled = true;
        self
    }

    fn with_missing_sandbox_control_binding(self) -> Self {
        let mut state = self.st.lock().unwrap();
        state.control_enabled = true;
        state.control_binding_missing = true;
        drop(state);
        self
    }
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn enforces_network_none(&self) -> bool {
        true
    }

    fn sandbox_control_services(&self) -> std::collections::BTreeSet<SandboxControlServiceKind> {
        if self.st.lock().unwrap().control_enabled {
            std::collections::BTreeSet::from([SandboxControlServiceKind::RepositoryGitCredential])
        } else {
            std::collections::BTreeSet::new()
        }
    }

    async fn sandbox_control_binding(
        &self,
        _container_id: &str,
        request: SandboxControlBindingRequest<'_>,
    ) -> Result<Option<pc::SandboxControlIncarnation>, RuntimeError> {
        let expected = match request {
            SandboxControlBindingRequest::New { .. } => None,
            SandboxControlBindingRequest::Adopt { expected, .. } => expected
                .and_then(pc::SandboxControlIncarnation::kubernetes_pod_uid)
                .map(str::to_owned),
        };
        self.st.lock().unwrap().control_binding_calls.push((
            matches!(request, SandboxControlBindingRequest::Adopt { .. }),
            expected,
        ));
        if self.st.lock().unwrap().control_binding_missing {
            return Ok(None);
        }
        pc::SandboxControlIncarnation::kubernetes_pod("fake-control-incarnation")
            .map(Some)
            .map_err(|error| RuntimeError::Backend(error.to_string()))
    }

    async fn open_sandbox_control_channel(
        &self,
        _container_id: &str,
        _binding: &pc::SandboxControlIncarnation,
        _kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        let (host, peer) = tokio::io::duplex(64 * 1024);
        let mut state = self.st.lock().unwrap();
        state.control_channel_opens += 1;
        state.control_peers.push(peer);
        Ok(Box::new(host))
    }

    fn supports_live_input_projection(&self) -> bool {
        self.st.lock().unwrap().live_input_projection
    }

    fn has_native_memory_mounts(&self) -> bool {
        self.st.lock().unwrap().native_memory
    }

    async fn project_live_input(
        &self,
        _container_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeError> {
        self.st
            .lock()
            .unwrap()
            .live_inputs
            .insert(path.into(), bytes.to_vec());
        Ok(())
    }

    async fn remove_live_input(&self, _container_id: &str, path: &str) -> Result<(), RuntimeError> {
        self.st.lock().unwrap().live_inputs.remove(path);
        Ok(())
    }

    async fn read_live_file(
        &self,
        _container_id: &str,
        _path: &str,
    ) -> Result<Option<Vec<u8>>, RuntimeError> {
        let state = self.st.lock().unwrap();
        match &state.live_credential_error {
            Some(error) => Err(RuntimeError::Backend(error.clone())),
            None => Ok(state.live_credential.clone()),
        }
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
        st.created_binds.insert(cid.clone(), plan.binds.clone());
        st.created_memory_mounts
            .insert(cid.clone(), plan.memory_mounts.clone());
        st.created_images.insert(cid.clone(), plan.image.clone());
        st.exits.insert(
            cid.clone(),
            pc::ExitStatus {
                code: Some(0),
                signaled: false,
            },
        );
        Ok(cid)
    }

    async fn spawn(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        if !self.st.lock().unwrap().alive.contains_key(container_id) {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        let mut state = self.st.lock().unwrap();
        let id = format!("exec-{}", state.spawned.len());
        if let Some(value) = command
            .env
            .iter()
            .find(|value| value.name == "API_KEY")
            .map(|value| {
                (
                    value.value.is_secret(),
                    value.value.expose() == "container-process-secret",
                )
            })
        {
            // Retain only the security observation, never the material itself.
            state.process_secret_observations.push(value);
        }
        let runtime_path = |name: &str| {
            command
                .env
                .iter()
                .find(|value| value.name == name && !value.value.is_secret())
                .map(|value| value.value.expose().to_string())
        };
        state.runtime_path_observations.push((
            runtime_path("AWAKEN_PROJECT_DIR"),
            runtime_path("AWAKEN_OUTPUTS_DIR"),
            runtime_path("HOME"),
            runtime_path("XDG_CONFIG_HOME"),
            runtime_path("XDG_CACHE_HOME"),
        ));
        state.spawned.push((container_id.to_string(), command.argv));
        Ok(Box::new(FakeExecProcess { id }))
    }

    async fn spawn_agent(
        &self,
        container_id: &str,
        command: pc::MaterializedCommand,
    ) -> Result<RuntimeAgentProcess, RuntimeError> {
        let memory_archive = if command.argv.iter().any(|arg| arg == "awaken-read-files") {
            self.st.lock().unwrap().memory_archive.clone()
        } else {
            Vec::new()
        };
        let process = self.spawn(container_id, command).await?;
        let capacity = memory_archive.len().max(64);
        let (ours, mut peer) = tokio::io::duplex(capacity);
        tokio::spawn(async move {
            let _ = peer.write_all(&memory_archive).await;
        });
        Ok(RuntimeAgentProcess {
            process,
            channel: Box::new(ours),
        })
    }

    async fn process(
        &self,
        container_id: &str,
        process_id: &str,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        if !self.st.lock().unwrap().alive.contains_key(container_id) {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        Ok(Box::new(FakeExecProcess {
            id: process_id.to_string(),
        }))
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
        let mut state = self.st.lock().unwrap();
        let opens = state.control_channel_opens;
        state.removals.push((container_id.into(), opens));
        state.alive.insert(container_id.into(), false);
        Ok(())
    }
}

#[derive(Default)]
struct RecordingSecretBroker {
    current: Mutex<Vec<u8>>,
    writes: Mutex<Vec<Vec<u8>>>,
    reject_writeback: AtomicBool,
}

#[async_trait]
impl pc::SecretBroker for RecordingSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        if self.reject_writeback.load(Ordering::SeqCst) {
            return Err(pc::SandboxError::new("injected write-back rejection"));
        }
        *self.current.lock().unwrap() = bytes.clone();
        self.writes.lock().unwrap().push(bytes);
        Ok(())
    }
}

fn writable_credential_spec(scope: &str) -> pc::SandboxSpec {
    let mut sandbox_spec = spec(scope);
    sandbox_spec.network = pc::NetworkPolicy::Unrestricted;
    sandbox_spec.mounts = vec![pc::MountRequirement {
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
    sandbox_spec
}

fn provider_without_broker(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    // A conventional forward proxy exercises connectivity configuration without
    // claiming to enforce a host allowlist. Required mounts are seeded because a
    // missing required source fails closed at create.
    ContainerProvider::new(runtime, "ghcr.io/awaken/sandbox:latest")
        .with_forward_proxy(ForwardProxy {
            url: "http://gw.internal:8888".into(),
        })
        .with_blob("file-1", b"in-bytes".to_vec())
        .with_blob("res-9", b"work-bytes".to_vec())
}

fn provider(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"container-process-secret".to_vec();
    provider_without_broker(runtime).with_secret_broker(broker)
}

struct UnavailableControlService;

#[async_trait]
impl SandboxControlService for UnavailableControlService {
    async fn handle(
        &self,
        _request: awaken_sandbox_control::SandboxControlRequest,
    ) -> awaken_sandbox_control::SandboxControlResponse {
        awaken_sandbox_control::SandboxControlResponse::Unavailable
    }
}

#[tokio::test]
async fn control_binding_is_demand_driven_and_adoption_requires_exact_incarnation() {
    /* Container control-binding cause/effect decision table:
     * C1=empty control demand; C2=typed demand on a capable runtime; C3=generic
     * adoption of exact handle evidence; C4=spec-aware exact adoption;
     * C5=requested/realized mismatch; C6=set/incarnation inconsistency;
     * C7=provider lacks the realized capability; C8=runtime omits an
     * incarnation. E1=no callback and unchanged ordinary creation; E2=one new
     * binding and exact topology persisted; E3=adopt with that exact topology;
     * E4=fail closed before ambient inference. Rules: B1 C1=>E1; B2 C2=>E2;
     * B3 C2+(C3|C4)=>E3; B4 C5|C6|C7|C8=>E4.
     */
    let ordinary_runtime = Arc::new(FakeRuntime::default());
    provider(ordinary_runtime.clone())
        .create_container(&spec("ordinary-control-free"))
        .await
        .expect("B1/E1");
    assert!(
        ordinary_runtime
            .st
            .lock()
            .unwrap()
            .control_binding_calls
            .is_empty(),
        "B1/E1 no callback"
    );

    let runtime = Arc::new(FakeRuntime::default().with_sandbox_control());
    let container_provider = provider(runtime.clone());
    let mut demanded = spec("demanded-control");
    demanded
        .control_services
        .insert(SandboxControlServiceKind::RepositoryGitCredential);
    let sandbox = container_provider
        .create_container(&demanded)
        .await
        .expect("B2/E2");
    let handle = pc::Sandbox::handle(&sandbox);
    let payload = recovery::decode_handle(&handle).unwrap();
    assert_eq!(payload.control_services, demanded.control_services, "B2/E2");
    assert_eq!(
        payload
            .sandbox_control_incarnation
            .as_ref()
            .and_then(pc::SandboxControlIncarnation::kubernetes_pod_uid),
        Some("fake-control-incarnation"),
        "B2/E2 persisted"
    );
    let generic = container_provider
        .adopt_container(&handle)
        .await
        .expect("B3/E3 generic restore");
    assert_eq!(generic.control_services, demanded.control_services, "B3/E3");
    container_provider
        .adopt_container_with_spec(Some(&demanded), &handle)
        .await
        .expect("B4/E3 exact spec");
    assert_eq!(
        runtime.st.lock().unwrap().control_binding_calls,
        vec![
            (false, None),
            (true, Some("fake-control-incarnation".into())),
            (true, Some("fake-control-incarnation".into()))
        ],
        "B2/B3/B4 exact callbacks"
    );

    let missing_incarnation = pc::SandboxHandle::container(
        &handle.sandbox_id,
        pc::ContainerSandboxHandleV1 {
            sandbox_control_incarnation: None,
            ..payload.clone()
        },
    );
    assert!(
        container_provider
            .adopt_container_with_spec(Some(&demanded), &missing_incarnation)
            .await
            .is_err(),
        "B6/E4 realized set without incarnation"
    );

    let missing_set = pc::SandboxHandle::container(
        &handle.sandbox_id,
        pc::ContainerSandboxHandleV1 {
            control_services: Default::default(),
            ..payload.clone()
        },
    );
    assert!(
        container_provider
            .adopt_container(&missing_set)
            .await
            .is_err(),
        "B6/E4 incarnation without realized set"
    );

    let ordinary = spec("demanded-control");
    assert!(
        container_provider
            .adopt_container_with_spec(Some(&ordinary), &handle)
            .await
            .is_err(),
        "B5/E4 requested narrower"
    );
    let legacy = pc::SandboxHandle::container(
        &handle.sandbox_id,
        pc::ContainerSandboxHandleV1 {
            sandbox_control_incarnation: None,
            control_services: Default::default(),
            ..payload.clone()
        },
    );
    assert!(
        container_provider
            .adopt_container_with_spec(Some(&demanded), &legacy)
            .await
            .is_err(),
        "B5/E4 legacy empty handle plus new demand"
    );
    assert!(
        provider(Arc::new(FakeRuntime::default()))
            .adopt_container(&handle)
            .await
            .is_err(),
        "B7/E4 unsupported realized topology"
    );

    let missing_runtime = Arc::new(FakeRuntime::default().with_missing_sandbox_control_binding());
    assert!(
        provider(missing_runtime.clone())
            .create_container(&demanded)
            .await
            .is_err(),
        "B8/E4 demanded create needs an incarnation"
    );
    assert_eq!(
        missing_runtime.st.lock().unwrap().removals.len(),
        1,
        "B8 failed creation reaps its runtime object"
    );
}

async fn take_control_peer(runtime: &FakeRuntime) -> tokio::io::DuplexStream {
    for _ in 0..64 {
        if let Some(peer) = runtime.st.lock().unwrap().control_peers.pop() {
            return peer;
        }
        tokio::task::yield_now().await;
    }
    panic!("control channel was not reopened")
}

async fn exchange_unavailable(peer: &mut tokio::io::DuplexStream) {
    awaken_sandbox_control::write_frame(
        peer,
        &awaken_sandbox_control::SandboxControlRequest::RepositoryGitCredentialGet {
            query: awaken_sandbox_control::RepositoryGitCredentialQuery {
                protocol: "https".into(),
                host: "gateway.test".into(),
                path: "git/repository".into(),
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        awaken_sandbox_control::read_frame::<_, awaken_sandbox_control::SandboxControlResponse>(
            peer
        )
        .await
        .unwrap(),
        awaken_sandbox_control::SandboxControlResponse::Unavailable
    ));
}

#[tokio::test(start_paused = true)]
async fn control_publication_survives_idle_and_channel_failure_but_stops_before_removal() {
    /* Container publication cause/effect decision table:
     * C1=one demanded, incarnation-bound publication; C2=an opened channel is
     * idle for longer than the exchange deadline; C3=one active channel reaches
     * EOF; C4=environment disposal; C5=an old lease drops after disposal.
     * E1=two later requests still receive responses on the same generation;
     * E2=single-channel failure triggers bounded reopen; E3=publication joins
     * before runtime removal; E4=no retry/reopen or stale-lease mutation after
     * disposal. Rules: B5 C1+C2=>E1; B6 C1+C3=>E2;
     * B7 C1+C4=>E3+E4; B8 C4+C5=>E4.
     */
    let runtime = Arc::new(FakeRuntime::default().with_sandbox_control());
    let mut demanded = spec("publication-lifecycle");
    demanded
        .control_services
        .insert(SandboxControlServiceKind::RepositoryGitCredential);
    let sandbox = provider(runtime.clone())
        .create_container(&demanded)
        .await
        .unwrap();
    let lease = SandboxControlServicePublisher::publish_sandbox_control_service(
        &sandbox,
        SandboxControlServiceKind::RepositoryGitCredential,
        Arc::new(UnavailableControlService),
    )
    .await
    .expect("B5 publication");
    let mut first = take_control_peer(runtime.as_ref()).await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    exchange_unavailable(&mut first).await;

    let mut second = take_control_peer(runtime.as_ref()).await;
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    exchange_unavailable(&mut second).await;

    let failed = take_control_peer(runtime.as_ref()).await;
    assert_eq!(runtime.st.lock().unwrap().control_channel_opens, 3, "B5/E1");
    drop(failed);
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    let _reopened = take_control_peer(runtime.as_ref()).await;
    assert_eq!(runtime.st.lock().unwrap().control_channel_opens, 4, "B6/E2");

    pc::Sandbox::dispose(&sandbox).await.expect("B7 dispose");
    {
        let state = runtime.st.lock().unwrap();
        assert_eq!(
            state.removals,
            vec![("cid-publication-lifecycle".into(), 4)],
            "B7/E3 close precedes removal"
        );
        assert_eq!(state.control_channel_opens, 4, "B7/E4");
    }
    tokio::time::advance(std::time::Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert_eq!(runtime.st.lock().unwrap().control_channel_opens, 4, "B7/E4");
    assert!(
        SandboxControlServicePublisher::publish_sandbox_control_service(
            &sandbox,
            SandboxControlServiceKind::RepositoryGitCredential,
            Arc::new(UnavailableControlService),
        )
        .await
        .is_err(),
        "B8/E4"
    );
    drop(lease);
    assert_eq!(runtime.st.lock().unwrap().control_channel_opens, 4, "B8/E4");
}

struct RejectingSecretBroker;

#[async_trait]
impl pc::SecretBroker for RejectingSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("claim expired"))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("claim expired"))
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new("not supported"))
    }
}

/// Container process-secret cause graph:
///
/// C1 a Process-visible secret is effective -> C2 a broker is installed -> C3
/// the exact reference resolves -> E1 the runtime receives a non-serializable,
/// redacted materialized secret. Failure of C2/C3 produces E2 before runtime
/// invocation. With no C1, launch remains independent of a broker.
///
/// | Rule | C1 secret | C2 broker | C3 resolves | Runtime called | Result |
/// |---|---|---|---|---|---|
/// | C1 | F | F | - | T | public launch |
/// | C2 | T | F | - | F | reject |
/// | C3 | T | T | F | F | reject |
/// | C4 | T | T | T | T | typed secret launch |
#[tokio::test]
async fn process_secret_decision_table_fences_container_runtime_invocation() {
    let public_runtime = Arc::new(FakeRuntime::default());
    let mut public_spec = spec("public-process");
    public_spec.env.retain(|value| value.name != "API_KEY");
    let public = provider_without_broker(public_runtime.clone())
        .create_container(&public_spec)
        .await
        .unwrap();
    pc::Sandbox::spawn(&public, pc::Command::new(["true"]))
        .await
        .unwrap();
    assert_eq!(public_runtime.st.lock().unwrap().spawned.len(), 1);

    let missing_runtime = Arc::new(FakeRuntime::default());
    let missing = provider_without_broker(missing_runtime.clone())
        .create_container(&spec("missing-broker"))
        .await
        .unwrap();
    assert!(
        pc::Sandbox::spawn(&missing, pc::Command::new(["true"]))
            .await
            .is_err()
    );
    assert!(missing_runtime.st.lock().unwrap().spawned.is_empty());

    let rejected_runtime = Arc::new(FakeRuntime::default());
    let rejected = provider_without_broker(rejected_runtime.clone())
        .with_secret_broker(Arc::new(RejectingSecretBroker))
        .create_container(&spec("rejected-claim"))
        .await
        .unwrap();
    assert!(
        pc::Sandbox::spawn(&rejected, pc::Command::new(["true"]))
            .await
            .is_err()
    );
    assert!(rejected_runtime.st.lock().unwrap().spawned.is_empty());

    let admitted_runtime = Arc::new(FakeRuntime::default());
    let admitted = provider(admitted_runtime.clone())
        .create_container(&spec("admitted-secret"))
        .await
        .unwrap();
    pc::Sandbox::spawn(&admitted, pc::Command::new(["true"]))
        .await
        .unwrap();
    let admitted_state = admitted_runtime.st.lock().unwrap();
    assert_eq!(admitted_state.spawned.len(), 1);
    assert_eq!(admitted_state.process_secret_observations, [(true, true)]);
}

#[tokio::test]
async fn resident_hand_is_the_only_pid1_path_and_receives_no_platform_credential() {
    /*
     * Resident placement cause/effect graph and decision table.
     * Causes: C1 resident config absent/present; C2 executable blank; C3 port
     * zero; C4 Session spec contains a Process secret; C5 resource limits are
     * positive/zero; C6 the resident Hand executes Bash without an attached
     * process boundary. Effects: E1 legacy
     * keepalive PID1; E2 exactly one `hand --listen` PID1; E3 durable ledger
     * path only in base env; E4 reject invalid config before runtime; E5 never
     * serialize Process/platform credentials into Pod base env; E6 the existing
     * runtime-owned project, output, HOME, and XDG paths reach resident tools.
     * Rules: RP1 !C1=>E1; RP2 C1+!C2+!C3=>E2+E3+E5;
     * RP3 C2||C3||zero(C5)=>E4; RP4 C1+C6=>E6. FMECA: duplicate Hand placement would permit concurrent
     * side effects (severity 5); selecting one command in ContainerProvider is
     * the mitigation. Credential disclosure through environment serialization
     * is severity 5; the existing Process-secret materialization boundary and
     * this negative assertion mitigate/detect it.
     */
    assert!(ResidentHandConfig::new(" ", 7777).is_err());
    assert!(ResidentHandConfig::new("awaken-sandbox", 0).is_err());
    assert!(
        ResidentHandConfig::new("awaken-sandbox", 7777)
            .unwrap()
            .with_resource_limits(0, 1)
            .is_err()
    );

    let runtime = Arc::new(FakeRuntime::default());
    provider(runtime.clone())
        .with_resident_hand(ResidentHandConfig::new("/opt/awaken-sandbox", 7777).unwrap())
        .create_container(&spec("resident"))
        .await
        .unwrap();

    let state = runtime.st.lock().unwrap();
    assert_eq!(
        state.created_command.get("cid-resident").unwrap(),
        &vec![
            "/opt/awaken-sandbox".to_string(),
            "hand".to_string(),
            "--listen".to_string(),
            "127.0.0.1:7777".to_string(),
        ]
    );
    let env = state.created_env.get("cid-resident").unwrap();
    assert!(env.iter().any(|(name, value)| {
        name == "AWAKEN_HAND_LEDGER_DIR" && value == "/tmp/.awaken-hand-operations"
    }));
    assert!(
        env.iter()
            .any(|(name, value)| { name == "AWAKEN_HAND_LEDGER_MAX_ENTRIES" && value == "4096" })
    );
    assert!(
        env.iter()
            .any(|(name, value)| name == "AWAKEN_HAND_MAX_CONNECTIONS" && value == "16")
    );
    for (name, value) in [
        ("AWAKEN_PROJECT_DIR", "/workspace"),
        ("AWAKEN_OUTPUTS_DIR", "/mnt/session/outputs"),
        ("HOME", "/workspace"),
        ("XDG_CONFIG_HOME", "/workspace/.config"),
        ("XDG_CACHE_HOME", "/workspace/.cache"),
    ] {
        assert_eq!(
            env.iter()
                .filter(|(candidate, _)| candidate == name)
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            [value],
            "RP4/E6"
        );
    }
    assert!(env.iter().all(|(name, value)| {
        name != "API_KEY"
            && !name.contains("TOKEN")
            && !name.contains("SECRET")
            && value != "container-process-secret"
    }));
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
    // Creation starts only the Session environment keepalive. Attempt commands use
    // exec and therefore cannot terminate the environment.
    assert_eq!(
        rt.st
            .lock()
            .unwrap()
            .created_command
            .get("cid-run-1")
            .unwrap(),
        &environment_keepalive_command()
    );

    // spawn executes the requested command inside the existing environment;
    // wait/poll/signal act on that exec process, not container PID 1.
    let proc = sandbox.spawn(pc::Command::new(["ignored"])).await.unwrap();
    assert_eq!(proc.id(), "exec-0");
    assert_eq!(
        rt.st.lock().unwrap().spawned,
        vec![("cid-run-1".into(), vec!["ignored".into()])]
    );
    assert_eq!(proc.wait().await.unwrap().code, Some(0));
    assert!(proc.poll().await.unwrap().is_some());
    proc.signal(pc::Signal::Term).await.unwrap();

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
    // Cause/effect recovery table — AR1:
    // C1: node A persists a container handle and disappears; C2: node B shares the
    // runtime and adopts that live handle; C3: the output boundary belongs to the
    // sandbox specification, not either worker process.
    // C1+C2+C3 => E1 node B reaches the same container, E2 its next process receives
    // the exact runtime-owned project/output paths, and E3 artifacts and lease
    // renewal remain available without creating a second environment.
    let rt =
        Arc::new(FakeRuntime::default().with_artifact("a1", "/mnt/session/outputs/o.txt", b"work"));
    let node_a = provider(rt.clone());
    let sandbox_a = node_a.create(&spec("run-x")).await.unwrap();
    let wire = serde_json::to_string(&sandbox_a.handle()).unwrap();
    drop(sandbox_a);
    drop(node_a);

    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    assert_eq!(recovered.provider_kind(), "container");
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
    assert_eq!(proc.id(), "exec-0");
    assert_eq!(
        rt.st
            .lock()
            .unwrap()
            .runtime_path_observations
            .last()
            .cloned(),
        Some((
            Some("/workspace".into()),
            Some("/mnt/session/outputs".into()),
            Some("/workspace".into()),
            Some("/workspace/.config".into()),
            Some("/workspace/.cache".into())
        )),
        "an adopted process receives the same runtime-owned paths"
    );
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
async fn handle_round_trips_and_adopt_reconnects() {
    let rt = Arc::new(FakeRuntime::default());
    let p = provider(rt.clone());
    let sandbox = p.create(&spec("run-2")).await.unwrap();
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind(), "container");

    let wire = serde_json::to_string(&handle).unwrap();
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    let adopted = p.adopt(&recovered).await.unwrap();
    assert_eq!(adopted.id(), "run-2");
    let proc = adopted.process("main").await.unwrap();
    assert_eq!(proc.id(), "main");
    // late attach fails closed on this tier
    assert!(adopted.attach(spec("x").mounts.remove(0)).await.is_err());
}

#[tokio::test]
async fn a_capable_runtime_replaces_only_read_only_files_below_the_live_input_root() {
    // Cause/effect live-input table — LI1:
    // C1: the resident runtime owns an isolated projector; C2: a later generation
    // is a read-only File below /mnt/session/uploads; C3: canonical BlobSource
    // bytes resolve. C1+C2+C3 => E1 attach atomically replaces the projected bytes
    // and E2 removal deletes them; C1 survives handle adoption => E3 recovery uses
    // the same projector. !C1 or !C2 => E4 fail closed without projection.
    let runtime = Arc::new(FakeRuntime::default().with_live_input_projection());
    let sandbox = provider(runtime.clone())
        .create_container(&spec("live-inputs"))
        .await
        .unwrap();
    let input = pc::MountRequirement {
        mount_id: "current-report".into(),
        source: pc::MountSource::File {
            file_id: "file-1".into(),
            content_hash: None,
        },
        mount_path: "/mnt/session/uploads/awaken-design/current/report.html".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    };

    assert!(sandbox.supports_live_mount_replacement(&[], std::slice::from_ref(&input)));
    let realized = pc::Sandbox::attach(&sandbox, input.clone()).await.unwrap();
    assert_eq!(realized.mount_path, input.mount_path);
    assert_eq!(realized.access, pc::MountAccess::ReadOnly);
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .live_inputs
            .get(&input.mount_path),
        Some(&b"in-bytes".to_vec())
    );

    sandbox
        .remove_live_input_path(&input.mount_path)
        .await
        .unwrap();
    assert!(
        !runtime
            .st
            .lock()
            .unwrap()
            .live_inputs
            .contains_key(&input.mount_path)
    );

    let adopted = provider(runtime.clone())
        .adopt_container(&pc::Sandbox::handle(&sandbox))
        .await
        .unwrap();
    assert!(adopted.supports_live_mount_replacement(&[], std::slice::from_ref(&input)));
    pc::Sandbox::attach(&adopted, input.clone())
        .await
        .expect("recovery retains the resident Pod's projector capability");

    let mut outside = input.clone();
    outside.mount_path = "/workspace/report.html".into();
    assert!(!sandbox.supports_live_mount_replacement(&[], std::slice::from_ref(&outside)));
    assert!(pc::Sandbox::attach(&sandbox, outside).await.is_err());

    let mut escaped = input;
    escaped.mount_path = "/mnt/session/uploads/../secret".into();
    assert!(!adopted.supports_live_mount_replacement(&[], std::slice::from_ref(&escaped)));
    assert!(pc::Sandbox::attach(&adopted, escaped).await.is_err());
}

#[tokio::test]
async fn adopt_rejects_a_handle_whose_runtime_is_gone() {
    /* Recovery decision rule A1: a well-formed durable handle plus a live
     * runtime target is adoptable; A2: the same handle after physical teardown
     * is an orphan and must fail adoption so the existing reconciler can
     * re-place it. Merely completing locator decoding is not successful adopt. */
    let rt = Arc::new(FakeRuntime::default());
    let p = provider(rt);
    let sandbox = p.create(&spec("run-gone")).await.unwrap();
    let handle = sandbox.handle();
    sandbox.dispose().await.unwrap();
    assert!(p.adopt(&handle).await.is_err(), "A2");
}

#[tokio::test]
async fn adopt_without_container_id_fails_closed() {
    let p = provider(Arc::new(FakeRuntime::default()));
    let bare = pc::SandboxHandle::new("container", "run-3");
    assert!(p.adopt(&bare).await.is_err());
}

#[tokio::test]
async fn adopt_rejects_a_handle_owned_by_another_provider() {
    let p = provider(Arc::new(FakeRuntime::default()));
    let foreign = pc::SandboxHandle::new("bwrap", "run-3");

    let error = match p.adopt(&foreign).await {
        Ok(_) => panic!("foreign provider handle was accepted"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("cannot adopt"), "{error}");
    assert!(error.contains("bwrap"), "{error}");
}

#[tokio::test]
async fn create_fails_closed_on_bad_spec_and_backend_error_but_needs_no_attempt_command() {
    let p = provider(Arc::new(FakeRuntime::default()));

    // Non-absolute outputs → prepare_environment rejects before the runtime.
    let mut bad = spec("run-4");
    bad.outputs_path = "relative/outputs".into();
    assert!(p.create(&bad).await.is_err());

    // The Session environment is independent of an attempt command.
    let mut no_cmd = spec("run-4b");
    no_cmd.command.clear();
    let environment = p.create(&no_cmd).await.unwrap();
    environment.dispose().await.unwrap();

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
async fn allowlist_fails_before_runtime_with_or_without_a_forward_proxy() {
    for with_proxy in [false, true] {
        let rt = Arc::new(FakeRuntime::default());
        let mut request = spec(if with_proxy {
            "run-allowlist-proxy"
        } else {
            "run-allowlist-direct"
        });
        request.network = pc::NetworkPolicy::Allowlist {
            hosts: vec!["api.anthropic.com".into()],
        };
        let base = ContainerProvider::new(rt.clone(), "ghcr.io/awaken/sandbox:latest")
            .with_blob("file-1", b"in-bytes".to_vec())
            .with_blob("res-9", b"work-bytes".to_vec());
        let result = if with_proxy {
            base.with_forward_proxy(ForwardProxy {
                url: "http://gw.internal:8888".into(),
            })
            .create(&request)
            .await
        } else {
            base.create(&request).await
        };
        assert!(result.is_err());
        assert!(
            rt.st.lock().unwrap().created_env.is_empty(),
            "a process proxy must not turn an allowlist into a runnable open-network container"
        );
    }
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
async fn unrestricted_egress_may_use_a_forward_proxy_for_connectivity() {
    let rt = Arc::new(FakeRuntime::default());
    provider(rt.clone())
        .create(&spec("run-forward-proxy"))
        .await
        .unwrap();
    let st = rt.st.lock().unwrap();
    let env = st.created_env.get("cid-run-forward-proxy").unwrap();
    assert!(env.contains(&("HTTPS_PROXY".into(), "http://gw.internal:8888".into())));
    assert!(env.contains(&("HTTP_PROXY".into(), "http://gw.internal:8888".into())));
    assert!(env.iter().any(|(key, _)| key == "NO_PROXY"));
}

#[tokio::test]
async fn open_agent_creates_the_container_and_returns_its_channel_and_process() {
    // The host-facing seam: realize the container running the ACP agent and hand back
    // its channel + process handle (runtime chosen behind the `dyn` by worker config).
    let rt = Arc::new(FakeRuntime::default());
    let agent_provider: Box<dyn AgentContainerProvider> = Box::new(provider(rt.clone()));
    let session = agent_provider.open_agent(&spec("run-oa")).await.unwrap();

    // The one-shot compatibility seam also launches the agent through exec; it does
    // not return PID 1 as though the environment were the attempt process.
    assert_eq!(session.process.id(), "exec-0");
    assert_eq!(
        session.process.poll().await.unwrap(),
        Some(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        })
    );
    // The durable handle carries the container id for reattach.
    assert_eq!(session.handle.provider_kind(), "container");
    let payload = session.handle.container_payload().unwrap();
    assert_eq!(payload.container_id, "cid-run-oa");
    // Durable-handle decision rule H1: C1=a realized container has mounted
    // inputs; C2=the provider is later adopted from only its typed handle.
    // C1 => E1 every checkpoint exclusion is frozen without an opaque `extra`
    // map; C1+C2 => E2 the durable wire round-trip preserves the exact paths.
    // This prevents a replacement Worker from archiving independently owned
    // mounts after the old untyped extension was removed.
    assert_eq!(
        payload.continuation_excluded_paths,
        ["/data/in.txt", "/work"]
    );
    let durable = serde_json::to_vec(&session.handle).unwrap();
    let adopted: pc::SandboxHandle = serde_json::from_slice(&durable).unwrap();
    assert_eq!(
        adopted
            .container_payload()
            .unwrap()
            .continuation_excluded_paths,
        ["/data/in.txt", "/work"]
    );
    // A live duplex channel was opened (the ACP bridge would drive it).
    let _channel = session.channel;
    // The physical container is an environment keepalive, not the attempt agent.
    assert_eq!(
        rt.st.lock().unwrap().created_command.get("cid-run-oa"),
        Some(&environment_keepalive_command())
    );
    assert_eq!(
        rt.st.lock().unwrap().spawned[0].1,
        ["claude", "--acp"].map(str::to_string)
    );
}

#[tokio::test]
async fn one_container_environment_executes_native_and_agent_processes_without_recreation() {
    // Cause/effect decision table — RP1:
    // C1: native exec or C2: opaque Agent/Hand exec enters a ContainerSandbox;
    // C3: the caller omits runtime paths or C4: attempts stale replacements;
    // C5: an opaque process selects writable homes beneath /workspace.
    // Rules (C1|C2)+(C3|C4) => E1 both processes receive /workspace and the
    // sandbox's exact output boundary, E2 caller values cannot override runtime
    // ownership, E3 the environment is still created only once, and C5 => E4
    // the process-scoped homes survive without changing project/output paths.
    let runtime = Arc::new(FakeRuntime::default());
    let sandbox = provider(runtime.clone())
        .create_container(&spec("shared-session"))
        .await
        .unwrap();

    let mut native_command = pc::Command::new(["sh", "-c", "touch marker"]);
    native_command.env.extend([
        pc::EnvVar {
            name: "AWAKEN_PROJECT_DIR".into(),
            value: pc::EnvValue::Inline {
                value: "/stale-workspace".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "AWAKEN_OUTPUTS_DIR".into(),
            value: pc::EnvValue::Inline {
                value: "/stale-outputs".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/root".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "XDG_CONFIG_HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/root/.config".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "XDG_CACHE_HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/root/.cache".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
    ]);
    let native = pc::Sandbox::spawn(&sandbox, native_command).await.unwrap();
    let mut agent_command = pc::Command {
        stdio: pc::Stdio::Piped,
        ..pc::Command::new(["opaque-agent", "--stdio"])
    };
    agent_command.env.extend([
        pc::EnvVar {
            name: "HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/workspace/.agent-home".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "XDG_CONFIG_HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/workspace/.agent-home/config".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
        pc::EnvVar {
            name: "XDG_CACHE_HOME".into(),
            value: pc::EnvValue::Inline {
                value: "/workspace/.agent-home/cache".into(),
            },
            visibility: pc::EnvVisibility::Process,
        },
    ]);
    let agent = sandbox.spawn_agent(agent_command).await.unwrap();

    assert_eq!(native.id(), "exec-0");
    assert_eq!(agent.process.id(), "exec-1");
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.created_command.len(), 1, "environment created once");
    assert_eq!(state.spawned.len(), 2);
    assert!(
        state
            .spawned
            .iter()
            .all(|(container, _)| container == "cid-shared-session")
    );
    assert_eq!(
        state.spawned[0].1,
        ["sh", "-c", "touch marker"].map(str::to_string)
    );
    assert_eq!(
        state.spawned[1].1,
        ["opaque-agent", "--stdio"].map(str::to_string)
    );
    assert_eq!(
        state.runtime_path_observations,
        vec![
            (
                Some("/workspace".into()),
                Some("/mnt/session/outputs".into()),
                Some("/workspace".into()),
                Some("/workspace/.config".into()),
                Some("/workspace/.cache".into())
            ),
            (
                Some("/workspace".into()),
                Some("/mnt/session/outputs".into()),
                Some("/workspace/.agent-home".into()),
                Some("/workspace/.agent-home/config".into()),
                Some("/workspace/.agent-home/cache".into())
            ),
        ],
        "native and agent processes share the runtime-owned paths"
    );
    assert_eq!(state.alive.get("cid-shared-session"), Some(&true));
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
        environment: None,
        command: vec!["agent".into()],
        deny_tool_egress: false,
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
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
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
async fn remote_runtime_harvests_live_credential_when_signalled_attempt_finishes_session() {
    let refreshed = br#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"rotated"}}"#;
    let rt = Arc::new(FakeRuntime::default().with_live_credential(refreshed));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"claudeAiOauth":{"accessToken":"old"}}"#.to_vec();
    let provider =
        ContainerProvider::new(rt.clone(), "agent:latest").with_secret_broker(broker.clone());
    let session = provider
        .open_agent(&writable_credential_spec("remote-credential-refresh"))
        .await
        .unwrap();
    session.process.signal(pc::Signal::Term).await.unwrap();
    session.process.wait().await.unwrap();
    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[refreshed.to_vec()]
    );
}

#[tokio::test]
async fn failed_remote_credential_harvest_blocks_writeback_and_environment_removal() {
    // Environment-disposal FMECA graph. C1 a durable writable credential exists;
    // C2 the remote runtime proves a successful read; C3 the broker accepts the
    // replacement; C4 environment removal follows. Effects: E1 one write-back then
    // removal; E2 any C2/C3 failure returns an error, records no authoritative write,
    // and preserves the environment for a retry. Rules: D1 C1+C2+C3=>E1 (covered by
    // the preceding success test); D2 C1+!C2=>E2; D3 C1+C2+!C3=>E2.
    let runtime = Arc::new(FakeRuntime::default().failing_live_credential("remote cat failed"));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"authoritative-old".to_vec();
    let provider =
        ContainerProvider::new(runtime.clone(), "agent:latest").with_secret_broker(broker.clone());
    let sandbox = provider
        .create(&writable_credential_spec(
            "failed-remote-credential-harvest",
        ))
        .await
        .unwrap();
    assert!(
        sandbox.dispose().await.is_err(),
        "D2 read failure propagates"
    );
    assert!(
        broker.writes.lock().unwrap().is_empty(),
        "D2 no stale write"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-failed-remote-credential-harvest"),
        Some(&true),
        "D2 preserve the environment until credential harvest can retry"
    );

    let runtime = Arc::new(FakeRuntime::default().with_live_credential(b"rotated"));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"authoritative-old".to_vec();
    broker.reject_writeback.store(true, Ordering::SeqCst);
    let provider =
        ContainerProvider::new(runtime.clone(), "agent:latest").with_secret_broker(broker.clone());
    let sandbox = provider
        .create(&writable_credential_spec("rejected-credential-writeback"))
        .await
        .unwrap();
    assert!(
        sandbox.dispose().await.is_err(),
        "D3 broker rejection propagates"
    );
    assert!(
        broker.writes.lock().unwrap().is_empty(),
        "D3 no partial write"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-rejected-credential-writeback"),
        Some(&true),
        "D3 preserve the environment until broker recovery"
    );
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
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: vec![pc::MountRequirement {
            mount_id: "f".into(),
            source,
            mount_path: "/data/f.txt".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/out".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
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
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), b"resolved-file-bytes".to_vec());

    let guard = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false)
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

#[cfg(unix)]
#[tokio::test]
async fn resolve_and_stage_makes_declared_read_write_content_writable_by_container_uid() {
    use std::os::unix::fs::PermissionsExt;

    let mut spec = file_mount_spec(
        "res-writable",
        pc::MountSource::File {
            file_id: "blob-rw".into(),
            content_hash: None,
        },
        true,
    );
    spec.mounts[0].access = pc::MountAccess::ReadWrite;
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut seed = HashMap::new();
    seed.insert("blob-rw".to_string(), b"writable".to_vec());

    let _guard = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false)
        .await
        .expect("resolve writable mount");
    let mode = std::fs::metadata(&plan.binds[0].source_ref)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o666);
    assert!(!plan.binds[0].read_only);
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
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let store: Option<std::sync::Arc<dyn pc::BlobSource>> = Some(std::sync::Arc::new(OneBlob(
        "res-9",
        b"from-the-store".to_vec(),
    )));

    resolve_and_stage(
        &spec,
        &mut plan.binds,
        &HashMap::new(),
        &store,
        &None,
        false,
    )
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
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let e = resolve_and_stage(&spec, &mut plan.binds, &HashMap::new(), &None, &None, false)
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
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), b"whatever".to_vec());
    let e = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false)
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
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let mut seed = HashMap::new();
    seed.insert("blob-1".to_string(), bytes);
    resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false)
        .await
        .expect("a matching pin resolves");
    assert_eq!(plan.binds[0].content.as_deref(), Some("pinned-bytes"));
}

#[tokio::test]
async fn inline_bytes_are_staged_binary_safe_and_hash_verified() {
    let bytes = vec![0, 0xff, 0x80, b'\n'];
    let spec = file_mount_spec(
        "inline-binary",
        pc::MountSource::InlineBytes {
            contents: bytes.clone(),
            content_hash: Some(content_fingerprint(&bytes)),
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();

    let staged = resolve_and_stage(&spec, &mut plan.binds, &HashMap::new(), &None, &None, false)
        .await
        .expect("matching binary content stages");

    assert!(staged.guard.is_some());
    assert_eq!(plan.binds[0].content, None);
    assert_eq!(
        plan.binds[0].content_bytes.as_deref(),
        Some(bytes.as_slice())
    );
    assert_eq!(std::fs::read(&plan.binds[0].source_ref).unwrap(), bytes);
}

#[tokio::test]
async fn inline_bytes_hash_mismatch_fails_before_container_start() {
    let spec = file_mount_spec(
        "inline-binary-corrupt",
        pc::MountSource::InlineBytes {
            contents: vec![0, 0xff],
            content_hash: Some("wrong".into()),
        },
        true,
    );
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();

    let error = resolve_and_stage(&spec, &mut plan.binds, &HashMap::new(), &None, &None, false)
        .await
        .expect_err("corrupt binary content must fail closed");

    assert!(error.to_string().contains("hash mismatch"), "{error}");
}
