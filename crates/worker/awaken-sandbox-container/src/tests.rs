// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
// awaken-allow(file-limit-ceiling): 7619 (legacy oversized test file: split, never grow)
//! Slice 5 coverage: pure planners (container/pod) and the provider lifecycle over
//! an in-memory fake [`ContainerRuntime`] — Session environment plus attempt execs,
//! no daemon required.

use super::*;
use crate::test_support::{disposal_authorization, spec};
use awaken_agent_channel::AgentChannel;
use awaken_container_contract::{LIVE_INPUTS_ROOT, RuntimeError};
use awaken_provisioning_contract::test_support::FenceTamper as _;
use awaken_provisioning_contract::{
    SandboxAdoption, SandboxCheckpointAccess, SandboxCheckpointMutation, SandboxEffectFence,
    SandboxObservationPurpose, SessionSandbox,
};
use awaken_provisioning_contract::{SandboxProvider, SandboxReadiness};
use std::collections::HashMap;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::io::AsyncWriteExt;

mod effect_gate_window;
mod memory_reservation;
mod terminal_missing_handle;
mod terminal_preparation;

fn add_future_restoration(handle: &pc::SandboxHandle) -> pc::SandboxHandle {
    let mut wire = serde_json::to_value(handle).unwrap();
    wire.as_object_mut().unwrap().insert(
        "restoration".into(),
        serde_json::json!({
            "effect_id": "effect-a",
            "generation_id": "generation-a",
            "checkpoint_id": "checkpoint-a",
            "checkpoint_digest": "sha256:digest-a",
            "sandbox_spec_fingerprint": "spec-a",
            "checkpoint_exclusions_fingerprint": "exclusions-a"
        }),
    );
    serde_json::from_value(wire).unwrap()
}

fn future_host_bind_handle() -> pc::SandboxHandle {
    serde_json::from_value(serde_json::json!({
        "sandbox_id": "future-container",
        "restoration": {
            "effect_id": "effect-a",
            "generation_id": "generation-a",
            "checkpoint_id": "checkpoint-a",
            "checkpoint_digest": "sha256:digest-a",
            "sandbox_spec_fingerprint": "spec-a",
            "checkpoint_exclusions_fingerprint": "exclusions-a"
        },
        "payload": {
            "schema": "container_v1",
            "container_id": "future-container",
            "outputs_path": "/outputs",
            "base_env": [],
            "live_input_projection": false,
            "continuation_excluded_paths": [],
            "runtime_handle": {
                "kind": "host_bind_restoration",
                "staging_root": "/provider/staging/a"
            }
        }
    }))
    .unwrap()
}

// ── Pure planners ─────────────────────────────────────────────────────────────

#[test]
fn container_capabilities_are_the_strongest_tier() {
    let c = container_capabilities(true, false, false, false, true, Default::default());
    assert_eq!(c.isolation, pc::IsolationClass::Container);
    assert!(c.tool_transparent && c.enforced_readonly && c.network_isolation);
    assert!(c.resource_limits && c.custom_rootfs);
    assert!(!c.enforced_network_allowlist);
    assert!(
        !c.secret_egress_substitution,
        "an unconfigured container provider cannot advertise substitution"
    );
}

/* Host-bind restore-locator table. C1=daemon-observed provider staging path;
 * C2=process-local wrapper drops before Session CAS; C3=locator escapes the
 * provider temp namespace; C4=Host reboot already removed the directory.
 * E1=retain physical binds across C2; E2=explicit terminal removal succeeds
 * idempotently; E3=reject before filesystem deletion. Rules: H1 C1+C2=>E1;
 * H2 C1+terminal=>E2; H3 C3=>E3; H4 C1+C4+terminal=>E2. */
#[test]
fn restored_host_staging_survives_wrapper_drop_and_only_terminally_removes() {
    let mut guard = None;
    let path = staging_dir(&mut guard, "restore-staging-test").unwrap();
    std::fs::write(path.join("physical"), b"retained").unwrap();
    drop(guard);
    assert!(!path.exists(), "ordinary staging still cleans on drop");

    let mut guard = None;
    let path = staging_dir(&mut guard, "restore-staging-test").unwrap();
    std::fs::write(path.join("physical"), b"retained").unwrap();
    let handle = crate::ContainerContinuationHandle::HostBindRestoration(
        crate::HostBindRestorationHandle::for_restore(path.to_string_lossy().into_owned()).unwrap(),
    );
    let retained = retained_host_staging(Some(&handle)).unwrap().unwrap();
    drop(retained);
    assert_eq!(
        std::fs::read(path.join("physical")).unwrap(),
        b"retained",
        "H1/E1"
    );
    remove_host_staging_path(&path).unwrap();
    assert!(!path.exists(), "H2/E2");
    remove_host_staging_path(&path).expect("H4 missing after Host restart is idempotent");

    let escaped_path = std::env::temp_dir().join("not-an-awaken-stage");
    let escaped = crate::ContainerContinuationHandle::HostBindRestoration(
        crate::HostBindRestorationHandle::for_restore(escaped_path.to_string_lossy().into_owned())
            .unwrap(),
    );
    assert!(retained_host_staging(Some(&escaped)).is_err(), "H3/E3");
    assert!(remove_host_staging_path(&escaped_path).is_err(), "H3/E3");
}

#[test]
fn restore_plan_fingerprint_normalizes_only_provider_derived_realization_fields() {
    /* Immutable-plan rules. C1 the frozen package demand is present; C2 package
     * resolution replaces only its image; C3 Docker/Podman add the sole managed
     * live-input host bind; C4 a security-bearing command/network field changes.
     * E1 C1 and C1+C2+C3 share one read-first fingerprint; E2 C4 changes it.
     * Rules PF1=C1=>E1; PF2=C1+C2+C3=>E1; PF3=C1+C4=>E2. */
    let mut restore_spec = spec("restore-plan-fingerprint");
    restore_spec.packages.managers.insert(
        "npm".into(),
        vec!["@modelcontextprotocol/server-filesystem@1.0.0".into()],
    );
    let pre_resolution = container_plan(
        &restore_spec,
        "ghcr.io/awaken/sandbox:base",
        &environment_keepalive_command(),
        None,
    )
    .unwrap();
    let frozen = restoration_plan_fingerprint(&pre_resolution);

    let mut realized = pre_resolution.clone();
    realized.image = "registry.internal/awaken-packages@sha256:resolved".into();
    realized.rootfs = RootfsPlan::Image(realized.image.clone());
    realized.binds.push(BindPlan {
        source_ref: "/tmp/awaken-acp-stage-provider/live-inputs".into(),
        mount_path: LIVE_INPUTS_ROOT.into(),
        read_only: false,
        content: None,
        content_bytes: None,
        secret_content: None,
        secret_writeback: false,
        credential_file_path: None,
    });
    assert_eq!(restoration_plan_fingerprint(&realized), frozen, "PF2/E1");

    realized.command.push("unexpected".into());
    assert_ne!(restoration_plan_fingerprint(&realized), frozen, "PF3/E2");
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
            &container_capabilities(true, false, false, false, true, Default::default()),
        ),
        Err(pc::PrepareError::EgressSecretUnsupported("API_KEY".into())),
        "an unsupported provider must fail before materializing or launching the sandbox"
    );
}

#[test]
fn container_runtime_without_package_builder_rejects_before_launch() {
    let mut requested = spec("packages");
    requested.packages = awaken_environment_contract::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    assert_eq!(
        pc::prepare_environment(
            &requested,
            &container_capabilities(true, false, false, false, true, Default::default()),
        ),
        Err(pc::PrepareError::PackageProvisioningUnsupported),
        "Kubernetes and out-of-tree runtimes without immutable image builds must fail before a workload exists"
    );
}

#[derive(Default)]
struct RecordingPackageProvisioner {
    calls: Mutex<
        Vec<(
            String,
            awaken_environment_contract::PackageRequirements,
            pc::NetworkPolicy,
        )>,
    >,
    response_gate: Option<Arc<tokio::sync::Semaphore>>,
    unreachable: bool,
}

#[async_trait]
impl PackageImageProvisioner for RecordingPackageProvisioner {
    async fn probe_builder(&self) -> Result<(), RuntimeError> {
        if self.unreachable {
            return Err(RuntimeError::Backend(
                "package builder is unreachable".into(),
            ));
        }
        Ok(())
    }

    async fn prepare_package_image(
        &self,
        base_image: &str,
        packages: &awaken_environment_contract::PackageRequirements,
        network: &pc::NetworkPolicy,
    ) -> Result<String, RuntimeError> {
        self.calls.lock().unwrap().push((
            base_image.to_string(),
            packages.clone(),
            network.clone(),
        ));
        if let Some(gate) = &self.response_gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok("registry.test/awaken-packages@sha256:prepared".into())
    }
}

#[tokio::test]
async fn startup_attestation_withholds_an_unreachable_package_builder() {
    // ADR-0092 decision 5. Causes: C1 the external builder answers or not.
    // Effects: E1 package provisioning stays advertised; E2 it is withheld
    // once attested (a non-building engine has no other builder); the engine's
    // own evidence (limits) is unaffected by the builder.
    for (unreachable, advertised) in [(false, true), (true, false)] {
        let builder = Arc::new(RecordingPackageProvisioner {
            unreachable,
            ..Default::default()
        });
        let provider = provider(Arc::new(FakeRuntime::default())).with_package_provisioner(builder);
        let evidence = provider.attest_startup().await.expect("the engine answers");
        assert_eq!(evidence.package_builder, !unreachable, "builder evidence");
        let capabilities = provider.capabilities();
        assert_eq!(capabilities.package_provisioning, advertised, "E1/E2");
        assert!(capabilities.resource_limits, "engine evidence unaffected");
    }
}

#[tokio::test]
async fn an_external_package_provisioner_enables_a_non_building_runtime() {
    let runtime = Arc::new(FakeRuntime::default());
    let builder = Arc::new(RecordingPackageProvisioner::default());
    let provider = provider(runtime.clone()).with_package_provisioner(builder.clone());
    assert!(provider.capabilities().package_provisioning);

    let mut requested = spec("external-package-builder");
    requested.packages = awaken_environment_contract::PackageRequirements {
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

/// P1 every rootfs/command has one init, non-root identity, no new privileges,
/// capability drop and read-only root. P2 only Open retains network; None and
/// unresolved Allowlist deny it. P3 reservation is the same projection with
/// create replacing run, so it cannot omit a floor used by activation.
#[test]
fn podman_execution_and_reservation_share_the_security_floor() {
    let mut plan = container_plan(&spec("floor"), "busybox:latest", &["sh".into()], None).unwrap();
    for network in [NetworkMode::Open, NetworkMode::None, NetworkMode::Allowlist] {
        plan.network = network.clone();
        let run = podman_run_argv("floor", &plan, &plan.rootfs);
        for flag in [
            "--init",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
        ] {
            assert_eq!(
                run.iter().filter(|argument| *argument == flag).count(),
                1,
                "P1 {flag}"
            );
        }
        assert!(
            run.windows(2)
                .any(|arguments| arguments == ["--user", "10001:10001"]),
            "P1 image identity"
        );
        assert_eq!(
            run.windows(2)
                .any(|arguments| arguments == ["--network", "none"]),
            network != NetworkMode::Open,
            "P2"
        );
        let created = crate::podman_plan::podman_create_argv("floor", &plan, &plan.rootfs);
        let mut expected = run;
        expected[0] = "create".into();
        expected.remove(1);
        assert_eq!(created, expected, "P3");
    }
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
    /* Memory layout cause/effect table — MP1. C1 one MemoryStore carries a
     * logical store id; C2 its effect-scoped materialization reference is
     * absent/present. C1 => E1 the byte-bind projection excludes the mount and
     * E2 the runtime plan retains the logical store id/path/access. C1+C2 => E3
     * first materialization uses the reference while durable recovery evidence
     * retains the logical store id through the same normalized projection
     * (covered by NM1/CN1), without changing E2. */
    let mut s = spec("mem");
    s.mounts.push(pc::MountRequirement {
        mount_id: "notes".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-42".into(),
            materialization_reference: Some("effect-reference-42".into()),
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
        fail_teardown: false,
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
    sandbox.dispose_unattached().await.unwrap();
    assert!(torn_down.load(Ordering::SeqCst));
}

struct UncertainMemoryMounter {
    mount_calls: Arc<AtomicUsize>,
    recovery_calls: Arc<AtomicUsize>,
    recovery_failures_remaining: Arc<AtomicUsize>,
    recovery_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    mount_paths: Arc<Mutex<Vec<std::path::PathBuf>>>,
    recovery_paths: Arc<Mutex<Vec<std::path::PathBuf>>>,
    mount_after_recovery: Option<Arc<AtomicBool>>,
    torn_down: Arc<AtomicBool>,
}

#[async_trait]
impl pc::MemoryMounter for UncertainMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        self.mount_calls.fetch_add(1, Ordering::SeqCst);
        self.mount_paths.lock().unwrap().push(host_path.to_owned());
        if self
            .mount_after_recovery
            .as_ref()
            .is_some_and(|ready| ready.load(Ordering::SeqCst))
        {
            std::fs::create_dir_all(host_path).unwrap();
            std::fs::write(host_path.join("seed.txt"), b"seed").unwrap();
            return pc::MemoryMountOutcome::Mounted(Box::new(FakeMemoryMount {
                torn_down: self.torn_down.clone(),
                fail_teardown: false,
                teardown_calls: Arc::new(AtomicUsize::new(0)),
                response_gate: None,
                realization: pc::Realization::Copy,
            }));
        }
        pc::MemoryMountOutcome::RecoveryRequired(pc::SandboxError::transport(
            "mount response was lost after a possible effect",
        ))
    }

    async fn recover_unknown_mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.recovery_calls.fetch_add(1, Ordering::SeqCst);
        self.recovery_paths
            .lock()
            .unwrap()
            .push(host_path.to_owned());
        let remaining = self.recovery_failures_remaining.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |value| value.checked_sub(1),
        );
        if remaining.is_ok() {
            Err(pc::SandboxError::transport(
                "mount recovery remains unavailable",
            ))
        } else {
            if let Some(gate) = &self.recovery_response_gate {
                gate.acquire().await.unwrap().forget();
            }
            if let Some(ready) = &self.mount_after_recovery {
                ready.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
    }

    async fn reconcile_recovered_copy(
        &self,
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "recovered Memory copy reconciliation is outside this fixture",
        ))
    }
}

#[tokio::test]
async fn uncertain_memory_staging_blocks_remount_until_exact_recovery() {
    // Cause/effect graph: C1 mount returns RecoveryRequired; C2 exact recovery
    // fails or succeeds; C3 the same provider receives another create. Effects:
    // E1 the first failure retains operation identity; E2 C3 cannot remount
    // while C2 fails; E3 successful recovery retires the retained operation and
    // only then permits a fresh mount; E4 the fresh unknown is compensated by
    // its own identity. Rules U1=C1+C2(fail)=>E1;
    // U2=U1+C3+C2(fail)=>E2; U3=U2+C3+C2(success)=>E3+E4.
    // The backend reservation, not a process-local ledger, supplies the exact
    // attempt identity and path for every retry.
    let runtime = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            recover_prepared_reservations: true,
            host_live_input_bind: true,
            ..Default::default()
        }),
    });
    let mut sandbox_spec = spec("uncertain-memory-stage");
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "uncertain-store".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/uncertain".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let provider = provider(runtime);
    let mount_calls = Arc::new(AtomicUsize::new(0));
    let recovery_calls = Arc::new(AtomicUsize::new(0));
    let recovery_failures_remaining = Arc::new(AtomicUsize::new(2));
    let mount_paths = Arc::new(Mutex::new(Vec::new()));
    let recovery_paths = Arc::new(Mutex::new(Vec::new()));
    provider.install_memory_mounter(Arc::new(UncertainMemoryMounter {
        mount_calls: mount_calls.clone(),
        recovery_calls: recovery_calls.clone(),
        recovery_failures_remaining,
        recovery_response_gate: None,
        mount_paths: mount_paths.clone(),
        recovery_paths: recovery_paths.clone(),
        mount_after_recovery: None,
        torn_down: Arc::new(AtomicBool::new(false)),
    }));

    assert!(provider.create(&sandbox_spec).await.is_err(), "U1/E1");
    assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "U1/E1");
    assert_eq!(recovery_calls.load(Ordering::SeqCst), 1, "U1/E1");

    assert!(provider.create(&sandbox_spec).await.is_err(), "U2/E2");
    assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "U2/E2");
    assert_eq!(recovery_calls.load(Ordering::SeqCst), 2, "U2/E2");

    assert!(provider.create(&sandbox_spec).await.is_err(), "U3/E3+E4");
    assert_eq!(mount_calls.load(Ordering::SeqCst), 2, "U3/E3");
    assert_eq!(recovery_calls.load(Ordering::SeqCst), 4, "U3/E4");
    let original_path = mount_paths.lock().unwrap()[0].clone();
    assert!(
        mount_paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| path == &original_path),
        "U1-U3 use the backend-bound root after process-local root replacement"
    );
    assert!(
        recovery_paths
            .lock()
            .unwrap()
            .iter()
            .all(|path| path == &original_path),
        "U1-U3 recover only the exact backend-bound participant"
    );
}

fn activation_memory_spec(scope: &str) -> pc::SandboxSpec {
    let mut sandbox_spec = spec(scope);
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "durable-copy".into(),
            materialization_reference: Some("claim-copy".into()),
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    sandbox_spec
}

#[tokio::test]
async fn activation_response_loss_recovers_copy_from_the_backend_bound_root() {
    /* Container Memory decision table CM1. Causes: C1 the exact backend
     * reservation is fresh/recovered/active; C2 Copy mount succeeds; C3
     * activation response arrives/is lost; C4 the Worker process-local Provider
     * is replaced. Effects: R1 fresh+C2 reserves before mount and activates
     * after the recovery-receipt commit; R2 fresh+C2+C3(loss) publishes no Sandbox and
     * retains the backend-bound root; R3 R2+C4+active reads the exact receipt,
     * performs no remount/compensation, and returns the same locator/evidence.
     * A recovering-but-not-active reservation instead invokes exact
     * `recover_unknown_mount` before a new mount (U1-U3). */
    let scope = "activation-loss-copy";
    let runtime = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            recover_prepared_reservations: true,
            host_live_input_bind: true,
            activation_failure: Some(ActivationFailure::AfterEffect(
                RuntimeError::MayHaveCommitted("injected activation response loss".into()),
            )),
            ..Default::default()
        }),
    });
    let sandbox_spec = activation_memory_spec(scope);
    let mount_calls = Arc::new(AtomicUsize::new(0));
    let recovery_calls = Arc::new(AtomicUsize::new(0));
    let teardown_calls = Arc::new(AtomicUsize::new(0));
    let mounter = Arc::new(OrderedCopyMounter {
        runtime: runtime.clone(),
        scope: scope.into(),
        mount_calls: mount_calls.clone(),
        recovery_calls: recovery_calls.clone(),
        teardown_calls: teardown_calls.clone(),
    });
    let first = provider(runtime.clone());
    first.install_memory_mounter(mounter.clone());
    let error = match first.create_container(&sandbox_spec).await {
        Ok(_) => panic!("R2 response loss cannot publish a Sandbox"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("may have committed"), "R2");
    assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "R1/R2");
    assert_eq!(runtime.st.lock().unwrap().activation_calls, 1, "R1/R2");
    assert_eq!(
        teardown_calls.load(Ordering::SeqCst),
        0,
        "R2 original Memory retained"
    );
    drop(first);

    let recovered_root = runtime
        .st
        .lock()
        .unwrap()
        .recovered_staging
        .clone()
        .unwrap();
    assert!(
        recovered_root
            .join(crate::staged_mounts::MEMORY_PRE_ACTIVATION_RECEIPT)
            .is_file(),
        "R2 pre-activation recovery receipt"
    );
    let second = provider(runtime.clone());
    second.install_memory_mounter(mounter);
    let sandbox = second.create_container(&sandbox_spec).await.expect("R3");
    assert_eq!(sandbox.container_id, format!("cid-{scope}"), "R3 locator");
    assert_eq!(mount_calls.load(Ordering::SeqCst), 1, "R3 no remount");
    assert_eq!(
        recovery_calls.load(Ordering::SeqCst),
        0,
        "R3 no compensation"
    );
    assert_eq!(
        pc::Sandbox::handle(&sandbox)
            .memory_materializations()
            .unwrap()
            .unwrap()
            .len(),
        1,
        "R3 durable heads"
    );
}

#[tokio::test]
async fn container_write_through_memory_is_rejected_before_physical_reservation() {
    /* Container consistency table CW1. C1 Memory requests ProviderDefault or
     * WriteThroughRequired; C2 no crash-recoverable FUSE participant-transfer
     * contract exists. ProviderDefault may select Copy; C1(write-through)+C2
     * rejects before preflight, prepare, mount, or staging. */
    let runtime = Arc::new(FakeRuntime::default());
    let mut sandbox_spec = spec("write-through-rejected");
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "write-through".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::WriteThroughRequired,
        },
        mount_path: "/workspace/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let provider = provider(runtime.clone());
    let torn_down = Arc::new(AtomicBool::new(false));
    provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
        fail_teardown: false,
    }));
    assert!(provider.create_container(&sandbox_spec).await.is_err());
    assert!(runtime.st.lock().unwrap().attempted_binds.is_empty());
    assert!(!torn_down.load(Ordering::SeqCst));
}

#[tokio::test]
async fn recovered_native_reservation_without_its_exact_memory_root_fails_closed() {
    /* Container root-identity table CR1. C1 the backend reports a prepared
     * reservation; C2 the authority-derived Memory root exists/does not exist;
     * C3 a process-local Provider has no participant. C1+C2(missing)+C3 must
     * reject before mount or activation. Creating a new directory would erase
     * the distinction between lost responsibility and a fresh attempt. */
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scope = format!("missing-native-memory-root-{}-{nonce}", std::process::id());
    let attempt = format!("missing-attempt-{}-{nonce}", std::process::id());
    let runtime = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            recover_prepared_reservations: true,
            native_memory: true,
            prepared_attempts: HashMap::from([(scope.clone(), attempt)]),
            ..Default::default()
        }),
    });
    let mut sandbox_spec = spec(&scope);
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "missing-root-store".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let provider = provider(runtime.clone());
    let torn_down = Arc::new(AtomicBool::new(false));
    provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
        fail_teardown: false,
    }));
    let error = match provider.create_container(&sandbox_spec).await {
        Ok(_) => panic!("missing root cannot become a fresh Memory attempt"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("retained host staging path is missing")
    );
    assert_eq!(runtime.st.lock().unwrap().activation_calls, 0);
    assert!(!torn_down.load(Ordering::SeqCst));
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

type ReconciledMemoryEntries = Arc<Mutex<Option<Vec<(String, Vec<u8>)>>>>;

struct HarvestingMemoryMounter {
    reference: Arc<Mutex<Option<String>>>,
    harvested: Arc<Mutex<Option<Vec<u8>>>>,
    recovered_evidence: Arc<Mutex<Option<pc::MemoryMaterializationEvidence>>>,
    reconciled: ReconciledMemoryEntries,
    reconcile_calls: Arc<AtomicUsize>,
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
    ) -> pc::MemoryMountOutcome {
        let result: Result<Box<dyn pc::MemoryMount>, pc::SandboxError> = (|| {
            *self.reference.lock().unwrap() = Some(store_id.to_owned());
            std::fs::create_dir_all(host_path)
                .map_err(|error| pc::SandboxError::backend("mount", error))?;
            std::fs::write(host_path.join("seed.txt"), b"seed")
                .map_err(|error| pc::SandboxError::backend("mount", error))?;
            Ok(Box::new(HarvestingMemoryMount {
                root: host_path.to_path_buf(),
                harvested: self.harvested.clone(),
            }) as Box<dyn pc::MemoryMount>)
        })();
        pc::MemoryMountOutcome::from_no_effect(result)
    }

    async fn recover_unknown_mount(
        &self,
        _: &str,
        _: &std::path::Path,
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        store_id: &str,
        evidence: &pc::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        _access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.reconcile_calls.fetch_add(1, Ordering::SeqCst);
        *self.reference.lock().unwrap() = Some(store_id.to_owned());
        *self.recovered_evidence.lock().unwrap() = Some(evidence.clone());
        *self.reconciled.lock().unwrap() = Some(files.to_vec());
        Ok(())
    }
}

#[async_trait]
impl pc::MemoryMount for HarvestingMemoryMount {
    fn realization(&self) -> pc::Realization {
        pc::Realization::Copy
    }

    fn materialization_heads(&self) -> Option<Vec<pc::MemoryMaterializationHead>> {
        Some(vec![pc::MemoryMaterializationHead {
            path: "seed.txt".into(),
            id: "seed-head".into(),
            version: 1,
            content_sha256: "seed-sha256".into(),
        }])
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        *self.harvested.lock().unwrap() = std::fs::read(self.root.join("changed.txt")).ok();
        Ok(())
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
    let recovered_evidence = Arc::new(Mutex::new(None));
    let reconciled = Arc::new(Mutex::new(None));
    let reconcile_calls = Arc::new(AtomicUsize::new(0));
    provider.install_memory_mounter(Arc::new(HarvestingMemoryMounter {
        reference: reference.clone(),
        harvested: harvested.clone(),
        recovered_evidence,
        reconciled,
        reconcile_calls,
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

    sandbox.dispose_unattached().await.expect("NM1 dispose");
    assert_eq!(
        harvested.lock().unwrap().as_deref(),
        Some(b"changed-in-pod".as_slice()),
        "E2 same mounter harvest"
    );
}

#[tokio::test]
async fn fenced_container_disposal_requires_exact_terminal_memory_ack() {
    use pc::Sandbox as _;
    /* Container disposal preparation/authorization table NM2. Causes: C1 a
     * fresh handle carries exact copy materialization M and a writable Secret;
     * C2 aggregate fence F is live; C3 acknowledgement is missing/exact/foreign;
     * C4 the exact acknowledgement/preparation response is lost and replayed;
     * C5 Secret writeback fails after a continuation ack; C6 a terminal successor
     * has the same/different realization lease; C7 aggregate preparation is/is
     * not durably accepted before physical authorization. Effects: E1 missing or
     * foreign acknowledgement fails before Secret writeback or runtime removal;
     * E2 exact M+F drains only the process-local CopyMount guard without RunV1
     * teardown; E3 exact ack/preparation replay is idempotent; E4 Secret failure
     * retains the realization before the cleanup gate; E5 a same-lease successor
     * rebinds drained acknowledgement with zero Memory I/O, while a foreign lease
     * remains rejected; E6 prepare writes the stable physical/ref Secret effect
     * but performs zero removal; E7 only C7 invokes exact runtime removal, with
     * zero repeated live credential read. Staging and non-copy dependencies stay
     * owned until removal succeeds. C8 Container/Kubernetes has not installed a
     * physical cleanup gate at preparation; E8 its returned predecessor is the
     * exact input fence and does not claim provider-marker durability.
     *
     * | Rule | M/F/ack | Secret prepare | prep durable | Effect |
     * |---|---|---|---|---|
     * | N1 | missing/foreign | not called | no | E1 |
     * | N2 | exact/replay | pending | no | E2+E3 |
     * | N3 | exact | fails | no | E4 |
     * | N4 | same lease successor | succeeds/replays | no | E5+E6+E8 |
     * | N5 | foreign/superseded | not called | no | E1 |
     * | N6 | exact latest | already durable | yes | E7 |
     *
     * N3-N6 are the canonical equivalent coverage for the retired one-shot
     * process-exit Secret tests: successful and failed writeback, replay, and
     * exact physical removal are all driven by the aggregate fence. A process
     * terminal status is deliberately absent from the causes because it owns
     * neither source preparation nor Environment disposal.
     * N7 current authority is revoked after an explicit Pending mount-lock
     * poll: reject with Copy guards, ack and Secret state untouched. N8 an
     * already successful ack replay under revoked authority also rejects. */
    let runtime = Arc::new(
        FakeRuntime::default()
            .with_native_memory_archive(Vec::new())
            .with_live_credential(b"rotated-terminal-secret"),
    );
    let mut sandbox_spec = writable_credential_spec("terminal-memory-ack");
    sandbox_spec.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "memory-authority".into(),
            materialization_reference: Some("run-v1-must-not-be-used-at-terminal".into()),
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/workspace/.mnt/memory".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"initial-terminal-secret".to_vec();
    broker.reject_writeback.store(true, Ordering::SeqCst);
    let provider = provider_without_broker(runtime.clone()).with_secret_broker(broker.clone());
    let torn_down = Arc::new(AtomicBool::new(false));
    provider.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
        fail_teardown: false,
    }));
    let sandbox = provider
        .create_container(&sandbox_spec)
        .await
        .expect("NM2 create exact copy-backed Container");
    let handle = sandbox.handle();
    let materializations = handle
        .memory_materializations()
        .expect("NM2 valid V2 handle")
        .expect("NM2 exact copy evidence")
        .to_vec();
    let continuation = pc::SandboxEffectFence::new(
        "continuation-memory-ack-effect",
        "worker-owner",
        "worker-incarnation",
        11,
        u64::MAX,
    )
    .unwrap();

    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &continuation,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation)
            )
            .await
            .is_err(),
        "N1 missing acknowledgement rejects preparation"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-terminal-memory-ack"),
        Some(&true),
        "N1 zero physical removal"
    );

    let mut foreign = materializations.clone();
    foreign[0].store_id = "foreign-memory".into();

    let authority = pc::test_support::EffectAuthority::new(&continuation);
    let memory_locked = sandbox.lifecycle.memory.lock().await;
    let copies_before = memory_locked.as_ref().unwrap().len();
    let mut waiting = Box::pin(sandbox.acknowledge_memory_reconciliation(
        &continuation,
        &materializations,
        &authority,
    ));
    std::future::poll_fn(|cx| {
        assert!(
            std::future::Future::poll(waiting.as_mut(), cx).is_pending(),
            "N7 wait"
        );
        std::task::Poll::Ready(())
    })
    .await;
    *authority.current.lock().unwrap() = None;
    drop(memory_locked);
    assert!(waiting.await.is_err(), "N7 recheck after wait");
    assert_eq!(
        sandbox
            .lifecycle
            .memory
            .lock()
            .await
            .as_ref()
            .unwrap()
            .len(),
        copies_before,
        "N7 Copy retained"
    );
    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &continuation,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation)
            )
            .await
            .is_err(),
        "N7 no ack"
    );
    assert!(
        sandbox
            .acknowledge_memory_reconciliation(
                &continuation,
                &foreign,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation)
            )
            .await
            .is_err(),
        "N2 foreign materialization cannot unlock disposal"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-terminal-memory-ack"),
        Some(&true),
        "N2 zero physical removal"
    );

    sandbox
        .acknowledge_memory_reconciliation(
            &continuation,
            &materializations,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation),
        )
        .await
        .expect("N2 exact continuation reconciliation acknowledgement");
    sandbox
        .acknowledge_memory_reconciliation(
            &continuation,
            &materializations,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation),
        )
        .await
        .expect("N2 response-loss replay is idempotent");
    assert!(
        sandbox
            .acknowledge_memory_reconciliation(&continuation, &materializations, &authority,)
            .await
            .is_err(),
        "N8 cached success does not survive revocation"
    );
    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &continuation,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation)
            )
            .await
            .is_err(),
        "N3 Secret rejection keeps the physical realization"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-terminal-memory-ack"),
        Some(&true),
        "N4 zero physical removal"
    );

    let terminal = pc::SandboxEffectFence::new(
        "terminal-memory-ack-effect",
        "worker-owner",
        "worker-incarnation",
        11,
        u64::MAX,
    )
    .unwrap();
    let foreign_lease = pc::SandboxEffectFence::new(
        "foreign-terminal-memory-ack-effect",
        "foreign-owner",
        "worker-incarnation",
        11,
        u64::MAX,
    )
    .unwrap();
    assert!(
        sandbox
            .acknowledge_memory_reconciliation(
                &foreign_lease,
                &materializations,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&foreign_lease)
            )
            .await
            .is_err(),
        "N6 a foreign realization lease cannot take over drained guards"
    );
    sandbox
        .acknowledge_memory_reconciliation(
            &terminal,
            &materializations,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("N4 same-lease terminal successor rebinds the acknowledgement");
    assert!(
        sandbox
            .prepare_disposal_for_effect(
                &continuation,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&continuation)
            )
            .await
            .is_err(),
        "N5 only the latest authorized successor may prepare"
    );
    broker.reject_writeback.store(false, Ordering::SeqCst);
    let expected_secret_effect = pc::SecretWritebackEffect::new(
        "credential://acp/native/claude",
        handle
            .container_physical_incarnation()
            .expect("N5 current physical incarnation"),
        terminal.clone(),
    )
    .expect("N5 canonical Secret effect");
    let prepared = sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("N4 exact successor prepares every source-dependent effect");
    assert_eq!(prepared, terminal, "N4/E8 exact input predecessor");
    let live_reads_after_prepare = runtime.st.lock().unwrap().live_credential_reads;
    let replayed = sandbox
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("N4 preparation response-loss replay");
    assert_eq!(replayed, terminal, "N4/E8 exact replay predecessor");
    assert_eq!(
        runtime.st.lock().unwrap().live_credential_reads,
        live_reads_after_prepare,
        "N4 successful preparation replay performs no second live read"
    );
    assert_eq!(
        live_reads_after_prepare, 2,
        "N3 failed writeback remains retryable; N4 successful writeback reads once"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-terminal-memory-ack"),
        Some(&true),
        "N4 preparation has zero physical effect"
    );
    sandbox
        .dispose_for_effect(
            &disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("N6 aggregate-authorized physical authorization");
    assert!(
        !torn_down.load(Ordering::SeqCst),
        "N3-N6 terminal-v2 reconciliation is not repeated through RunV1 teardown"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-terminal-memory-ack"),
        Some(&false),
        "N6 physical removal follows durable preparation"
    );
    assert_eq!(
        runtime.st.lock().unwrap().live_credential_reads,
        live_reads_after_prepare,
        "N6 physical authorization performs zero live credential read"
    );
    assert_eq!(
        broker.effect_operations.lock().unwrap().as_slice(),
        std::slice::from_ref(&terminal.operation_id()),
        "N4/N5 failed predecessor writes no Secret; successor writes exactly once"
    );
    assert_eq!(
        broker.writeback_ids.lock().unwrap().as_slice(),
        &[expected_secret_effect.writeback_id().to_string()],
        "N5 Secret idempotency is bound to the exact credential reference and physical Sandbox, not the aggregate operation",
    );
}

#[tokio::test]
async fn physical_disposal_rechecks_current_authority_before_delete_release_and_cached_success() {
    use pc::Sandbox as _;
    use std::time::Duration;
    // C1 Pending participant lock / observation / committed removal response /
    // Memory lock / first or last FUSE teardown; C2 revoke/replace/expire/renew/
    // cancel. Cross the complete 6x5 table with past/future Control timestamps;
    // only the current local proof governs expiry, never Worker UTC. Reuse the
    // real disposal + exact-observation call chain.
    // R1 stale authority denies the NEXT effect and Done, retaining staging and
    // every remaining participant. R2 renewal completes. R3 cancellation keeps
    // the same participants for retry; an already committed delete stays gone.
    // R4 exact retry converges without repeating committed physical deletion.
    // R5 cached Done under revoked authority returns Err with zero I/O.
    // Two FUSE participants isolate physical release from the already accepted
    // source-preparation phase. No fake probe can turn an in-flight backend
    // commit into a claim that cancellation undid it. No wall-time sleeps.
    // R6 every admitted observation, including retry, must be TerminalCleanup:
    // current authority cannot turn Recovery's process absence into proof that
    // retained resources are gone. The backend classifier owns that distinction.
    for (boundary, expiry) in [
        "participant",
        "observation",
        "remove",
        "memory-lock",
        "first-mount",
        "last-mount",
    ]
    .into_iter()
    .flat_map(|boundary| [100, u64::MAX - 1].map(|expiry| (boundary, expiry)))
    {
        for change in ["revoke", "replace", "expire", "renew", "cancel"] {
            let runtime = Arc::new(FakeRuntime::default());
            let sandbox = provider(runtime.clone())
                .create_container(&spec("physical-authority"))
                .await
                .unwrap();
            let prepared =
                pc::SandboxEffectFence::new("prepared", "owner", "boot", 1, expiry).unwrap();
            sandbox
                .prepare_disposal_for_effect(
                    &prepared,
                    &pc::test_support::EffectAuthority::new(&prepared),
                )
                .await
                .unwrap();
            let authorization = disposal_authorization(&prepared);
            let authority = pc::test_support::EffectAuthority::new(authorization.effect_fence());
            let staging = sandbox
                .lifecycle
                .staging
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .path()
                .to_owned();
            let retained = staging.join("synthetic-retained");
            std::fs::write(&retained, b"synthetic-resource").unwrap();
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            let calls = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
            *sandbox.lifecycle.memory.lock().await = Some(
                calls
                    .iter()
                    .enumerate()
                    .map(|(index, calls)| StagedMemoryMount {
                        handle: Box::new(FakeMemoryMount {
                            torn_down: Arc::new(AtomicBool::new(false)),
                            fail_teardown: false,
                            teardown_calls: calls.clone(),
                            realization: pc::Realization::Fuse,
                            response_gate: ((boundary == "first-mount" && index == 0)
                                || (boundary == "last-mount" && index == 1))
                                .then(|| gate.clone()),
                        }),
                        host_path: staging.join(format!("fuse-{index}")),
                        mount_path: format!("/memory/{index}"),
                        access: pc::MountAccess::ReadWrite,
                        materialization: None,
                        native_runtime_volume: false,
                        writeback_prepared: true,
                    })
                    .collect(),
            );
            if boundary == "observation" {
                runtime.st.lock().unwrap().observation_response_gate = Some(gate.clone());
            }
            if boundary == "remove" {
                runtime.st.lock().unwrap().removal_response_gate = Some(gate.clone());
            }
            let participant = if boundary == "participant" {
                Some(sandbox.lifecycle.remove_done.lock().await)
            } else {
                None
            };
            let memory = if boundary == "memory-lock" {
                Some(sandbox.lifecycle.memory.lock().await)
            } else {
                None
            };
            let mut pending = Box::pin(sandbox.dispose_for_effect(&authorization, &authority));
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                    "{boundary}/{change}"
                );
                std::task::Poll::Ready(())
            })
            .await;
            let mut current = authorization.effect_fence().clone();
            current.renew();
            if change == "replace" {
                current.reassign();
            }
            *authority.current.lock().unwrap() = if change == "revoke" {
                None
            } else {
                Some((
                    current,
                    if change == "expire" {
                        Duration::ZERO
                    } else {
                        Duration::from_secs(1)
                    },
                ))
            };
            drop(participant);
            drop(memory);
            gate.add_permits(1);
            if change == "cancel" {
                drop(pending);
            } else {
                assert_eq!(
                    pending.await.is_ok(),
                    change == "renew",
                    "{boundary}/{change}"
                );
            }
            assert_eq!(
                runtime.st.lock().unwrap().removals.len(),
                usize::from(
                    change == "renew" || !matches!(boundary, "participant" | "observation")
                )
            );
            assert_eq!(
                calls[0].load(Ordering::SeqCst),
                usize::from(change == "renew" || matches!(boundary, "first-mount" | "last-mount"))
            );
            assert_eq!(
                calls[1].load(Ordering::SeqCst),
                usize::from(change == "renew" || boundary == "last-mount")
            );
            if change != "renew" {
                assert!(!*sandbox.lifecycle.remove_done.lock().await);
                assert_eq!(
                    sandbox
                        .lifecycle
                        .memory
                        .lock()
                        .await
                        .as_ref()
                        .unwrap()
                        .len(),
                    2
                );
                assert_eq!(std::fs::read(&retained).unwrap(), b"synthetic-resource");
                *authority.current.lock().unwrap() =
                    Some((authorization.effect_fence().clone(), Duration::from_secs(1)));
                sandbox
                    .dispose_for_effect(&authorization, &authority)
                    .await
                    .unwrap();
            }
            assert!(*sandbox.lifecycle.remove_done.lock().await);
            assert!(sandbox.lifecycle.memory.lock().await.is_none());
            assert!(!staging.exists());
            assert_eq!(runtime.st.lock().unwrap().removals.len(), 1);
            let observations = runtime.st.lock().unwrap().observations;
            assert!(observations > 0, "R6 non-vacuous runtime observation");
            assert_eq!(
                runtime.st.lock().unwrap().observation_purposes,
                vec![SandboxObservationPurpose::TerminalCleanup; observations as usize],
                "R6 {boundary}/{change}"
            );
            *authority.current.lock().unwrap() = None;
            assert!(
                sandbox
                    .dispose_for_effect(&authorization, &authority)
                    .await
                    .is_err()
            );
            assert_eq!(
                runtime.st.lock().unwrap().observations,
                observations,
                "R5 no cached-path I/O"
            );
            assert_eq!(runtime.st.lock().unwrap().removals.len(), 1);
        }
    }
}

#[tokio::test]
async fn disposal_preparation_rechecks_current_authority_after_each_credential_wait() {
    use pc::Sandbox as _;
    // C1 authority changes while awaiting writeback lock / live read / committed
    // write response; C2 revoked / same-generation renewal / new generation.
    // R1 revoked or replaced => Err, no NEXT read/write/prepared receipt, and
    // retain the exact environment. R2 renewal => success, one write. R3 a write
    // already committed before revocation may complete; retry uses the same
    // stable writeback ID. This proves admission between waits, NOT remote CAS
    // fencing inside an admitted broker request. Poll Pending before changing
    // authority; R4 caller cancellation at the same waits retains the exact
    // environment and retries with the original writeback identity. The 3x4
    // table uses no sleeps or guessed timing; dropping Pending is not a receipt.
    for boundary in ["lock", "read", "write"] {
        for change in ["revoke", "renew", "replace", "cancel"] {
            let runtime =
                Arc::new(FakeRuntime::default().with_live_credential(b"synthetic-rotated"));
            let broker = Arc::new(RecordingSecretBroker::default());
            *broker.current.lock().unwrap() = b"synthetic-initial".to_vec();
            let sandbox = provider_without_broker(runtime.clone())
                .with_secret_broker(broker.clone())
                .create_container(&writable_credential_spec("preparation-waits"))
                .await
                .unwrap();
            let fence =
                pc::SandboxEffectFence::new("prepare", "owner", "boot", 1, u64::MAX - 1).unwrap();
            let authority = pc::test_support::EffectAuthority::new(&fence);
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            match boundary {
                "read" => {
                    runtime.st.lock().unwrap().live_credential_response_gate = Some(gate.clone())
                }
                "write" => *broker.writeback_response_gate.lock().unwrap() = Some(gate.clone()),
                _ => {}
            }
            let held = if boundary == "lock" {
                Some(sandbox.lifecycle.writeback_done.lock().await)
            } else {
                None
            };
            let mut pending = Box::pin(sandbox.prepare_disposal_for_effect(&fence, &authority));
            std::future::poll_fn(|cx| {
                assert!(
                    std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                    "{boundary}/{change}"
                );
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                runtime.st.lock().unwrap().live_credential_reads,
                usize::from(boundary != "lock")
            );
            assert_eq!(
                broker.writes.lock().unwrap().len(),
                usize::from(boundary == "write")
            );
            let mut successor = fence.clone();
            successor.renew();
            if change == "replace" {
                successor.reassign();
            }
            if change != "cancel" {
                *authority.current.lock().unwrap() = if change == "revoke" {
                    None
                } else {
                    Some((successor, std::time::Duration::from_secs(1)))
                };
            }
            drop(held);
            gate.add_permits(1);
            if change == "cancel" {
                drop(pending);
            } else {
                let result = pending.await;
                assert_eq!(result.is_ok(), change == "renew", "{boundary}/{change}");
            }
            assert_eq!(
                broker.writes.lock().unwrap().len(),
                usize::from(change == "renew" || boundary == "write")
            );
            assert!(!*sandbox.lifecycle.remove_done.lock().await);
            assert_eq!(
                runtime
                    .st
                    .lock()
                    .unwrap()
                    .alive
                    .get("cid-preparation-waits"),
                Some(&true)
            );
            runtime.st.lock().unwrap().live_credential_response_gate = None;
            *broker.writeback_response_gate.lock().unwrap() = None;
            // Retry only under restored exact fixture authority, not a new
            // Sandbox or invented successful preparation after the rejection.
            *authority.current.lock().unwrap() =
                Some((fence.clone(), std::time::Duration::from_secs(1)));
            sandbox
                .prepare_disposal_for_effect(&fence, &authority)
                .await
                .unwrap();
            let ids = broker.writeback_ids.lock().unwrap();
            assert!(ids.iter().all(|id| id == &ids[0]));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cold_native_adoption_reconstructs_terminal_secret_and_memory_participants() {
    /* Cold terminal-participant cause/effect decision table — CN1:
     * Causes: C1 the durable V2 handle carries one canonical copy-head set that
     * exactly joins the frozen Memory store+mount; C2 the runtime is native and
     * can reconstruct participant access; C3 the replacement Worker installs
     * the frozen Secret broker while the root terminal owner retains the only
     * MemoryMounter commit port; C4 live Secret/Memory reads succeed; C5 the
     * exact incarnation is Terminal/Ready/absent/Provisioning; C6 the deployment wraps the
     * provider in a warm-capacity adapter. C1-C6+
     * Terminal or Ready => E1 terminal preparation rebuilds descriptors without a host
     * staging guard, E2 the root commits Memory exactly once and acknowledges
     * the complete evidence while Container writes back its Secret, and E3
     * removes the physical object only afterwards.
     * Exact absent => E5 `None` without fabricating a lifecycle; Provisioning,
     * !C2, !C3, a different C1, or missing handle => E4 fail closed with zero
     * removal. The ordinary active-adoption port also
     * rejects Terminal, so it cannot become a parallel authorization path. The
     * original in-process mount guard is dropped before recovery, so success
     * cannot come from the first Worker's process-local cleanup state. */
    let runtime = Arc::new(
        FakeRuntime::default()
            .with_native_memory_archive(one_file_archive("changed.txt", b"changed-after-restart"))
            .with_live_credential(b"credential-after-restart"),
    );
    let mut sandbox_spec = writable_credential_spec("cold-native-recovery");
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
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"credential-before-restart".to_vec();
    let reference = Arc::new(Mutex::new(None));
    let harvested = Arc::new(Mutex::new(None));
    let recovered_evidence = Arc::new(Mutex::new(None));
    let reconciled = Arc::new(Mutex::new(None));
    let reconcile_calls = Arc::new(AtomicUsize::new(0));
    let mounter = Arc::new(HarvestingMemoryMounter {
        reference: reference.clone(),
        harvested: harvested.clone(),
        recovered_evidence: recovered_evidence.clone(),
        reconciled: reconciled.clone(),
        reconcile_calls: reconcile_calls.clone(),
    });

    let node_a = provider_without_broker(runtime.clone()).with_secret_broker(broker.clone());
    node_a.install_memory_mounter(mounter.clone());
    let sandbox = node_a
        .create_container(&sandbox_spec)
        .await
        .expect("CN1 initial native create");
    let handle = pc::Sandbox::handle(&sandbox);
    let handle: pc::SandboxHandle =
        serde_json::from_slice(&serde_json::to_vec(&handle).expect("CN1 serialize durable handle"))
            .expect("CN1 deserialize durable handle");
    let evidence = handle
        .memory_materializations()
        .expect("CN1 valid handle")
        .expect("CN1 current V2 evidence");
    assert_eq!(evidence.len(), 1, "C1 one copy projection");
    assert_eq!(evidence[0].store_id, "mutable-store-id");
    assert_eq!(evidence[0].mount_path, "/workspace/.mnt/memory");
    assert_eq!(evidence[0].heads[0].path, "seed.txt");
    drop(sandbox);
    drop(node_a);

    assert!(
        ContainerCleanupState::recovered_native(
            &sandbox_spec,
            &handle,
            Some(broker.clone()),
            false,
            None,
        )
        .is_err(),
        "E4 host-bind attempt cannot reconstruct participants"
    );
    assert!(
        ContainerCleanupState::recovered_native(&sandbox_spec, &handle, None, true, None).is_err(),
        "E4 missing broker"
    );
    let mut different = sandbox_spec.clone();
    let pc::MountSource::MemoryStore { store_id, .. } = &mut different.mounts[1].source else {
        unreachable!("CN1 Memory fixture")
    };
    *store_id = "different-store".into();
    assert!(
        ContainerCleanupState::recovered_native(
            &different,
            &handle,
            Some(broker.clone()),
            true,
            None,
        )
        .is_err(),
        "E4 frozen Memory identity mismatch"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-cold-native-recovery"),
        Some(&true),
        "E4 decision rows do not remove the live realization"
    );

    let node_b =
        Arc::new(provider_without_broker(runtime.clone()).with_secret_broker(broker.clone()));
    let pool = WarmContainerPool::new(node_b.clone(), 0);
    let terminal_fence = pc::SandboxEffectFence::new(
        "cold-native-terminal",
        "worker-owner",
        "worker-incarnation",
        7,
        u64::MAX,
    )
    .unwrap();
    runtime.set_observation(pc::SandboxObservation::Terminal {
        physical_incarnation: "cid-cold-native-recovery".into(),
    });
    assert!(
        SandboxProvider::adopt_for_effect(
            &pool,
            SandboxAdoption::new(&sandbox_spec, &handle),
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &terminal_fence,
                authority: &pc::test_support::EffectAuthority::new(&terminal_fence)
            }
        )
        .await
        .is_err(),
        "E4 active adoption does not disguise terminal preparation",
    );
    let adapter_prepared = SandboxProvider::prepare_terminal_for_effect(
        &pool,
        &sandbox_spec,
        Some(&handle),
        None,
        &terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
    )
    .await
    .expect("E1 reconstruct exact terminal participants through adapter port")
    .expect("E1 terminal realization has a lifecycle owner");
    let recovered_files = adapter_prepared
        .read_files("/workspace/.mnt/memory")
        .await
        .expect("E2 provider reads the exact frozen Memory root")
        .into_iter()
        .map(|file| (file.path, file.bytes))
        .collect::<Vec<_>>();
    pc::MemoryMounter::reconcile_recovered_copy(
        mounter.as_ref(),
        "terminal-memory-operation",
        &evidence[0],
        &recovered_files,
        pc::MountAccess::ReadWrite,
    )
    .await
    .expect("E2 root terminal Memory owner commits once");
    drop(adapter_prepared);
    runtime.set_observation(pc::SandboxObservation::Ready);
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &pool,
            &sandbox_spec,
            Some(&handle),
            None,
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence)
        )
        .await
        .expect("E1 exact Ready remains disposable")
        .is_some(),
        "E1 Ready produces the same raw lifecycle without renewing or launching it"
    );
    runtime.set_observation(pc::SandboxObservation::DefinitivelyUnavailable {
        physical_incarnation: Some("cid-cold-native-recovery".into()),
    });
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &pool,
            &sandbox_spec,
            Some(&handle),
            None,
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence)
        )
        .await
        .expect("E5 exact absence is a closed outcome")
        .is_none(),
        "E5 absence cannot fabricate a lifecycle"
    );
    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &pool,
            &sandbox_spec,
            None,
            Some(&terminal_fence),
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence)
        )
        .await
        .is_err(),
        "E4 built-in container restore has no handle-free terminal owner"
    );
    runtime.set_observation(pc::SandboxObservation::Terminal {
        physical_incarnation: "cid-cold-native-recovery".into(),
    });
    let adopted = pc::SandboxProvider::prepare_terminal_for_effect(
        node_b.as_ref(),
        &sandbox_spec,
        Some(&handle),
        None,
        &terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
    )
    .await
    .expect("E1 neutral port projects the same terminal owner")
    .expect("E1 terminal realization has a neutral lifecycle owner");
    adopted
        .acknowledge_memory_reconciliation(
            &terminal_fence,
            evidence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("E2 exact terminal Memory acknowledgement gates physical cleanup");
    adopted
        .prepare_disposal_for_effect(
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
        )
        .await
        .expect("E2 source-dependent preparation precedes aggregate admission");
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-cold-native-recovery"),
        Some(&true),
        "E2 preparation has zero physical effect"
    );
    adopted
        .dispose_for_effect(
            &disposal_authorization(&terminal_fence),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal_fence).effect_fence(),
            ),
        )
        .await
        .expect("E2/E3 terminal cleanup");

    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[b"credential-after-restart".to_vec()],
        "E2 recovered Secret writeback"
    );
    assert_eq!(
        broker.effect_operations.lock().unwrap().as_slice(),
        &["cold-native-terminal".to_string()],
        "E2 recovered Secret writeback uses the root terminal operation"
    );
    assert_eq!(
        reference.lock().unwrap().as_deref(),
        Some("terminal-memory-operation"),
        "E2 root owns the operation reference"
    );
    assert_eq!(
        recovered_evidence
            .lock()
            .unwrap()
            .as_ref()
            .expect("E2 durable copy base")
            .heads[0]
            .path
            .as_str(),
        "seed.txt",
        "E2 reconciliation consumes the persisted original heads"
    );
    assert_eq!(
        reconciled.lock().unwrap().as_deref(),
        Some([("changed.txt".to_string(), b"changed-after-restart".to_vec())].as_slice()),
        "E2 recovered Memory bytes"
    );
    assert_eq!(
        reconcile_calls.load(Ordering::SeqCst),
        1,
        "E2 Container disposal cannot perform a second Memory commit"
    );
    assert!(
        harvested.lock().unwrap().is_none(),
        "E1 process-local first-Worker guard was not reused"
    );
    assert_eq!(
        runtime
            .st
            .lock()
            .unwrap()
            .alive
            .get("cid-cold-native-recovery"),
        Some(&false),
        "E3 exact physical removal follows root Memory commit and Secret writeback"
    );
}

#[tokio::test]
async fn cold_host_bind_terminal_preparation_fails_before_observation_or_removal() {
    /* Cold host-bind cause/effect table. C1 the durable V2 handle names a
     * Docker/Podman-style realization whose File/Secret/Memory participants are
     * owned only by the creating process; C2 that process is gone; C3 the root
     * authorizes terminal cleanup. C1+C2+C3 => E1 fail closed before backend
     * observation/removal and E2 preserve the exact physical object. Native
     * reconstructible runtimes occupy the successful CN1 rows above. This is a
     * deliberate safety boundary, not a claim that CurrentAttemptOnly recovery
     * is live or automatically reclaimable. */
    let runtime = Arc::new(FakeRuntime::default());
    let provider = provider(runtime.clone());
    let sandbox_spec = spec("cold-host-bind-terminal");
    let sandbox = provider
        .create_container(&sandbox_spec)
        .await
        .expect("fixture creates one current V2 handle");
    let handle = pc::Sandbox::handle(&sandbox);
    drop(sandbox);
    runtime.set_observation(pc::SandboxObservation::Terminal {
        physical_incarnation: "cid-cold-host-bind-terminal".into(),
    });
    let observations_before = runtime.st.lock().unwrap().observations;
    let terminal_fence = pc::SandboxEffectFence::new(
        "cold-host-bind-cleanup",
        "worker-owner",
        "worker-incarnation",
        8,
        u64::MAX,
    )
    .unwrap();

    assert!(
        SandboxProvider::prepare_terminal_for_effect(
            &provider,
            &sandbox_spec,
            Some(&handle),
            None,
            &terminal_fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence)
        )
        .await
        .is_err(),
        "E1 prior process-local participants are not reconstructible"
    );
    let state = runtime.st.lock().unwrap();
    assert_eq!(
        state.observations, observations_before,
        "E1 zero observation"
    );
    assert_eq!(
        state.alive.get("cid-cold-host-bind-terminal"),
        Some(&true),
        "E2 zero removal"
    );
}

#[tokio::test]
async fn cold_host_bind_terminal_recovery_requires_complete_participants() {
    // Cause/effect graph: exact terminal identity + recoverable host staging +
    // writable Secret broker => fenced cleanup and one writeback. Missing root,
    // invalid root or missing broker => no removal and no writeback. The older
    // CurrentAttemptOnly test owns runtimes with no reconstruction capability.
    // R7=all evidence; R8a=root absent; R8b=root outside namespace; R8c=no broker.
    for rule in ["R7", "R8a", "R8b", "R8c"] {
        let runtime = Arc::new(FakeRuntime::default().with_live_credential(b"changed"));
        let broker = Arc::new(RecordingSecretBroker::default());
        *broker.current.lock().unwrap() = b"initial".to_vec();
        let sandbox_spec = writable_credential_spec(&format!("cold-bind-{rule}"));
        let original = provider_without_broker(runtime.clone()).with_secret_broker(broker.clone());
        let sandbox = original.create_container(&sandbox_spec).await.unwrap();
        let handle = pc::Sandbox::handle(&sandbox);
        let id = sandbox.container_id.clone();
        let source =
            std::path::PathBuf::from(&runtime.st.lock().unwrap().created_binds[&id][0].source_ref);
        let root = source.parent().unwrap().to_path_buf();
        drop(sandbox);
        drop(original);
        {
            let mut state = runtime.st.lock().unwrap();
            state.host_live_input_bind = true;
            state.recovered_staging = match rule {
                "R8a" => None,
                "R8b" => Some(std::env::temp_dir()),
                _ => Some(root.clone()),
            };
        }
        runtime.set_observation(pc::SandboxObservation::Terminal {
            physical_incarnation: id.clone(),
        });
        let cold = provider_without_broker(runtime.clone());
        if rule != "R8c" {
            cold.install_secret_broker(broker.clone());
        }
        let fence =
            pc::SandboxEffectFence::new("cold-cleanup", "owner", "incarnation", 8, u64::MAX)
                .unwrap();
        let result = SandboxProvider::prepare_terminal_for_effect(
            &cold,
            &sandbox_spec,
            Some(&handle),
            None,
            &fence,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
        )
        .await;
        if rule == "R7" {
            let cleanup = result
                .expect("R7 reconstruct existing participants")
                .unwrap();
            // R7 joins the existing operation-bound authority contract: the
            // preparation fence and physical disposal fence name distinct
            // effects. Reuse the shared fixture, never infer authority from a
            // successful reconstruction or add a permissive compatibility path.
            let preparation_authority = pc::test_support::EffectAuthority::new(&fence);
            cleanup
                .prepare_disposal_for_effect(&fence, &preparation_authority)
                .await
                .unwrap();
            assert!(
                runtime.st.lock().unwrap().alive[&id],
                "preparation cannot remove the object"
            );
            let authorization = disposal_authorization(&fence);
            let disposal_authority =
                pc::test_support::EffectAuthority::new(authorization.effect_fence());
            cleanup
                .dispose_for_effect(&authorization, &disposal_authority)
                .await
                .unwrap();
            cleanup
                .dispose_for_effect(&authorization, &disposal_authority)
                .await
                .unwrap();
            assert_eq!(
                broker.writes.lock().unwrap().len(),
                1,
                "R7 idempotent writeback"
            );
            assert!(!runtime.st.lock().unwrap().alive[&id]);
            assert!(!root.exists(), "R7 remove exact retained staging");
        } else {
            assert!(result.is_err(), "{rule} must fail closed");
            assert!(
                runtime.st.lock().unwrap().alive[&id],
                "{rule} no physical removal"
            );
            assert!(
                broker.writes.lock().unwrap().is_empty(),
                "{rule} no writeback"
            );
            // Fixture cleanup is outside the assertion and never earns success.
            std::fs::remove_dir_all(&root).unwrap();
        }
        assert_eq!(
            runtime.st.lock().unwrap().created_command.len(),
            1,
            "no reactivation"
        );
    }
}

#[tokio::test]
async fn cold_reconstruction_and_adoption_recheck_authority_after_each_provider_wait() {
    use std::time::Duration;
    async fn recover(
        provider: &ContainerProvider<FakeRuntime>,
        spec: &pc::SandboxSpec,
        handle: &pc::SandboxHandle,
        fence: &pc::SandboxEffectFence,
        authority: &dyn pc::CurrentExecutionAuthority,
        live: bool,
    ) -> Result<Option<Arc<dyn SessionSandbox>>, pc::SandboxError> {
        if live {
            provider
                .adopt_for_effect(
                    SandboxAdoption::new(spec, handle),
                    awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority },
                )
                .await
                .map(Some)
        } else {
            provider
                .prepare_terminal_for_effect(spec, Some(handle), None, fence, authority)
                .await
        }
    }
    // Cause/effect table: C1 Ready observation/staging/control response, exact
    // absence response, or Disposing observation response; C2 revoke/replace/expire/
    // renew/cancel while the REAL future is Pending. R1 stale authority admits
    // no NEXT staging/control effect or success (including None); R2 renewal
    // completes; R3 cancellation retains original host bytes. R4 explicit
    // restored fixture authority retries the exact handle. C3 live adoption or
    // terminal reconstruction; C4 past/future Control expiry. All 7x5x2 rows
    // assert zero removal/writeback: reconstruction is
    // not disposal. These checks do not cancel already accepted backend work.
    // R5 every cold observation uses TerminalCleanup, including the None row:
    // Recovery absence is insufficient for terminal resource acknowledgement.
    // R6 Ready terminal reconstruction has no Control wait/effect at all; only
    // live adoption crosses that boundary. Observation/staging revocation rows
    // still cover Ready preparation, and every such row asserts zero binding.
    for expiry in [100, u64::MAX - 1] {
        for (phase, boundary) in [
            ("live", "observation"),
            ("live", "staging"),
            ("live", "control"),
            ("ready", "observation"),
            ("ready", "staging"),
            ("absent", "observation"),
            ("disposing", "observation"),
        ] {
            for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                let runtime = Arc::new(FakeRuntime::default().with_sandbox_control());
                let broker = Arc::new(RecordingSecretBroker::default());
                let provider =
                    provider_without_broker(runtime.clone()).with_secret_broker(broker.clone());
                let mut spec = writable_credential_spec("cold-current-authority");
                spec.control_services
                    .insert(SandboxControlServiceKind::RepositoryGitCredential);
                let sandbox = provider.create_container(&spec).await.unwrap();
                let handle = pc::Sandbox::handle(&sandbox);
                let id = sandbox.container_id.clone();
                let source = std::path::PathBuf::from(
                    &runtime.st.lock().unwrap().created_binds[&id][0].source_ref,
                );
                let root = source.parent().unwrap().to_path_buf();
                let source = root.join("cold-authority-proof");
                std::fs::write(&source, b"synthetic retained bytes").unwrap();
                let bytes = std::fs::read(&source).unwrap();
                drop(sandbox);
                let terminal =
                    pc::SandboxEffectFence::new("terminal", "owner", "boot", 8, expiry).unwrap();
                let authority = pc::test_support::EffectAuthority::new(&terminal);
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                let (observations, bindings) = {
                    let mut state = runtime.st.lock().unwrap();
                    state.host_live_input_bind = true;
                    state.recovered_staging = Some(root.clone());
                    state.observation = Some(match phase {
                        "absent" => pc::SandboxObservation::DefinitivelyUnavailable {
                            physical_incarnation: Some(id.clone()),
                        },
                        "disposing" => pc::SandboxObservation::Disposing {
                            physical_incarnation: id.clone(),
                        },
                        _ => pc::SandboxObservation::Ready,
                    });
                    match boundary {
                        "observation" => state.observation_response_gate = Some(gate.clone()),
                        "staging" => state.staging_response_gate = Some(gate.clone()),
                        _ => state.control_binding_response_gate = Some(gate.clone()),
                    }
                    (state.observations, state.control_binding_calls.len())
                };
                let denied = pc::test_support::EffectAuthority::denied();
                assert!(
                    recover(
                        &provider,
                        &spec,
                        &handle,
                        &terminal,
                        &denied,
                        phase == "live"
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    runtime.st.lock().unwrap().observations,
                    observations,
                    "entry denial has zero I/O"
                );
                let mut pending = Box::pin(recover(
                    &provider,
                    &spec,
                    &handle,
                    &terminal,
                    &authority,
                    phase == "live",
                ));
                std::future::poll_fn(|cx| {
                    assert!(
                        std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                        "{phase}/{boundary}/{change}"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        "revoke" => *current = None,
                        "replace" => current.as_mut().unwrap().0.reassign(),
                        "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                        "renew" => current.as_mut().unwrap().0.renew(),
                        _ => {}
                    }
                }
                if change == "cancel" {
                    drop(pending);
                } else {
                    gate.add_permits(1);
                    let result = pending.await;
                    assert_eq!(
                        result.is_ok(),
                        change == "renew",
                        "{phase}/{boundary}/{change}"
                    );
                    if change == "renew" {
                        assert_eq!(result.unwrap().is_none(), phase == "absent");
                    }
                }
                {
                    let mut state = runtime.st.lock().unwrap();
                    assert_eq!(state.observations, observations + 1);
                    assert_eq!(
                        state.observation_purposes.last(),
                        Some(&if phase == "live" {
                            SandboxObservationPurpose::Recovery
                        } else {
                            SandboxObservationPurpose::TerminalCleanup
                        }),
                        "R5 {phase}/{boundary}/{change}"
                    );
                    assert_eq!(
                        state.recovered_staging_calls,
                        usize::from(
                            matches!(phase, "ready" | "live")
                                && (change == "renew" || boundary != "observation")
                        )
                    );
                    assert_eq!(
                        state.control_binding_calls.len() - bindings,
                        usize::from(
                            phase == "live" && (boundary == "control" || change == "renew")
                        )
                    );
                    assert!(state.removals.is_empty());
                    state.observation_response_gate = None;
                    state.staging_response_gate = None;
                    state.control_binding_response_gate = None;
                }
                assert_eq!(std::fs::read(&source).unwrap(), bytes, "retained bytes");
                assert!(broker.writes.lock().unwrap().is_empty());
                *authority.current.lock().unwrap() =
                    Some((terminal.clone(), Duration::from_secs(1)));
                let replay = recover(
                    &provider,
                    &spec,
                    &handle,
                    &terminal,
                    &authority,
                    phase == "live",
                )
                .await
                .unwrap();
                assert_eq!(replay.is_none(), phase == "absent");
                assert_eq!(
                    runtime.st.lock().unwrap().observation_purposes.last(),
                    Some(&if phase == "live" {
                        SandboxObservationPurpose::Recovery
                    } else {
                        SandboxObservationPurpose::TerminalCleanup
                    }),
                    "R5 exact retry preserves the purpose"
                );
                drop(replay);
                crate::restore_target::remove_host_staging_path(&root).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn effect_fenced_total_absence_has_no_terminal_environment() {
    /* Total-absence terminal preparation table CT1. Causes: C1 the durable
     * current handle carries exact physical identity; C2 adapter observation
     * has already proved every typed participant absent; C3 the aggregate
     * supplies a live terminal effect fence. Effect E1 return `None`, so no
     * executable or cleanup-only Environment is fabricated and exact disposal
     * replay is a zero-mutation success. Rule CT1 C1+C2+C3=>E1. Kubernetes's
     * absent-Pod/live-PVC row is deliberately rejected by its adapter before
     * this provider-neutral branch and is covered by KAP1/KPV5. */
    let runtime = Arc::new(FakeRuntime::default().with_native_memory_archive(Vec::new()));
    let provider = provider(runtime.clone());
    let sandbox_spec = spec("claim-only-terminal");
    let sandbox = provider.create_container(&sandbox_spec).await.unwrap();
    let created = pc::Sandbox::handle(&sandbox);
    let mut previous = created.container_payload().unwrap().clone();
    previous.runtime_handle = Some(
        crate::ContainerContinuationHandle::KubernetesContinuationV2 {
            pod_uid: "pod-a".into(),
            claim_uid: Some("claim-p".into()),
        },
    );
    let handle = pc::SandboxHandle::container_v2(
        created.sandbox_id.clone(),
        crate::ContainerSandboxHandleV2 {
            previous,
            adoption_fingerprint: created
                .container_adoption_fingerprint()
                .unwrap()
                .unwrap()
                .clone(),
            realization_fingerprint: created.realization_fingerprint().unwrap().clone(),
            owned_paths: created.owned_paths().unwrap().to_vec(),
        },
    );
    drop(sandbox);
    runtime.set_observation(pc::SandboxObservation::DefinitivelyUnavailable {
        physical_incarnation: Some("pod-a".into()),
    });
    let terminal_fence = pc::SandboxEffectFence::new(
        "claim-only-terminal-effect",
        "worker-owner",
        "worker-incarnation",
        9,
        u64::MAX,
    )
    .unwrap();

    let target = SandboxProvider::prepare_terminal_for_effect(
        &provider,
        &sandbox_spec,
        Some(&handle),
        None,
        &terminal_fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal_fence),
    )
    .await
    .expect("CT1 exact total-absence observation");
    assert!(target.is_none(), "CT1/E1");
}

struct FakeMemoryMounter {
    torn_down: Arc<AtomicBool>,
    fail_teardown: bool,
}

struct FakeMemoryMount {
    torn_down: Arc<AtomicBool>,
    fail_teardown: bool,
    teardown_calls: Arc<AtomicUsize>,
    response_gate: Option<Arc<tokio::sync::Semaphore>>,
    realization: pc::Realization,
}

#[async_trait]
impl pc::MemoryMounter for FakeMemoryMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        let result: Result<Box<dyn pc::MemoryMount>, pc::SandboxError> = (|| {
            std::fs::create_dir_all(host_path)
                .map_err(|e| pc::SandboxError::backend("mount", e))?;
            std::fs::write(host_path.join("seed.txt"), b"seed")
                .map_err(|e| pc::SandboxError::backend("mount", e))?;
            Ok(Box::new(FakeMemoryMount {
                torn_down: self.torn_down.clone(),
                fail_teardown: self.fail_teardown,
                teardown_calls: Arc::new(AtomicUsize::new(0)),
                response_gate: None,
                realization: pc::Realization::Copy,
            }) as Box<dyn pc::MemoryMount>)
        })();
        pc::MemoryMountOutcome::from_no_effect(result)
    }

    async fn recover_unknown_mount(
        &self,
        _: &str,
        _: &std::path::Path,
        _: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "recovered Memory copy reconciliation is outside this fixture",
        ))
    }
}

#[async_trait]
impl pc::MemoryMount for FakeMemoryMount {
    fn realization(&self) -> pc::Realization {
        self.realization
    }

    fn materialization_heads(&self) -> Option<Vec<pc::MemoryMaterializationHead>> {
        Some(Vec::new())
    }

    async fn teardown(&self) -> Result<(), pc::SandboxError> {
        self.torn_down.store(true, Ordering::SeqCst);
        self.teardown_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.response_gate {
            let _permit = gate.acquire().await.unwrap();
        }
        if self.fail_teardown {
            Err(pc::SandboxError::backend(
                "teardown",
                "injected Memory teardown failure",
            ))
        } else {
            Ok(())
        }
    }
}

struct OrderedCopyMounter {
    runtime: Arc<FakeRuntime>,
    scope: String,
    mount_calls: Arc<AtomicUsize>,
    recovery_calls: Arc<AtomicUsize>,
    teardown_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl pc::MemoryMounter for OrderedCopyMounter {
    async fn mount(
        &self,
        _store_id: &str,
        host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        let state = self.runtime.st.lock().unwrap();
        assert_eq!(
            state.alive.get(&format!("cid-{}", self.scope)),
            Some(&true),
            "physical reservation precedes Memory"
        );
        assert_eq!(state.activation_calls, 0, "Memory precedes activation");
        drop(state);
        self.mount_calls.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(host_path).unwrap();
        std::fs::write(host_path.join("seed.txt"), b"seed").unwrap();
        pc::MemoryMountOutcome::Mounted(Box::new(FakeMemoryMount {
            torn_down: Arc::new(AtomicBool::new(false)),
            fail_teardown: false,
            teardown_calls: self.teardown_calls.clone(),
            response_gate: None,
            realization: pc::Realization::Copy,
        }))
    }

    async fn recover_unknown_mount(
        &self,
        _store_id: &str,
        _host_path: &std::path::Path,
        _access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.recovery_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn reconcile_recovered_copy(
        &self,
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        Err(awaken_provisioning_contract::SandboxError::unsupported(
            "recovered Memory copy reconciliation is outside this fixture",
        ))
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

enum ActivationFailure {
    BeforeEffect(RuntimeError),
    AfterEffect(RuntimeError),
}

#[derive(Default)]
struct FakeState {
    network_allowlist: bool,
    checkpoint_expected: Option<(
        pc::SandboxHandle,
        pc::SandboxEffectFence,
        pc::SandboxEffectFence,
    )>,
    checkpoint_calls: usize,
    checkpoint_response_gate: Option<Arc<tokio::sync::Semaphore>>,
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
    /// Makes every exec refuse, so a test can drive the reaper's failure edge
    /// against a container the fake still reports as running.
    fail_spawn: bool,
    may_have_committed_create: bool,
    expire_effect_fence_once: bool,
    creation_response_gate: Option<(&'static str, Arc<tokio::sync::Semaphore>)>,
    creation_boundaries: Vec<&'static str>,
    recover_prepared_reservations: bool,
    prepared_attempts: HashMap<String, String>,
    active_reservations: std::collections::HashSet<String>,
    activation_calls: usize,
    activation_failure: Option<ActivationFailure>,
    effect_fences: Vec<SandboxEffectFence>,
    attempted_binds: Vec<BindPlan>,
    refreshed_credential: Option<Vec<u8>>,
    live_credential: Option<Vec<u8>>,
    live_credential_error: Option<String>,
    live_credential_reads: usize,
    live_credential_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    credential_source: Option<std::path::PathBuf>,
    spawned: Vec<(String, Vec<String>)>,
    runtime_path_observations: Vec<RuntimePathObservation>,
    process_secret_observations: Vec<(bool, bool)>,
    live_input_projection: bool,
    host_live_input_bind: bool,
    recovered_staging: Option<std::path::PathBuf>,
    live_inputs: HashMap<String, Vec<u8>>,
    native_memory: bool,
    memory_archive: Vec<u8>,
    created_memory_mounts: HashMap<String, Vec<MemoryMount>>,
    control_enabled: bool,
    control_binding_missing: bool,
    control_binding_calls: Vec<(bool, Option<String>)>,
    control_channel_opens: usize,
    control_open_pending: Option<(Arc<tokio::sync::Notify>, Arc<AtomicBool>)>,
    control_peers: Vec<tokio::io::DuplexStream>,
    removals: Vec<(String, usize)>,
    removal_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    observation_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    staging_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    control_binding_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    recovered_staging_calls: usize,
    inspections: usize,
    restore_query_gate: Option<(&'static str, Arc<tokio::sync::Semaphore>)>,
    restore_queries: Vec<(&'static str, String)>,
    restore_targets: HashMap<String, pc::SandboxRestorationEvidence>,
    restore_plan_fingerprints: HashMap<String, String>,
    restore_creates: usize,
    restore_disposals: usize,
    restore_disposal_observation_gate: Option<Arc<tokio::sync::Semaphore>>,
    restore_disposal_response_gate: Option<Arc<tokio::sync::Semaphore>>,
    restore_disposal_authorizations: Vec<pc::SandboxDisposalAuthorization>,
    observations: u32,
    observation_purposes: Vec<SandboxObservationPurpose>,
    retained_observation_attempts: Vec<Option<String>>,
    observation: Option<pc::SandboxObservation>,
    unmaterialized_absence_proven: bool,
    recovered_created_pod: Option<(String, String, Option<String>)>,
    recovered_guarded_claim: Option<(String, crate::KubernetesGuardedClaimHandle)>,
    unmaterialized_absence_checks: usize,
    configuration: Option<std::collections::BTreeMap<String, String>>,
    historical_configuration: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Default)]
struct FakeRuntime {
    st: Mutex<FakeState>,
}

#[test]
fn worker_relay_substitution_requires_live_runtime_proxy_and_selected_route() {
    // Causes: C1 runtime attests no-bypass allowlist, C2 signed proxy issuer
    // installed, C3 deployment selected the existing Worker relay route.
    // Effects: E1 one admissible substitution capability; E2 every missing
    // cause refuses it. Decision table: R1 C1+C2+C3 -> E1; R2 any missing ->
    // E2. This tests the composed provider capability used by Worker admission,
    // not the standalone boolean helper.
    for runtime_attested in [false, true] {
        for issuer_installed in [false, true] {
            for relay_selected in [false, true] {
                let runtime = Arc::new(FakeRuntime::default());
                runtime.st.lock().unwrap().network_allowlist = runtime_attested;
                let mut provider = ContainerProvider::for_test(runtime, "fixture");
                if issuer_installed {
                    provider = provider.with_allowlist_proxy(
                        AllowlistProxy::new("http://gateway.test:8091", [7; 32], 60).unwrap(),
                    );
                }
                if relay_selected {
                    provider = provider.with_worker_mcp_relay_substitution();
                }
                let caps = provider.capabilities();
                assert_eq!(
                    caps.supports_secret_egress_without_bypass(),
                    runtime_attested && issuer_installed && relay_selected,
                    "C1={runtime_attested} C2={issuer_installed} C3={relay_selected}"
                );
            }
        }
    }
}

#[tokio::test]
async fn container_checkpoint_projection_preserves_exact_handle_and_current_authority() {
    // CF1 valid current handle -> forward every physical/fence coordinate once;
    // CF2 entry denial or legacy handle -> zero runtime calls; CF3 delayed
    // successful runtime response + revoke/replace/expire -> no provider success;
    // CF4 renewal of the same grant -> success. The fake never checks CF3 on
    // behalf of the provider, so this covers real composition wiring.
    use pc::Sandbox as _;
    for change in [
        "current", "legacy", "denied", "renew", "revoke", "replace", "expire",
    ] {
        let runtime = Arc::new(FakeRuntime::default());
        let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
        let mut requested = spec("checkpoint-projection");
        requested.mounts.clear();
        requested.env.clear();
        let create =
            pc::SandboxEffectFence::new("create", "owner", "boot", 1, u64::MAX - 1).unwrap();
        let mut sandbox = provider
            .create_container_for_effect(
                &requested,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &create,
                    authority: &pc::test_support::EffectAuthority::new(&create),
                },
                ContainerRealizationIntent::Create,
            )
            .await
            .unwrap();
        let source =
            pc::SandboxEffectFence::new("suspend", "owner", "boot", 2, u64::MAX - 1).unwrap();
        let active =
            pc::SandboxEffectFence::new("terminal", "owner", "boot", 2, u64::MAX - 1).unwrap();
        let authority = if change == "denied" {
            pc::test_support::EffectAuthority::denied()
        } else {
            pc::test_support::EffectAuthority::new(&active)
        };
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        {
            let mut state = runtime.st.lock().unwrap();
            state.checkpoint_expected = Some((sandbox.handle(), source.clone(), active.clone()));
            state.checkpoint_response_gate = Some(gate.clone());
        }
        if change == "legacy" {
            sandbox.adoption_fingerprint = None;
            sandbox.realization_fingerprint = None;
        }
        let access = SandboxCheckpointAccess {
            source_effect_fence: &source,
            effect_fence: &active,
            request_fingerprint: "request",
            authority: &authority,
        };
        let mut pending = Box::pin(SessionSandbox::checkpoint_upload(
            &sandbox,
            access,
            SandboxCheckpointMutation::Acquire,
        ));
        if matches!(change, "denied" | "legacy") {
            assert!(pending.await.is_err(), "CF2");
            assert_eq!(runtime.st.lock().unwrap().checkpoint_calls, 0);
            continue;
        }
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                "CF must reach runtime response wait"
            );
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(runtime.st.lock().unwrap().checkpoint_calls, 1);
        {
            let mut current = authority.current.lock().unwrap();
            match change {
                "renew" => current.as_mut().unwrap().0.renew(),
                "revoke" => *current = None,
                "replace" => current.as_mut().unwrap().0.reassign(),
                "expire" => current.as_mut().unwrap().1 = std::time::Duration::ZERO,
                "current" => {}
                _ => unreachable!(),
            }
        }
        gate.add_permits(1);
        assert_eq!(
            pending.await.is_ok(),
            matches!(change, "current" | "renew"),
            "CF3/CF4/{change}"
        );
        assert!(runtime.st.lock().unwrap().removals.is_empty());
    }
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
    async fn creation_response(&self, boundary: &'static str) {
        let gate = {
            let mut state = self.st.lock().unwrap();
            state.creation_boundaries.push(boundary);
            state
                .creation_response_gate
                .as_ref()
                .filter(|(selected, _)| *selected == boundary)
                .map(|(_, gate)| gate.clone())
        };
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
    }

    async fn restore_query_response(&self, query: &'static str, container_id: &str) {
        let gate = {
            let mut state = self.st.lock().unwrap();
            state.restore_queries.push((query, container_id.to_owned()));
            state
                .restore_query_gate
                .as_ref()
                .and_then(|(boundary, gate)| (*boundary == query).then(|| gate.clone()))
        };
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
    }

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

    fn expiring_effect_fence_once(self) -> Self {
        self.st.lock().unwrap().expire_effect_fence_once = true;
        self
    }

    fn set_observation(&self, observation: pc::SandboxObservation) {
        self.st.lock().unwrap().observation = Some(observation);
    }
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    fn realization_configuration(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, RuntimeError> {
        Ok(self
            .st
            .lock()
            .unwrap()
            .configuration
            .clone()
            .unwrap_or_else(|| {
                std::collections::BTreeMap::from([(
                    "adapter_type".into(),
                    std::any::type_name::<Self>().into(),
                )])
            }))
    }

    fn historical_terminal_configuration(
        &self,
        _: Option<&crate::ContainerContinuationHandle>,
    ) -> Result<Option<std::collections::BTreeMap<String, String>>, RuntimeError> {
        Ok(self.st.lock().unwrap().historical_configuration.clone())
    }
    async fn missing_handle_terminal_evidence(
        &self,
        _scope: &str,
        plan: &ContainerPlan,
        expected_effect_fence: Option<&SandboxEffectFence>,
    ) -> Result<MissingHandleTerminalEvidence, RuntimeError> {
        let mut state = self.st.lock().unwrap();
        state.unmaterialized_absence_checks += 1;
        if expected_effect_fence.is_some()
            && let Some((container_id, guarded)) = &state.recovered_guarded_claim
        {
            return Ok(MissingHandleTerminalEvidence::ExactIncarnation {
                container_id: container_id.clone(),
                runtime_handle: crate::ContainerContinuationHandle::KubernetesGuardedClaim(
                    Box::new(guarded.clone()),
                ),
            });
        }
        if expected_effect_fence.is_some()
            && let Some((container_id, pod_uid, claim_uid)) = &state.recovered_created_pod
        {
            return Ok(MissingHandleTerminalEvidence::ExactIncarnation {
                container_id: container_id.clone(),
                runtime_handle: crate::ContainerContinuationHandle::KubernetesContinuationV2 {
                    pod_uid: pod_uid.clone(),
                    claim_uid: claim_uid.clone(),
                },
            });
        }
        Ok(
            if expected_effect_fence.is_some()
                && state.unmaterialized_absence_proven
                && plan.packages.is_empty()
                && plan.binds.is_empty()
                && plan.memory_mounts.is_empty()
                && plan.filesystem_continuity == pc::FilesystemContinuity::Ephemeral
            {
                MissingHandleTerminalEvidence::Unmaterialized
            } else {
                MissingHandleTerminalEvidence::Unknown
            },
        )
    }

    async fn checkpoint_upload(
        &self,
        expectation: ContainerObservationExpectation<'_>,
        access: SandboxCheckpointAccess<'_>,
        mutation: SandboxCheckpointMutation<'_>,
    ) -> Result<Option<pc::SandboxCheckpointUpload>, RuntimeError> {
        let gate = {
            let mut state = self.st.lock().unwrap();
            let (handle, source, active) = state
                .checkpoint_expected
                .as_ref()
                .expect("checkpoint fixture configured");
            let decoded = crate::DecodedContainerHandle::decode(handle).unwrap();
            let expected =
                ContainerObservationExpectation::from_decoded_for_effect(&decoded, active);
            assert_eq!(expectation.container_id, expected.container_id);
            assert_eq!(
                expectation.adoption_fingerprint,
                expected.adoption_fingerprint
            );
            assert_eq!(
                expectation.realization_fingerprint,
                expected.realization_fingerprint
            );
            assert_eq!(expectation.runtime_handle, expected.runtime_handle);
            assert_eq!(expectation.effect_fence, expected.effect_fence);
            assert_eq!(access.source_effect_fence, source);
            assert_eq!(access.effect_fence, active);
            assert_eq!(access.request_fingerprint, "request");
            assert!(matches!(mutation, SandboxCheckpointMutation::Acquire));
            state.checkpoint_calls += 1;
            state.checkpoint_response_gate.clone()
        };
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        // Deliberately accept the late response: the actual ContainerSandbox,
        // not this fixture, must prove its own post-await authority check.
        Ok(None)
    }

    async fn probe_ready(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn enforces_network_none(&self) -> bool {
        true
    }

    fn enforces_network_allowlist(&self) -> bool {
        self.st.lock().unwrap().network_allowlist
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
    ) -> Result<Option<crate::SandboxControlIncarnation>, RuntimeError> {
        let expected = match request {
            SandboxControlBindingRequest::New { .. } => None,
            SandboxControlBindingRequest::Adopt { expected, .. } => expected
                .and_then(crate::SandboxControlIncarnation::kubernetes_pod_uid)
                .map(str::to_owned),
        };
        self.st.lock().unwrap().control_binding_calls.push((
            matches!(request, SandboxControlBindingRequest::Adopt { .. }),
            expected,
        ));
        let gate = self
            .st
            .lock()
            .unwrap()
            .control_binding_response_gate
            .clone();
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        self.creation_response("control").await;
        if self.st.lock().unwrap().control_binding_missing {
            return Ok(None);
        }
        crate::SandboxControlIncarnation::kubernetes_pod("fake-control-incarnation")
            .map(Some)
            .map_err(|error| RuntimeError::Backend(error.to_string()))
    }

    async fn open_sandbox_control_channel(
        &self,
        _container_id: &str,
        _binding: &crate::SandboxControlIncarnation,
        _kind: SandboxControlServiceKind,
    ) -> Result<Box<dyn AgentChannel>, RuntimeError> {
        let pending = self.st.lock().unwrap().control_open_pending.clone();
        if let Some((entered, dropped)) = pending {
            struct PendingOpen(Arc<AtomicBool>);
            impl Drop for PendingOpen {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _pending = PendingOpen(dropped);
            entered.notify_one();
            std::future::pending::<()>().await;
        }
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

    fn uses_host_live_input_bind(&self) -> bool {
        self.st.lock().unwrap().host_live_input_bind
    }

    async fn recovered_host_staging_root(
        &self,
        _id: &str,
    ) -> Result<Option<std::path::PathBuf>, RuntimeError> {
        let (staging, gate) = {
            let mut state = self.st.lock().unwrap();
            state.recovered_staging_calls += 1;
            (
                state.recovered_staging.clone(),
                state.staging_response_gate.clone(),
            )
        };
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        Ok(staging)
    }

    fn uses_host_bind_materialization(&self) -> bool {
        !self.st.lock().unwrap().native_memory
    }

    async fn preflight_create_for_effect(
        &self,
        _context: &ContainerRealizationContext<'_>,
        _plan: &ContainerPlan,
        realization_fingerprint: Option<&pc::SandboxRealizationFingerprint>,
    ) -> Result<(), RuntimeError> {
        self.creation_response(if realization_fingerprint.is_some() {
            "resolved-preflight"
        } else {
            "initial-preflight"
        })
        .await;
        Ok(())
    }

    async fn prepare_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        _realization_fingerprint: &pc::SandboxRealizationFingerprint,
    ) -> Result<PreparedContainer, RuntimeError> {
        if let Some(fence) = context.effect_fence {
            self.st.lock().unwrap().effect_fences.push(fence.clone());
        }
        let expires = {
            let mut state = self.st.lock().unwrap();
            std::mem::take(&mut state.expire_effect_fence_once)
        };
        if expires {
            return Err(RuntimeError::EffectFenceExpired);
        }
        let recovered = {
            let state = self.st.lock().unwrap();
            if state.recover_prepared_reservations {
                state
                    .prepared_attempts
                    .get(context.scope)
                    .map(|attempt_id| {
                        (
                            attempt_id.clone(),
                            state.active_reservations.contains(context.scope),
                        )
                    })
            } else {
                None
            }
        };
        let result = if let Some((attempt_id, active)) = recovered {
            Ok(if active {
                PreparedContainer::already_active(format!("cid-{}", context.scope), attempt_id)
            } else {
                PreparedContainer::recovering(format!("cid-{}", context.scope), attempt_id)
            })
        } else {
            match self.create(context.scope, plan).await {
                Ok(locator) => {
                    self.st.lock().unwrap().prepared_attempts.insert(
                        context.scope.to_owned(),
                        context.attempt.as_str().to_owned(),
                    );
                    Ok(PreparedContainer::fresh(
                        locator,
                        context.attempt.as_str().to_owned(),
                    ))
                }
                Err(error) => Err(error),
            }
        };
        self.creation_response("create").await;
        result
    }

    async fn activate_for_effect(
        &self,
        context: &ContainerRealizationContext<'_>,
        plan: &ContainerPlan,
        _realization_fingerprint: &pc::SandboxRealizationFingerprint,
        prepared_locator: &str,
    ) -> Result<String, RuntimeError> {
        let failure = {
            let mut state = self.st.lock().unwrap();
            state
                .created_binds
                .insert(prepared_locator.to_owned(), plan.binds.clone());
            state
                .created_memory_mounts
                .insert(prepared_locator.to_owned(), plan.memory_mounts.clone());
            state.activation_calls += 1;
            match state.activation_failure.take() {
                Some(ActivationFailure::BeforeEffect(error)) => return Err(error),
                failure => {
                    state.active_reservations.insert(context.scope.to_owned());
                    failure
                }
            }
        };
        self.creation_response("activation").await;
        if let Some(ActivationFailure::AfterEffect(error)) = failure {
            return Err(error);
        }
        Ok(prepared_locator.to_owned())
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
        let (result, gate) = {
            let mut state = self.st.lock().unwrap();
            state.live_credential_reads += 1;
            let result = match &state.live_credential_error {
                Some(error) => Err(RuntimeError::Backend(error.clone())),
                None => Ok(state.live_credential.clone()),
            };
            (result, state.live_credential_response_gate.clone())
        };
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        result
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
        st.attempted_binds = plan.binds.clone();
        if st.recover_prepared_reservations {
            st.recovered_staging = plan
                .binds
                .iter()
                .find(|bind| bind.mount_path == LIVE_INPUTS_ROOT)
                .and_then(|bind| std::path::Path::new(&bind.source_ref).parent())
                .map(std::path::Path::to_path_buf);
        }
        if st.may_have_committed_create {
            return Err(RuntimeError::MayHaveCommitted(
                "injected create response loss".into(),
            ));
        }
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

    async fn recover_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<Option<RuntimeRestoreTarget>, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(RuntimeError::Backend(
                "fake restore target plan mismatch".into(),
            ));
        }
        let cid = format!("restore-{id}");
        let state = self.st.lock().unwrap();
        let Some(observed) = state.restore_targets.get(&cid) else {
            return Ok(None);
        };
        if observed != evidence {
            return Err(RuntimeError::Backend(
                "fake restore target evidence mismatch".into(),
            ));
        }
        if state
            .restore_plan_fingerprints
            .get(&cid)
            .map(String::as_str)
            != Some(plan_fingerprint)
        {
            return Err(RuntimeError::Backend(
                "fake physical restore plan mismatch".into(),
            ));
        }
        if state.alive.get(&cid) != Some(&true) {
            return Err(RuntimeError::Backend(
                "fake exact restore target is not alive".into(),
            ));
        }
        Ok(Some(RuntimeRestoreTarget {
            container_id: cid,
            disposition: pc::SandboxRestoreTargetDisposition::Recovered,
        }))
    }

    async fn restore_or_adopt(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
    ) -> Result<RuntimeRestoreTarget, RuntimeError> {
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(RuntimeError::Backend(
                "fake restore target plan mismatch".into(),
            ));
        }
        let cid = format!("restore-{id}");
        let mut state = self.st.lock().unwrap();
        if let Some(observed) = state.restore_targets.get(&cid) {
            if observed != evidence {
                return Err(RuntimeError::Backend(
                    "fake restore target evidence mismatch".into(),
                ));
            }
            if state
                .restore_plan_fingerprints
                .get(&cid)
                .map(String::as_str)
                != Some(plan_fingerprint)
            {
                return Err(RuntimeError::Backend(
                    "fake physical restore plan mismatch".into(),
                ));
            }
            return Ok(RuntimeRestoreTarget {
                container_id: cid,
                disposition: pc::SandboxRestoreTargetDisposition::Recovered,
            });
        }
        state.restore_targets.insert(cid.clone(), evidence.clone());
        state
            .restore_plan_fingerprints
            .insert(cid.clone(), plan_fingerprint.to_owned());
        state.restore_creates += 1;
        state.alive.insert(cid.clone(), true);
        state
            .created_command
            .insert(cid.clone(), plan.command.clone());
        state.created_env.insert(cid.clone(), plan.env.clone());
        state.created_binds.insert(cid.clone(), plan.binds.clone());
        state.created_images.insert(cid.clone(), plan.image.clone());
        Ok(RuntimeRestoreTarget {
            container_id: cid,
            disposition: pc::SandboxRestoreTargetDisposition::Created,
        })
    }

    async fn restoration_evidence(
        &self,
        container_id: &str,
    ) -> Result<Option<pc::SandboxRestorationEvidence>, RuntimeError> {
        let evidence = {
            let state = self.st.lock().unwrap();
            if state.alive.get(container_id) != Some(&true) {
                return Err(RuntimeError::Backend(
                    "fake restored target is not alive".into(),
                ));
            }
            state.restore_targets.get(container_id).cloned()
        };
        self.restore_query_response("evidence", container_id).await;
        Ok(evidence)
    }

    async fn restoration_plan_fingerprint(
        &self,
        container_id: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let fingerprint = self
            .st
            .lock()
            .unwrap()
            .restore_plan_fingerprints
            .get(container_id)
            .cloned();
        self.restore_query_response("plan", container_id).await;
        Ok(fingerprint)
    }

    async fn handle_extra(
        &self,
        container_id: &str,
    ) -> Result<Option<crate::ContainerContinuationHandle>, RuntimeError> {
        self.restore_query_response("handle", container_id).await;
        self.creation_response("handle").await;
        Ok(None)
    }

    async fn dispose_restore_target(
        &self,
        id: &str,
        plan: &ContainerPlan,
        plan_fingerprint: &str,
        evidence: &pc::SandboxRestorationEvidence,
        authorization: &pc::SandboxDisposalAuthorization,
        authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), RuntimeError> {
        authorization
            .validate()
            .map_err(|error| RuntimeError::Backend(error.to_string()))?;
        if restoration_plan_fingerprint(plan) != plan_fingerprint {
            return Err(RuntimeError::Backend(
                "fake restore cleanup plan mismatch".into(),
            ));
        }
        let admit = || {
            pc::with_current_sandbox_effect(authorization.effect_fence(), authority, |_| Ok(()))
                .map_err(|error| RuntimeError::Backend(error.to_string()))
        };
        admit()?;
        let gate = self
            .st
            .lock()
            .unwrap()
            .restore_disposal_observation_gate
            .clone();
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        admit()?;
        let cid = format!("restore-{id}");
        let response = {
            let mut state = self.st.lock().unwrap();
            state
                .restore_disposal_authorizations
                .push(authorization.clone());
            let Some(observed) = state.restore_targets.get(&cid) else {
                return Ok(());
            };
            if observed != evidence
                || state
                    .restore_plan_fingerprints
                    .get(&cid)
                    .map(String::as_str)
                    != Some(plan_fingerprint)
            {
                return Err(RuntimeError::Backend(
                    "fake restore cleanup identity mismatch".into(),
                ));
            }
            state.restore_targets.remove(&cid);
            state.restore_plan_fingerprints.remove(&cid);
            state.alive.remove(&cid);
            state.restore_disposals += 1;
            state.restore_disposal_response_gate.clone()
        };
        if let Some(gate) = response {
            let _permit = gate.acquire().await.unwrap();
        }
        // An admitted remote delete can return success after revocation. The
        // provider decision table must prove its OWN post-response rejection;
        // this fixture must not reject on its behalf and mask missing wiring.
        Ok(())
    }

    async fn observe(
        &self,
        expectation: crate::ContainerObservationExpectation<'_>,
    ) -> Result<pc::SandboxObservation, RuntimeError> {
        let (observation, gate) = {
            let mut state = self.st.lock().unwrap();
            state.observations += 1;
            state.observation_purposes.push(expectation.purpose);
            state
                .retained_observation_attempts
                .push(expectation.retained_attempt.map(|a| a.as_str().to_string()));
            let observation = state.observation.clone().unwrap_or_else(|| {
                if state.alive.get(expectation.container_id) == Some(&false) {
                    pc::SandboxObservation::DefinitivelyUnavailable {
                        physical_incarnation: Some(expectation.container_id.to_string()),
                    }
                } else {
                    pc::SandboxObservation::Ready
                }
            });
            (observation, state.observation_response_gate.clone())
        };
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        Ok(observation)
    }
    async fn spawn(
        &self,
        admitted: crate::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<Box<dyn pc::ProcessHandle>, RuntimeError> {
        let container_id = admitted.container_id();
        if !self.st.lock().unwrap().alive.contains_key(container_id) {
            return Err(RuntimeError::NotFound(container_id.into()));
        }
        if self.st.lock().unwrap().fail_spawn {
            return Err(RuntimeError::Backend("fake runtime refuses to exec".into()));
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
        admitted: crate::AdmittedExec,
        command: pc::MaterializedCommand,
    ) -> Result<AgentProcess, RuntimeError> {
        let container_id = admitted.container_id();
        let memory_archive = if command.argv.iter().any(|arg| arg == "read-tree-nofollow") {
            self.st.lock().unwrap().memory_archive.clone()
        } else {
            Vec::new()
        };
        let process = self
            .spawn(crate::AdmittedExec::unrecorded(container_id), command)
            .await?;
        let capacity = memory_archive.len().max(64);
        let (ours, mut peer) = tokio::io::duplex(capacity);
        tokio::spawn(async move {
            let _ = peer.write_all(&memory_archive).await;
        });
        Ok(AgentProcess {
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
        let result = {
            let mut state = self.st.lock().unwrap();
            state.inspections += 1;
            match state.alive.get(container_id) {
                Some(true) => Ok(ContainerState::Running),
                Some(false) => Ok(ContainerState::Gone),
                None => Err(RuntimeError::NotFound(container_id.into())),
            }
        };
        self.restore_query_response("inspect", container_id).await;
        result
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
        let gate = {
            let mut state = self.st.lock().unwrap();
            if let Some((_, dropped)) = &state.control_open_pending {
                assert!(
                    dropped.load(Ordering::SeqCst),
                    "B9/B10 setup must be reaped at the actual removal edge"
                );
            }
            let opens = state.control_channel_opens;
            state.removals.push((container_id.into(), opens));
            state.alive.insert(container_id.into(), false);
            state.removal_response_gate.clone()
        };
        // Pause only the response, after the fake backend really committed.
        // Revocation cannot retroactively undo this already-admitted removal.
        if let Some(gate) = gate {
            let _permit = gate.acquire().await.unwrap();
        }
        Ok(())
    }

    async fn remove_exact_incarnation(
        &self,
        container_id: &str,
        _runtime_handle: Option<&crate::ContainerContinuationHandle>,
        _authorization: &pc::SandboxDisposalAuthorization,
        _authority: &dyn pc::CurrentExecutionAuthority,
    ) -> Result<(), RuntimeError> {
        self.remove(container_id).await
    }
}

#[tokio::test]
async fn expired_effect_fence_rejects_before_runtime_or_mount_materialization() {
    /* Provider-effect cause/effect table. Causes: C1 fence live/expired; C2 a
     * spec that would otherwise require BlobSource staging and runtime create.
     * Effects: E1 expired rejects at the one provider boundary; E2 no runtime
     * observation/create and no materialized Sandbox is published. The live row
     * is covered by the fenced runtime decision tests; this test locks the
     * time-bound zero-effect rejection rather than accepting a later adapter
     * error as equivalent evidence. */
    let runtime = Arc::new(FakeRuntime::default());
    let provider = ContainerProvider::for_test(runtime.clone(), "agent:test");
    let expired =
        pc::SandboxEffectFence::new("operation-expired", "owner-1", "runtime-1", 1, 0).unwrap();
    let error = match provider
        .create_container_for_effect(
            &spec("expired-fence"),
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &expired,
                authority: &pc::test_support::EffectAuthority::denied(),
            },
            ContainerRealizationIntent::Create,
        )
        .await
    {
        Ok(_) => panic!("E1 expired fence must reject"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("expired"), "E1");
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.observations, 0, "E2 observation");
    assert!(state.alive.is_empty(), "E2 create");
}

// Inject response loss of authority at an existing dependency, without
// replacing its effect or ownership behavior with another fake implementation.
#[cfg(unix)]
struct PausedCreationInput<T> {
    inner: T,
    gate: Arc<tokio::sync::Semaphore>,
}

#[cfg(unix)]
#[async_trait]
impl<T: pc::BlobSource> pc::BlobSource for PausedCreationInput<T> {
    async fn get(&self, id: &str) -> Option<Vec<u8>> {
        let result = self.inner.get(id).await;
        self.gate.acquire().await.unwrap().forget();
        result
    }
}

#[cfg(unix)]
#[async_trait]
impl<T: pc::MemoryMounter> pc::MemoryMounter for PausedCreationInput<T> {
    async fn mount(
        &self,
        store: &str,
        path: &std::path::Path,
        access: pc::MountAccess,
    ) -> pc::MemoryMountOutcome {
        let result = self.inner.mount(store, path, access).await;
        self.gate.acquire().await.unwrap().forget();
        result
    }

    async fn recover_unknown_mount(
        &self,
        store: &str,
        path: &std::path::Path,
        access: pc::MountAccess,
    ) -> Result<(), pc::SandboxError> {
        self.inner.recover_unknown_mount(store, path, access).await
    }

    async fn reconcile_recovered_copy(
        &self,
        reference: &str,
        evidence: &awaken_provisioning_contract::MemoryMaterializationEvidence,
        files: &[(String, Vec<u8>)],
        access: awaken_provisioning_contract::MountAccess,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        let _ = (reference, evidence, files, access);
        self.inner
            .reconcile_recovered_copy(reference, evidence, files, access)
            .await
    }
}

#[cfg(unix)]
#[tokio::test]
async fn container_creation_rechecks_current_authority_at_provider_wait_boundaries() {
    // CA graph: entry -> scope preflight -> package -> resolved preflight ->
    // File response -> backend prepare response -> Memory/recovery response ->
    // activation -> handle response -> control response -> publication.
    // C1 boundary x C2 current proof {revoked, replaced, exhausted, renewed}
    // x C3 neutral/SessionSandbox/warm-pool port (144 rows).
    // A copied future fence is constant. Package/File/Memory responses exercise
    // actual dependencies, including already-created local participant bytes.
    // CA1 loss before prepare => Err, no physical creation; CA2 loss at/after
    // prepare => Err, no NEXT effect/publication/removal, exact backend-bound
    // staging retained; revoked authority cannot authorize compensation. CA3
    // same-effect renewal => one successful physical owner.
    // CA4 no proof at entry => zero runtime observations. CA5 an earlier
    // unknown Memory mount has a failed compensation behind a prepared backend;
    // recovery of that exact backend-bound path pauses. Loss on its response
    // denies a new mount/activation; renewal allows a fresh mount only after the
    // original recovery completed.
    // Gates pause real
    // futures; no sleeps or simulated scheduler. The backend create response
    // gate follows the physical fake mutation, so rejection cannot undo it.
    // This table proves provider call boundaries, not remote-command atomicity,
    // crash recovery, cancellation during a pending backend call, or absence.
    for port in ["neutral", "environment", "pool"] {
        for boundary in [
            "entry",
            "initial-preflight",
            "package",
            "resolved-preflight",
            "recovery",
            "file",
            "memory",
            "create",
            "staging",
            "activation",
            "handle",
            "control",
        ] {
            for change in ["revoke", "replace", "exhaust", "renew"] {
                let runtime = Arc::new(FakeRuntime {
                    st: Mutex::new(FakeState {
                        recover_prepared_reservations: true,
                        host_live_input_bind: true,
                        ..Default::default()
                    }),
                });
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                {
                    let mut state = runtime.st.lock().unwrap();
                    state.control_enabled = true;
                    if boundary == "staging" {
                        state.staging_response_gate = Some(gate.clone());
                    } else if boundary != "entry" {
                        state.creation_response_gate = Some((boundary, gate.clone()));
                    }
                }
                let mut provider = ContainerProvider::for_test(runtime.clone(), "agent:test");
                if boundary == "file" {
                    provider = provider.with_blob_source(Arc::new(PausedCreationInput {
                        inner: OneBlob("file-1", b"retained creation input".to_vec()),
                        gate: gate.clone(),
                    }));
                } else {
                    provider = provider.with_blob("file-1", b"retained creation input".to_vec());
                }
                let mut requested = spec("creation-current-authority");
                requested.mounts.truncate(1);
                requested.env.clear();
                if boundary == "package" {
                    provider =
                        provider.with_package_provisioner(Arc::new(RecordingPackageProvisioner {
                            response_gate: Some(gate.clone()),
                            ..Default::default()
                        }));
                    requested
                        .packages
                        .managers
                        .insert("pip".into(), vec!["fixture==1".into()]);
                }
                let torn_down = Arc::new(AtomicBool::new(false));
                if ["memory", "recovery"].contains(&boundary) {
                    provider.install_memory_mounter(Arc::new(PausedCreationInput {
                        inner: FakeMemoryMounter {
                            torn_down: torn_down.clone(),
                            fail_teardown: false,
                        },
                        gate: gate.clone(),
                    }));
                    requested.mounts.push(pc::MountRequirement {
                        mount_id: "memory".into(),
                        source: pc::MountSource::MemoryStore {
                            store_id: "store".into(),
                            materialization_reference: None,
                            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
                        },
                        mount_path: "/mnt/memory".into(),
                        access: pc::MountAccess::ReadWrite,
                        lifetime: pc::MountLifetime::Session,
                        required: true,
                    });
                }
                let recovered_calls = Arc::new(AtomicUsize::new(0));
                if boundary == "recovery" {
                    let mount_after_recovery = Arc::new(AtomicBool::new(false));
                    provider.install_memory_mounter(Arc::new(UncertainMemoryMounter {
                        mount_calls: Arc::new(AtomicUsize::new(0)),
                        recovery_calls: recovered_calls.clone(),
                        recovery_failures_remaining: Arc::new(AtomicUsize::new(1)),
                        recovery_response_gate: Some(gate.clone()),
                        mount_paths: Arc::new(Mutex::new(Vec::new())),
                        recovery_paths: Arc::new(Mutex::new(Vec::new())),
                        mount_after_recovery: Some(mount_after_recovery),
                        torn_down: torn_down.clone(),
                    }));
                    assert!(
                        provider.create_container(&requested).await.is_err(),
                        "CA5 retain original unknown participant"
                    );
                    assert_eq!(recovered_calls.load(Ordering::SeqCst), 1);
                    assert_eq!(runtime.st.lock().unwrap().alive.len(), 1);
                }
                let provider = Arc::new(provider);
                let pool = WarmContainerPool::new(provider.clone(), 1);
                requested
                    .control_services
                    .insert(SandboxControlServiceKind::RepositoryGitCredential);
                let fence =
                    pc::SandboxEffectFence::new("create", "worker", "boot", 3, u64::MAX - 1)
                        .unwrap();
                let authority = pc::test_support::EffectAuthority::new(&fence);
                let change_authority = || {
                    *authority.current.lock().unwrap() = match change {
                        "revoke" => None,
                        "replace" => Some((
                            fence.with_persisted(|w| w.epoch = 4).unwrap(),
                            std::time::Duration::from_secs(1),
                        )),
                        "exhaust" => Some((fence.clone(), std::time::Duration::ZERO)),
                        "renew" => Some((
                            fence
                                .with_persisted(|w| w.expires_at_unix_ms = u64::MAX)
                                .unwrap(),
                            std::time::Duration::from_secs(2),
                        )),
                        _ => unreachable!(),
                    };
                };
                if boundary == "entry" {
                    change_authority();
                }
                let mut pending = Box::pin(async {
                    if port == "neutral" {
                        pc::SandboxProvider::claim_for_effect(
                            provider.as_ref(),
                            &requested,
                            awaken_provisioning_contract::SandboxEffect::Fenced {
                                fence: &fence,
                                authority: &authority,
                            },
                            awaken_provisioning_contract::SandboxRealizationIntent::Create,
                        )
                        .await
                        .map(|_| ())
                    } else {
                        let port: &dyn SandboxProvider<
                            Environment = dyn awaken_provisioning_contract::SessionSandbox,
                        > = if port == "pool" {
                            &pool
                        } else {
                            provider.as_ref()
                        };
                        port.claim_for_effect(
                            &requested,
                            awaken_provisioning_contract::SandboxEffect::Fenced {
                                fence: &fence,
                                authority: &authority,
                            },
                            awaken_provisioning_contract::SandboxRealizationIntent::Create,
                        )
                        .await
                        .map(|_| ())
                    }
                });
                if boundary != "entry" {
                    assert!(
                        matches!(
                            std::future::poll_fn(|cx| std::task::Poll::Ready(
                                std::future::Future::poll(pending.as_mut(), cx)
                            ))
                            .await,
                            std::task::Poll::Pending
                        ),
                        "{port}/{boundary}/{change}: actual Pending"
                    );
                    change_authority();
                    gate.add_permits(1);
                }
                let result = pending.await;
                if boundary == "recovery" {
                    assert_eq!(
                        recovered_calls.load(Ordering::SeqCst),
                        2,
                        "CA5 resume the exact retained recovery"
                    );
                }
                assert_eq!(
                    result.is_ok(),
                    change == "renew",
                    "{port}/{boundary}/{change}: {result:?}"
                );
                let after_prepare = [
                    "memory",
                    "recovery",
                    "create",
                    "staging",
                    "activation",
                    "handle",
                    "control",
                ]
                .contains(&boundary);
                {
                    let state = runtime.st.lock().unwrap();
                    assert_eq!(
                        state.alive.len(),
                        usize::from(change == "renew" || after_prepare),
                        "{port}/{boundary}/{change}"
                    );
                    assert!(state.removals.is_empty(), "CA2 never manufacture cleanup");
                    if after_prepare && change != "renew" {
                        let binds = state.created_binds.values().next().unwrap();
                        assert_eq!(
                            std::fs::read(&binds[0].source_ref).unwrap(),
                            b"retained creation input",
                            "CA2 preserve the exact physical input after response-time revocation"
                        );
                    }
                    if change != "renew" {
                        if boundary == "entry" {
                            assert!(state.creation_boundaries.is_empty(), "CA4");
                        } else {
                            let last = match boundary {
                                "package" => "initial-preflight",
                                "file" => "resolved-preflight",
                                "memory" | "recovery" | "staging" => "create",
                                other => other,
                            };
                            assert_eq!(
                                state.creation_boundaries.last(),
                                Some(&last),
                                "CA1/CA2 no next call"
                            );
                        }
                        if boundary == "memory" {
                            assert!(
                                !torn_down.load(Ordering::SeqCst),
                                "CA2 revoked authority cannot authorize compensation"
                            );
                        }
                    }
                }
                if change != "renew" && after_prepare {
                    // The backend reservation/active object, not a process-local
                    // ledger, is the next call's sole recovery root.
                    runtime.st.lock().unwrap().staging_response_gate = None;
                    gate.add_permits(1);
                    let replay = provider.create_container(&requested).await;
                    assert!(
                        replay.is_ok(),
                        "CA2 exact backend recovery remains available"
                    );
                    assert_eq!(
                        runtime.st.lock().unwrap().alive.len(),
                        1,
                        "CA2 no extra physical object"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn container_effect_retries_only_with_a_live_strict_same_effect_renewal() {
    /* Renewable provider-boundary cause/effect table. Causes: C1 the runtime
     * crosses an idempotent boundary and reports the asserted fence expired;
     * C2 the sole Session authority source returns a live strict successor,
     * no source, or a foreign operation; C3 the effect identity and staged
     * inputs remain unchanged. Effects: E1 replay the same runtime effect once
     * with the newer expiry and publish one Sandbox; E2 missing/foreign proof
     * fails closed without a second runtime call; E3 no cleanup is inferred
     * after the first call may have created physical participants.
     *
     * | Rule | expired boundary | refreshed proof | Effect |
     * | R1 | yes | live strict same effect | E1 + E3 |
     * | R2 | any | missing | entry denial, zero runtime calls |
     * | R3 | any | foreign/regressed | entry denial, zero runtime calls |
     * | R3-equal | yes | equal | E2 + E3 after one runtime call |
     */
    let now = crate::effect_clock_reading_ms(Some(&awaken_time::system_clock())).unwrap();
    let initial = pc::SandboxEffectFence::new(
        "renewable-create",
        "worker-a",
        "worker-a/incarnation",
        7,
        now.saturating_add(10_000),
    )
    .unwrap();
    let renewed = pc::SandboxEffectFence::new(
        initial.operation_id(),
        initial.owner(),
        initial.runtime_incarnation(),
        initial.epoch(),
        u64::MAX,
    )
    .unwrap();
    let runtime = Arc::new(FakeRuntime::default().expiring_effect_fence_once());
    let provider = ContainerProvider::for_test(runtime.clone(), "agent:test");
    let mut renewable_spec = spec("renewed-provider-boundary");
    renewable_spec.mounts.clear();
    renewable_spec.env.clear();
    let source = pc::test_support::EffectAuthority::new(&renewed);
    provider
        .create_container_for_effect(
            &renewable_spec,
            awaken_provisioning_contract::SandboxEffect::Fenced {
                fence: &initial,
                authority: &source,
            },
            ContainerRealizationIntent::Create,
        )
        .await
        .expect("R1 strict renewal replays the exact effect");
    assert_eq!(
        runtime.st.lock().unwrap().effect_fences,
        vec![initial.clone(), renewed],
        "R1/E1 exact old then renewed fence"
    );
    assert_eq!(runtime.st.lock().unwrap().alive.len(), 1, "R1/E1");

    for (rule, source) in [
        ("R2", None),
        ("R3-equal", Some(initial.clone())),
        (
            "R3-regressed",
            Some(
                initial
                    .with_persisted(|w| w.expires_at_unix_ms = initial.expires_at_unix_ms() - 1)
                    .unwrap(),
            ),
        ),
        (
            "R3",
            Some(
                pc::SandboxEffectFence::new(
                    "foreign-create",
                    initial.owner(),
                    initial.runtime_incarnation(),
                    initial.epoch(),
                    u64::MAX,
                )
                .unwrap(),
            ),
        ),
    ] {
        let runtime = Arc::new(FakeRuntime::default().expiring_effect_fence_once());
        let provider = ContainerProvider::for_test(runtime.clone(), "agent:test");
        let mut rejected_spec = spec(&format!("rejected-provider-boundary-{rule}"));
        rejected_spec.mounts.clear();
        rejected_spec.env.clear();
        // R2 (no current Session authority) is now unrepresentable as a
        // fenced effect; the closest executable row is an authority that
        // admits nothing.
        let source = source.as_ref().map_or_else(
            pc::test_support::EffectAuthority::denied,
            pc::test_support::EffectAuthority::new,
        );
        let error = match provider
            .create_container_for_effect(
                &rejected_spec,
                awaken_provisioning_contract::SandboxEffect::Fenced {
                    fence: &initial,
                    authority: &source,
                },
                ContainerRealizationIntent::Create,
            )
            .await
        {
            Ok(_) => panic!("R2/R3 invalid renewal must fail closed"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("current Session authority")
                || error.to_string().contains("strict successor")
                || error.to_string().contains("permission denied"),
            "{rule}/E2: {error}"
        );
        assert_eq!(
            runtime.st.lock().unwrap().effect_fences.len(),
            usize::from(rule == "R3-equal"),
            "{rule}/E2: missing/foreign proof rejects before the first call"
        );
        assert!(runtime.st.lock().unwrap().alive.is_empty(), "{rule}/E2");
    }
}

#[derive(Default)]
struct RecordingSecretBroker {
    current: Mutex<Vec<u8>>,
    writes: Mutex<Vec<Vec<u8>>>,
    effect_operations: Mutex<Vec<String>>,
    writeback_ids: Mutex<Vec<String>>,
    reject_writeback: AtomicBool,
    writeback_response_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl RecordingSecretBroker {
    fn record_writeback(&self, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        if self.reject_writeback.load(Ordering::SeqCst) {
            return Err(pc::SandboxError::unauthorized(
                "injected write-back rejection",
            ));
        }
        *self.current.lock().unwrap() = bytes.clone();
        self.writes.lock().unwrap().push(bytes);
        Ok(())
    }
}

#[async_trait]
impl pc::SecretBroker for RecordingSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn write_back(
        &self,
        target: pc::SecretWritebackTarget<'_>,
        bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        self.record_writeback(bytes)?;
        let pc::SecretWritebackTarget::Effect(effect) = target else {
            return Ok(());
        };
        self.effect_operations
            .lock()
            .unwrap()
            .push(effect.authorization().operation_id().to_owned());
        self.writeback_ids
            .lock()
            .unwrap()
            .push(effect.writeback_id().to_string());
        let gate = self.writeback_response_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
}

fn writable_credential_spec(scope: &str) -> pc::SandboxSpec {
    let mut sandbox_spec = spec(scope);
    sandbox_spec.network = pc::NetworkPolicy::Unrestricted;
    sandbox_spec.mounts = vec![writable_credential_mount()];
    sandbox_spec
}

fn writable_credential_mount() -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: "native-auth".into(),
        source: pc::MountSource::Secret {
            reference: "credential://acp/native/claude".into(),
            content_hash: None,
        },
        mount_path: "/acp-config/.credentials.json".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    }
}

fn base_provider(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    // One immutable provider composition is shared by creation and fresh-process
    // recovery. The conventional proxy supplies connectivity without claiming to
    // enforce a host allowlist.
    ContainerProvider::for_test(runtime, "ghcr.io/awaken/sandbox:latest").with_forward_proxy(
        ForwardProxy {
            url: "http://gw.internal:8888".into(),
        },
    )
}

fn provider_without_broker(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    // Required mounts are seeded because a missing required source fails closed
    // at ordinary creation.
    base_provider(runtime)
        .with_blob("file-1", b"in-bytes".to_vec())
        .with_blob("res-9", b"work-bytes".to_vec())
}

fn provider(runtime: Arc<FakeRuntime>) -> ContainerProvider<FakeRuntime> {
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = b"container-process-secret".to_vec();
    provider_without_broker(runtime).with_secret_broker(broker)
}

fn restore_request(spec: &pc::SandboxSpec) -> pc::SandboxRestoreRequest {
    pc::SandboxRestoreRequest {
        workspace_id: "workspace-a".into(),
        session_id: spec.scope.clone(),
        effect_id: "blake3:0000000000000000000000000000000000000000000000000000000000000002".into(),
        generation_id: "generation-a".into(),
        checkpoint: pc::SandboxCheckpointRef {
            id: "checkpoint-a".into(),
            format: "provider-owned-checkpoint".into(),
            digest: "digest-a".into(),
            size_bytes: 7,
            created_at_unix_ms: 10,
            expires_at_unix_ms: 20,
            environment_fingerprint: "environment-a".into(),
            base_image_fingerprint: "image-a".into(),
            excluded_mounts: spec
                .mounts
                .iter()
                .map(|mount| mount.mount_path.clone())
                .collect(),
            suspend_effect_id: "suspend-a".into(),
        },
    }
}

/*
 * Exact container-target cause/effect decision table (R1-R4, R6-R7).
 * Causes: C1 target absent; C2 exact tuple already present; C3 fresh provider
 * object sharing only the physical runtime while creation Blob inputs are no
 * longer available; C4 concurrent callers; C5 same effect with mismatched
 * generation/checkpoint evidence; C6 handle evidence is stripped while
 * physical evidence remains.
 * Effects: E1 one physical creation; E2 one identical durable handle; E3 one
 * Created plus one Recovered disposition; E4 recover without provider-local
 * memory; E5 mismatch preserves target and fails closed; E6 request-owned
 * handle verification rejects stripped evidence while accepting the exact
 * handle.
 * C7 terminal cleanup carries the original request. E7 delete only the exact
 * target, with absent replay succeeding. Rules: R1=C1=>E1; R2=C2=>E2;
 * R3=C2+C3=>E2+E4; R4=C1+C4=>E1+E2+E3; R6=C5=>E5;
 * R7=C6=>E6; R8=C7=>E7.
 */
#[tokio::test]
async fn exact_restore_target_is_shared_by_concurrent_and_fresh_container_providers() {
    let runtime = Arc::new(FakeRuntime::default());
    let scope = "exact-restore";
    let sandbox_spec = spec(scope);
    let request = restore_request(&sandbox_spec);
    let left_provider = provider(runtime.clone());
    let right_provider = provider(runtime.clone());

    let (left, right) = tokio::join!(
        left_provider.acquire_restore_environment(&sandbox_spec, &request),
        right_provider.acquire_restore_environment(&sandbox_spec, &request),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    assert_eq!(
        [left.disposition(), right.disposition()]
            .into_iter()
            .filter(|disposition| { *disposition == pc::SandboxRestoreTargetDisposition::Created })
            .count(),
        1,
        "R4/E3 exactly one caller creates"
    );
    assert_eq!(left.target().handle(), right.target().handle(), "R4/E2");
    assert_eq!(runtime.st.lock().unwrap().restore_creates, 1, "R1/E1");
    let handle = left.target().handle();

    // The fresh provider retains the same immutable provider composition but no
    // BlobSource, seed, or SecretBroker: R3 proves physical lookup happens before
    // resolving the two required mount sources used only for creation.
    let fresh_provider = base_provider(runtime.clone());
    let replay = fresh_provider
        .acquire_restore_environment(&sandbox_spec, &request)
        .await
        .unwrap();
    assert_eq!(
        replay.disposition(),
        pc::SandboxRestoreTargetDisposition::Recovered,
        "R3/E4"
    );
    assert_eq!(replay.target().handle(), handle, "R3/E2");

    let mut mismatched = request.clone();
    mismatched.generation_id.push_str("-different");
    assert!(
        fresh_provider
            .acquire_restore_environment(&sandbox_spec, &mismatched)
            .await
            .is_err(),
        "R6/E5"
    );
    assert_eq!(runtime.st.lock().unwrap().restore_creates, 1, "R6/E5");

    request
        .verify_handle(&sandbox_spec, &handle)
        .expect("R7 exact evidence verifies");
    let stripped = pc::SandboxHandle::container(
        handle.sandbox_id.clone(),
        handle.container_payload().unwrap().clone(),
    );
    assert!(
        request.verify_handle(&sandbox_spec, &stripped).is_err(),
        "R7/E6 physical restore evidence cannot be stripped"
    );

    drop(left);
    drop(right);
    drop(replay);
    let authorization = disposal_authorization(
        &pc::SandboxEffectFence::new(
            "restore-terminal",
            "terminal-owner",
            "terminal-runtime",
            2,
            u64::MAX,
        )
        .unwrap(),
    );
    fresh_provider
        .dispose_restored_environment(
            &sandbox_spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("R8 exact terminal disposal");
    fresh_provider
        .dispose_restored_environment(
            &sandbox_spec,
            &request,
            &authorization,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                authorization.effect_fence(),
            ),
        )
        .await
        .expect("R8 absent replay");
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.restore_disposals, 1, "R8/E7");
    // R8A: both the first physical effect and response-loss replay forward
    // exactly the caller's typed preparation/successor authority.
    assert_eq!(
        state.restore_disposal_authorizations,
        vec![authorization.clone(), authorization]
    );
    assert!(state.restore_targets.is_empty(), "R8/E7");
}

#[tokio::test]
async fn malformed_restore_effect_is_rejected_before_every_substrate_effect() {
    // Pre-effect admission table R9: C1 the Session operation id is a legacy
    // opaque FNV value or malformed BLAKE3 syntax; C2 Phase-B acquisition is
    // requested. C1+C2 => E1 reject before recover/create/inspect/remove/spawn
    // and E2 leave the physical runtime empty. Existing durable operations may
    // replay in the aggregate, but rollout admission cannot activate them.
    for effect_id in [
        "fnv1a64:0123456789abcdef",
        "blake3:00",
        "blake3:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let runtime = Arc::new(FakeRuntime::default());
        let container_provider = provider(runtime.clone());
        let sandbox_spec = spec("malformed-restore");
        let mut request = restore_request(&sandbox_spec);
        request.effect_id = effect_id.into();
        assert!(
            container_provider
                .acquire_restore_environment(&sandbox_spec, &request)
                .await
                .is_err(),
            "R9/E1 {effect_id}"
        );
        let state = runtime.st.lock().unwrap();
        assert_eq!(state.restore_creates, 0, "R9/E1 {effect_id}");
        assert_eq!(state.restore_disposals, 0, "R9/E1 {effect_id}");
        assert!(state.restore_targets.is_empty(), "R9/E2 {effect_id}");
        assert!(state.alive.is_empty(), "R9/E2 {effect_id}");
        assert_eq!(state.inspections, 0, "R9/E1 {effect_id}");
        assert!(state.spawned.is_empty(), "R9/E1 {effect_id}");
        assert!(state.removals.is_empty(), "R9/E1 {effect_id}");
    }
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
     * C1=empty control demand; C2=typed demand on a capable runtime;
     * C3=spec-aware exact adoption; C4=fenced spec-aware exact adoption;
     * C5=requested/realized mismatch; C6=set/incarnation inconsistency;
     * C7=provider lacks the realized capability; C8=runtime omits an
     * incarnation. E1=no callback and unchanged ordinary creation; E2=one new
     * binding and exact topology persisted; E3=adopt with that exact topology;
     * E4=fail closed before ambient inference; E5=a missing post-create binding
     * retains the exact possibly-committed runtime object for root-driven retry
     * and performs no compensating removal. Rules: B1 C1=>E1; B2 C2=>E2;
     * B3 C2+(C3|C4)=>E3; B4 C5|C6|C7=>E4; B5 C8=>E5.
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
            .and_then(crate::SandboxControlIncarnation::kubernetes_pod_uid),
        Some("fake-control-incarnation"),
        "B2/E2 persisted"
    );
    let exact = SandboxProvider::adopt_for_effect(
        &container_provider,
        SandboxAdoption::new(&demanded, &handle),
        awaken_provisioning_contract::SandboxEffect::Unattached,
    )
    .await
    .expect("B3/E3 exact adoption");
    // A4: reconstructed native participants are independently recoverable;
    // adoption must still never mint the departed creator's in-memory proof.
    assert!(
        exact.retained_create_attempt().is_none(),
        "A4 no creator proof after adoption"
    );
    assert_eq!(
        recovery::decode_handle(&exact.handle())
            .unwrap()
            .control_services,
        demanded.control_services,
        "B3/E3"
    );
    let adoption_fence = SandboxEffectFence::new(
        "control-adoption",
        "worker-control",
        "worker-control:1:boot",
        1,
        u64::MAX,
    )
    .unwrap();
    SandboxProvider::adopt_for_effect(
        &container_provider,
        SandboxAdoption::new(&demanded, &handle),
        awaken_provisioning_contract::SandboxEffect::Fenced {
            fence: &adoption_fence,
            authority: &pc::test_support::EffectAuthority::new(&adoption_fence),
        },
    )
    .await
    .expect("B4/E3 fenced exact spec");
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
        crate::ContainerSandboxHandleV1 {
            sandbox_control_incarnation: None,
            ..payload.clone()
        },
    );
    assert!(
        SandboxProvider::adopt_for_effect(
            &container_provider,
            SandboxAdoption::new(&demanded, &missing_incarnation),
            awaken_provisioning_contract::SandboxEffect::Unattached
        )
        .await
        .is_err(),
        "B6/E4 realized set without incarnation"
    );

    let missing_set = pc::SandboxHandle::container(
        &handle.sandbox_id,
        crate::ContainerSandboxHandleV1 {
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
        SandboxProvider::adopt_for_effect(
            &container_provider,
            SandboxAdoption::new(&ordinary, &handle),
            awaken_provisioning_contract::SandboxEffect::Unattached
        )
        .await
        .is_err(),
        "B5/E4 requested narrower"
    );
    let legacy = pc::SandboxHandle::container(
        &handle.sandbox_id,
        crate::ContainerSandboxHandleV1 {
            sandbox_control_incarnation: None,
            control_services: Default::default(),
            ..payload.clone()
        },
    );
    assert!(
        SandboxProvider::adopt_for_effect(
            &container_provider,
            SandboxAdoption::new(&demanded, &legacy),
            awaken_provisioning_contract::SandboxEffect::Unattached
        )
        .await
        .is_err(),
        "B5/E4 legacy empty handle plus new demand"
    );
    let unsupported_provider = provider(Arc::new(FakeRuntime::default()));
    assert!(
        SandboxProvider::adopt_for_effect(
            &unsupported_provider,
            SandboxAdoption::new(&demanded, &handle),
            awaken_provisioning_contract::SandboxEffect::Unattached
        )
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
    let missing_state = missing_runtime.st.lock().unwrap();
    assert!(
        missing_state.removals.is_empty(),
        "B8/E5 indeterminate post-create result is not compensated"
    );
    assert_eq!(
        missing_state.alive.get("cid-demanded-control"),
        Some(&true),
        "B8/E5 exact runtime object remains recoverable"
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
    awaken_sandbox_control_transport::write_frame(
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
        awaken_sandbox_control_transport::read_frame::<
            _,
            awaken_sandbox_control::SandboxControlResponse,
        >(peer)
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

    sandbox.dispose().await.expect("B7 dispose");
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

#[tokio::test]
async fn control_disposal_and_publisher_cancellation_reap_initial_channel_setup() {
    // B9 setup is awaiting its first runtime channel, before a publication
    // handle has been returned. B10 caller is cancelled at that await.
    // Cause/effect rows: B9+dispose => setup future dropped before removal;
    // B9+B10+dispose => same, no lost reservation or detached setup task.
    // In both rows: zero channels opened, exactly one removal, no later publish.
    for cancel_publisher in [false, true] {
        let runtime = Arc::new(FakeRuntime::default().with_sandbox_control());
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        runtime.st.lock().unwrap().control_open_pending = Some((entered.clone(), dropped.clone()));
        let mut demanded = spec("publication-setup");
        demanded
            .control_services
            .insert(SandboxControlServiceKind::RepositoryGitCredential);
        let sandbox = Arc::new(
            provider(runtime.clone())
                .create_container(&demanded)
                .await
                .unwrap(),
        );
        let publishing = tokio::spawn({
            let sandbox = sandbox.clone();
            async move {
                sandbox
                    .publish_sandbox_control_service(
                        SandboxControlServiceKind::RepositoryGitCredential,
                        Arc::new(UnavailableControlService),
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), entered.notified())
            .await
            .expect("timed out waiting for entered.notified()");
        if cancel_publisher {
            publishing.abort();
        }
        sandbox.dispose().await.unwrap();
        assert!(
            dropped.load(Ordering::SeqCst),
            "B9/B10 setup reaped before disposal return"
        );
        let result = publishing.await;
        if cancel_publisher {
            assert!(result.err().unwrap().is_cancelled());
        } else {
            assert!(result.unwrap().is_err());
        }
        {
            let state = runtime.st.lock().unwrap();
            assert_eq!(state.control_channel_opens, 0);
            assert_eq!(state.removals, vec![("cid-publication-setup".into(), 0)]);
        }
        assert!(
            sandbox
                .publish_sandbox_control_service(
                    SandboxControlServiceKind::RepositoryGitCredential,
                    Arc::new(UnavailableControlService),
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn adoption_configuration_is_revalidated_before_runtime_observation() {
    /* Provider-effective adoption cause/effect table.
     * Causes: C1 durable handle is current V2/legacy V1; C2 current effective
     * spec+egress+runtime+resident configuration is exact/different; C3 backend
     * observation would report Ready. Effects: E1 exact V2 reaches the backend
     * once and returns Ready; E2 different V2 returns Incompatible with zero
     * backend I/O; E3 legacy remains continuity-only and is governed by the
     * runtime observer. Rules exercised here: A1 V2+exact+C3=>E1; A2
     * V2+different+C3=>E2. Runtime observation tests own E3. */
    let runtime = Arc::new(FakeRuntime::default());
    let provider = provider(runtime.clone());
    let exact = spec("adoption-configuration");
    let sandbox = provider.create_container(&exact).await.unwrap();
    let handle = pc::Sandbox::handle(&sandbox);

    assert_eq!(
        SandboxProvider::observe(
            &provider,
            SandboxAdoption::new(&exact, &handle),
            awaken_provisioning_contract::SandboxObservationBasis::Unattached
        )
        .await
        .unwrap(),
        pc::SandboxObservation::Ready,
        "A1"
    );
    assert_eq!(runtime.st.lock().unwrap().observations, 1, "A1 one I/O");

    let mut drifted = exact.clone();
    drifted.outputs_path = "/mnt/session/drifted-outputs".into();
    // A2 diagnostic evidence carries only the two equality fingerprints, not
    // raw spec/paths/environment contents. This changes no admission transition:
    // mismatch still rejects before backend observation and cannot clear a slot.
    let pc::SandboxObservation::Incompatible { reason } = SandboxProvider::observe(
        &provider,
        SandboxAdoption::new(&drifted, &handle),
        awaken_provisioning_contract::SandboxObservationBasis::Unattached,
    )
    .await
    .unwrap() else {
        panic!("A2 mismatch must reject");
    };
    let stored = handle.container_adoption_fingerprint().unwrap().unwrap();
    assert!(reason.ends_with(&format!(", stored={stored})")));
    assert_eq!(reason.matches("fnv1a64:").count(), 2);
    assert!(!reason.contains(&drifted.outputs_path));
    assert!(!reason.contains(&exact.scope));
    assert_eq!(runtime.st.lock().unwrap().observations, 1, "A2 zero I/O");
    // A3 the creating object holds the original attempt (never durable handle
    // data); both fenced/unfenced observation share fingerprint validation and
    // pass precisely this proof. A4 an adopted wrapper cannot mint a new one.
    let mut retained = SandboxAdoption::new(&exact, &handle);
    retained.retained_attempt = sandbox.retained_create_attempt();
    assert!(
        retained.retained_attempt.is_some(),
        "A3 creator retains proof"
    );
    SandboxProvider::observe(
        &provider,
        retained,
        awaken_provisioning_contract::SandboxObservationBasis::Unattached,
    )
    .await
    .unwrap();
    let fence = SandboxEffectFence::new("observe", "owner", "runtime", 1, u64::MAX).unwrap();
    SandboxProvider::observe(&provider, retained, retained.effect_basis(&fence))
        .await
        .unwrap();
    let attempts = runtime
        .st
        .lock()
        .unwrap()
        .retained_observation_attempts
        .clone();
    let expected = sandbox
        .retained_create_attempt()
        .unwrap()
        .as_str()
        .to_string();
    assert_eq!(
        attempts,
        [None, Some(expected.clone()), Some(expected.clone())],
        "A3 one observer"
    );
    // A5 the same creator proceeds from admitted preparation to physical
    // disposal: its exact proof must reach that second runtime observation,
    // then removal happens once. A6 response replay adds neither observation
    // nor removal. The shared runtime matrix still owns cold/wrong-proof and
    // foreign/expired-fence rejection; preparation alone never grants deletion.
    pc::Sandbox::prepare_disposal_for_effect(
        &sandbox,
        &fence,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(&fence),
    )
    .await
    .unwrap();
    let authorization = disposal_authorization(&fence);
    pc::Sandbox::dispose_for_effect(
        &sandbox,
        &authorization,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(
            authorization.effect_fence(),
        ),
    )
    .await
    .unwrap();
    pc::Sandbox::dispose_for_effect(
        &sandbox,
        &authorization,
        &awaken_provisioning_contract::test_support::EffectAuthority::new(
            authorization.effect_fence(),
        ),
    )
    .await
    .unwrap();
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.retained_observation_attempts.len(), 4, "A5/A6");
    // A7 ordinary unfenced/fenced observations remain Recovery; only disposal
    // changes purpose. Replay adds no observation. This independent wire oracle
    // prevents both global TerminalCleanup defaulting and loss of the disposal
    // override when current-authority checks and main's classifier are composed.
    assert_eq!(
        state.observation_purposes,
        [
            SandboxObservationPurpose::Recovery,
            SandboxObservationPurpose::Recovery,
            SandboxObservationPurpose::Recovery,
            SandboxObservationPurpose::TerminalCleanup,
        ],
        "A7 one observer, distinct recovery and cleanup semantics"
    );
    assert_eq!(
        state.retained_observation_attempts.last(),
        Some(&Some(expected)),
        "A5 exact creator proof"
    );
    assert_eq!(
        state.removals.len(),
        1,
        "A6 disposal replay has no extra effect"
    );
}

#[tokio::test]
async fn observation_purpose_reaches_runtime_without_granting_cleanup() {
    // O1 ordinary constructor => Recovery. O2 explicit terminal observation
    // => TerminalCleanup through the same fenced/unfenced observer. O3 both
    // purposes remain subject to exact adoption configuration; drift => no I/O.
    // All rows are read-only: zero removals and no cleanup authorization minted.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = provider(runtime.clone());
    let exact = spec("purpose-propagation");
    let sandbox = provider.create_container(&exact).await.unwrap();
    let handle = pc::Sandbox::handle(&sandbox);
    let fence = SandboxEffectFence::new("observe", "owner", "runtime", 1, u64::MAX).unwrap();
    for purpose in [
        SandboxObservationPurpose::Recovery,
        SandboxObservationPurpose::TerminalCleanup,
    ] {
        for fenced in [false, true] {
            let mut adoption = SandboxAdoption::new(&exact, &handle);
            assert_eq!(
                adoption.observation_purpose,
                SandboxObservationPurpose::Recovery
            );
            adoption.observation_purpose = purpose;
            if fenced {
                SandboxProvider::observe(&provider, adoption, adoption.effect_basis(&fence))
                    .await
                    .unwrap();
            } else {
                SandboxProvider::observe(
                    &provider,
                    adoption,
                    awaken_provisioning_contract::SandboxObservationBasis::Unattached,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                runtime.st.lock().unwrap().observation_purposes.last(),
                Some(&purpose)
            );
            let mut drifted = exact.clone();
            drifted.outputs_path = "/different".into();
            adoption.spec = &drifted;
            let before = runtime.st.lock().unwrap().observations;
            let observation =
                SandboxProvider::observe(&provider, adoption, adoption.effect_basis(&fence))
                    .await
                    .unwrap();
            assert!(matches!(
                observation,
                pc::SandboxObservation::Incompatible { .. }
            ));
            assert_eq!(runtime.st.lock().unwrap().observations, before);
            assert!(runtime.st.lock().unwrap().removals.is_empty());
        }
    }
}

#[tokio::test]
async fn historical_configuration_is_only_an_exact_fenced_terminal_candidate() {
    // HC1 current mismatch + exact historical candidate + terminal + fence =>
    // existing runtime observer decides, not this comparison. HC2 any missing
    // condition => Incompatible before I/O; HC3 drift in immutable spec remains
    // rejected even with a historical runtime candidate. No deletion/activation.
    let runtime = Arc::new(FakeRuntime::default());
    let original = runtime.realization_configuration().unwrap();
    let provider = provider(runtime.clone());
    let spec = spec("historical-configuration");
    let created = provider.create_container(&spec).await.unwrap();
    let handle = pc::Sandbox::handle(&created);
    let mut changed = original.clone();
    changed.insert("storage".into(), "new".into());
    runtime.st.lock().unwrap().configuration = Some(changed);
    let fence = SandboxEffectFence::new("cleanup", "owner", "boot", 1, u64::MAX).unwrap();
    for candidate in [false, true] {
        runtime.st.lock().unwrap().historical_configuration = candidate.then(|| original.clone());
        for terminal in [false, true] {
            for fenced in [false, true] {
                for drift in [false, true] {
                    let mut requested = spec.clone();
                    if drift {
                        requested.outputs_path = "/other".into();
                    }
                    let mut adoption = SandboxAdoption::new(&requested, &handle);
                    if terminal {
                        adoption.observation_purpose = SandboxObservationPurpose::TerminalCleanup;
                    }
                    let before = runtime.st.lock().unwrap().observations;
                    let result = if fenced {
                        SandboxProvider::observe(&provider, adoption, adoption.effect_basis(&fence))
                            .await
                            .unwrap()
                    } else {
                        SandboxProvider::observe(
                            &provider,
                            adoption,
                            awaken_provisioning_contract::SandboxObservationBasis::Unattached,
                        )
                        .await
                        .unwrap()
                    };
                    let admitted = candidate && terminal && fenced && !drift;
                    assert_eq!(matches!(result, pc::SandboxObservation::Ready), admitted);
                    assert_eq!(
                        runtime.st.lock().unwrap().observations,
                        before + u32::from(admitted)
                    );
                    assert!(runtime.st.lock().unwrap().removals.is_empty());
                }
            }
        }
    }
}

#[tokio::test]
async fn restored_adoption_rechecks_original_authority_after_each_query_response() {
    use pc::test_support::EffectAuthority;
    use std::time::Duration;

    // Cause/effect graph: exact frozen restore request + original current proof
    // -> inspect -> evidence -> plan -> continuation handle -> publish handle.
    // Every async arrow can suspend; authority loss after any response must cut
    // off the NEXT query or publication, without disposing/recreating the target.
    // Decision table: C1 concrete or object-erased view of the one provider port; C2 each of four response
    // cuts; C3 unchanged/renewed/revoked/replaced/exhausted/cancelled authority;
    // C4 past/future Control expiry (not a Worker clock oracle).
    // R1 denied entry => zero I/O. R2 unchanged/renewed => exact handle success.
    // R3 revoked/replaced/exhausted => error and exact query prefix only.
    // R4 cancellation => same prefix, no invented cleanup; an explicitly
    // restored fixture authority can retry the SAME request/target. R5 every
    // row preserves physical evidence and performs no create/remove,
    // process start, lease renewal or control-channel activation during adoption.
    // Full 2x4x6x2 Pending schedules. The cold provider has no Blob/Secret source;
    // resource creation/recovery stays with the existing restore-target owner.
    // SessionExecutionBinding's adoption wait/re-entry invariant is reused, not
    // a second lifecycle model. Its bounded two-effect proof is NOT a proof of
    // this four-query Rust refinement, real daemon behavior or remote atomicity.
    const QUERIES: [&str; 4] = ["inspect", "evidence", "plan", "handle"];
    for port in ["concrete", "erased"] {
        for expiry in [100, u64::MAX - 1] {
            for (cut, boundary) in QUERIES.into_iter().enumerate() {
                for change in [
                    "unchanged",
                    "renew",
                    "revoke",
                    "replace",
                    "expire",
                    "cancel",
                ] {
                    let runtime = Arc::new(FakeRuntime::default());
                    let spec = spec("restored-adoption-current");
                    let request = restore_request(&spec);
                    let creator = provider(runtime.clone());
                    let created = creator
                        .acquire_restore_environment(&spec, &request)
                        .await
                        .unwrap();
                    let handle = created.target().handle();
                    let id = handle.container_payload().unwrap().container_id.clone();
                    drop(created);
                    drop(creator);
                    let provider = base_provider(runtime.clone());
                    let fence =
                        pc::SandboxEffectFence::new("restore-adopt", "owner", "boot", 8, expiry)
                            .unwrap();
                    let authority = EffectAuthority::new(&fence);
                    let call = |authority| -> std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = Result<pc::SandboxHandle, pc::SandboxError>,
                                > + Send
                                + '_,
                        >,
                    > {
                        let (provider, spec, handle, fence) = (&provider, &spec, &handle, &fence);
                        Box::pin(async move {
                            if port == "concrete" {
                                pc::SandboxProvider::adopt_for_effect(
                                    provider,
                                    awaken_provisioning_contract::SandboxAdoption::new(
                                        spec, handle,
                                    ),
                                    awaken_provisioning_contract::SandboxEffect::Fenced {
                                        fence,
                                        authority,
                                    },
                                )
                                .await
                                .map(|sandbox| sandbox.handle())
                            } else {
                                provider
                                    .adopt_for_effect(
                                        SandboxAdoption::new(spec, handle),
                                        awaken_provisioning_contract::SandboxEffect::Fenced {
                                            fence,
                                            authority,
                                        },
                                    )
                                    .await
                                    .map(|environment| environment.handle())
                            }
                        })
                    };
                    let original = {
                        let mut state = runtime.st.lock().unwrap();
                        state.restore_queries.clear();
                        (
                            state.restore_targets.clone(),
                            state.restore_plan_fingerprints.clone(),
                        )
                    };
                    let denied = EffectAuthority::denied();
                    assert!(call(&denied).await.is_err(), "R1 {port}");
                    assert!(runtime.st.lock().unwrap().restore_queries.is_empty(), "R1");
                    let gate = Arc::new(tokio::sync::Semaphore::new(0));
                    runtime.st.lock().unwrap().restore_query_gate = Some((boundary, gate.clone()));
                    let mut pending = call(&authority);
                    std::future::poll_fn(|cx| {
                        assert!(
                            std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                            "{port}/{boundary}/{change}/{expiry}"
                        );
                        std::task::Poll::Ready(())
                    })
                    .await;
                    {
                        let mut current = authority.current.lock().unwrap();
                        match change {
                            "revoke" => *current = None,
                            "replace" => current.as_mut().unwrap().0.reassign(),
                            "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                            "renew" => current.as_mut().unwrap().0.renew(),
                            _ => {}
                        }
                    }
                    let successful = matches!(change, "unchanged" | "renew");
                    if change == "cancel" {
                        drop(pending);
                    } else {
                        gate.add_permits(1);
                        let result = pending.await;
                        assert_eq!(
                            result.is_ok(),
                            successful,
                            "R2/R3 {port}/{boundary}/{change}/{expiry}"
                        );
                        if let Ok(observed) = result {
                            assert_eq!(observed, handle, "R2 exact handle");
                        }
                    }
                    {
                        let mut state = runtime.st.lock().unwrap();
                        let expected = &QUERIES[..if successful { 4 } else { cut + 1 }];
                        assert_eq!(
                            state.restore_queries,
                            expected
                                .iter()
                                .map(|query| (*query, id.clone()))
                                .collect::<Vec<_>>(),
                            "R3/R4 exact prefix {port}/{boundary}/{change}"
                        );
                        state.restore_query_gate = None;
                        state.restore_queries.clear();
                    }
                    *authority.current.lock().unwrap() =
                        Some((fence.clone(), Duration::from_secs(1)));
                    assert_eq!(call(&authority).await.unwrap(), handle, "R4 exact retry");
                    let state = runtime.st.lock().unwrap();
                    assert_eq!(
                        state.restore_queries,
                        QUERIES
                            .iter()
                            .map(|query| (*query, id.clone()))
                            .collect::<Vec<_>>(),
                        "R4"
                    );
                    assert_eq!(state.restore_targets, original.0, "R5 evidence retained");
                    assert_eq!(
                        state.restore_plan_fingerprints, original.1,
                        "R5 plan retained"
                    );
                    assert_eq!(state.alive.get(&id), Some(&true), "R5 target retained");
                    assert_eq!(state.restore_creates, 1, "R5 no recreation");
                    assert_eq!(state.created_command.len(), 1, "R5 no ordinary create");
                    assert_eq!(state.alive.len(), 1, "R5 no replacement target");
                    assert_eq!(state.observations, 0, "R5 no ordinary adoption fallback");
                    assert_eq!(state.restore_disposals, 0, "R5 no restore cleanup");
                    assert!(state.removals.is_empty(), "R5 no ordinary cleanup");
                    assert!(state.spawned.is_empty(), "R5 no process start");
                    assert!(
                        state.control_binding_calls.is_empty(),
                        "R5 no control activation"
                    );
                    assert_eq!(state.lease_touches, 0, "R5 no lease renewal");
                    assert_eq!(state.control_channel_opens, 0, "R5 no control channel");
                }
            }
        }
    }
}

#[tokio::test]
async fn terminal_restored_container_disposal_rechecks_current_authority_across_response_loss() {
    use pc::test_support::EffectAuthority;
    use std::time::Duration;
    // C1 exact target bound/absent; C2 Pending observation/committed-delete
    // response; C3 revoke/replace/expire/renew/cancel. Constraint: an absent
    // target has no delete response. C4 concrete versus object-erased view of the same provider port.
    // R1 denied entry => zero backend admission. R2 stale authority after
    // observation => no deletion; after committed deletion => no success, no
    // invented rollback. R3 renewal completes; R4 cancellation keeps the same
    // request identity for exact retry. R5 absent replay never recreates or
    // repeats a deletion, and still denies without current authority.
    // Full 3x5x2 actual Pending schedules, reusing the original fake runtime,
    // physical target map and checkpoint request, not another recovery engine.
    for port in ["concrete", "erased"] {
        for (present, boundary) in [
            (true, "observation"),
            (true, "delete"),
            (false, "observation"),
        ] {
            for change in ["revoke", "replace", "expire", "renew", "cancel"] {
                let runtime = Arc::new(FakeRuntime::default());
                let provider = provider(runtime.clone());
                let spec = spec("restored-current");
                let request = restore_request(&spec);
                if present {
                    drop(
                        provider
                            .acquire_restore_environment(&spec, &request)
                            .await
                            .unwrap(),
                    );
                }
                let initial_creates = runtime.st.lock().unwrap().restore_creates;
                let authorization = disposal_authorization(
                    &pc::SandboxEffectFence::new(
                        "restored-disposal",
                        "owner",
                        "boot",
                        1,
                        u64::MAX - 1,
                    )
                    .unwrap(),
                );
                let authority = EffectAuthority::new(authorization.effect_fence());
                let call = |authority| -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<(), pc::SandboxError>> + Send + '_>,
                > {
                    if port == "concrete" {
                        pc::RestoreTargetProvider::<Arc<dyn SessionSandbox>>::dispose_restored(
                            &provider,
                            &spec,
                            &request,
                            &authorization,
                            authority,
                        )
                    } else {
                        let target_owner: &dyn pc::RestoreTargetProvider<Arc<dyn SessionSandbox>> =
                            &provider;
                        target_owner.dispose_restored(&spec, &request, &authorization, authority)
                    }
                };
                let denied = EffectAuthority::denied();
                assert!(call(&denied).await.is_err(), "R1");
                assert!(
                    runtime
                        .st
                        .lock()
                        .unwrap()
                        .restore_disposal_authorizations
                        .is_empty()
                );
                let gate = Arc::new(tokio::sync::Semaphore::new(0));
                {
                    let mut state = runtime.st.lock().unwrap();
                    if boundary == "observation" {
                        state.restore_disposal_observation_gate = Some(gate.clone());
                    } else {
                        state.restore_disposal_response_gate = Some(gate.clone());
                    }
                }
                let mut pending = call(&authority);
                std::future::poll_fn(|cx| {
                    assert!(
                        std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                        "{port}/{present}/{boundary}/{change}"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                {
                    let mut current = authority.current.lock().unwrap();
                    match change {
                        "revoke" => *current = None,
                        "replace" => current.as_mut().unwrap().0.reassign(),
                        "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                        "renew" => current.as_mut().unwrap().0.renew(),
                        _ => {}
                    }
                }
                if change == "cancel" {
                    drop(pending);
                } else {
                    gate.add_permits(1);
                    assert_eq!(pending.await.is_ok(), change == "renew", "R2/R3");
                }
                {
                    let mut state = runtime.st.lock().unwrap();
                    assert_eq!(
                        state.restore_disposals,
                        usize::from(present && (boundary == "delete" || change == "renew"))
                    );
                    assert_eq!(
                        state.restore_targets.is_empty(),
                        !present || boundary == "delete" || change == "renew"
                    );
                    state.restore_disposal_observation_gate = None;
                    state.restore_disposal_response_gate = None;
                }
                // A fresh fixture grant is an explicit successor test input,
                // never a production reopening of the revoked Worker proof.
                let retry = EffectAuthority::new(authorization.effect_fence());
                call(&retry).await.unwrap();
                call(&retry).await.unwrap();
                assert!(call(&denied).await.is_err(), "R5");
                let state = runtime.st.lock().unwrap();
                assert_eq!(state.restore_creates, initial_creates, "R4 no recreation");
                assert_eq!(
                    state.restore_disposals,
                    usize::from(present),
                    "R4/R5 exact-once physical removal"
                );
                assert!(state.restore_targets.is_empty());
            }
        }
    }
}

struct RejectingSecretBroker;

#[async_trait]
impl pc::SecretBroker for RejectingSecretBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::fence_rejected("claim expired"))
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::fence_rejected("claim expired"))
    }

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported("not supported"))
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
    sandbox.dispose_unattached().await.unwrap();
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
    let sandbox_spec = spec("run-x");
    let sandbox_a = node_a.create(&sandbox_spec).await.unwrap();
    let wire = serde_json::to_string(&sandbox_a.handle()).unwrap();
    drop(sandbox_a);
    drop(node_a);

    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    assert_eq!(recovered.provider_kind(), "container");
    let node_b = provider(rt.clone());
    let sandbox_b = SandboxProvider::adopt_for_effect(
        &node_b,
        SandboxAdoption::new(&sandbox_spec, &recovered),
        awaken_provisioning_contract::SandboxEffect::Unattached,
    )
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

    sandbox_b.dispose_unattached().await.unwrap();
    assert!(matches!(
        sandbox_b.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn handle_round_trips_and_adopt_reconnects() {
    /* Adoption-port cause/effect table — AP1. C1 current V2 handle; C2 frozen
     * SandboxSpec present/absent. C1+!C2 => E1 handle-only compatibility port
     * rejects before treating terminal participants as complete. C1+C2 => E2
     * canonical provider adoption reconnects and retains live behavior. Legacy
     * V1+!C2 remains the separate compatibility row covered below. */
    let rt = Arc::new(FakeRuntime::default());
    let p = provider(rt.clone());
    let sandbox_spec = spec("run-2");
    let sandbox = p.create(&sandbox_spec).await.unwrap();
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind(), "container");

    let wire = serde_json::to_string(&handle).unwrap();
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    assert!(
        p.adopt(&recovered).await.is_err(),
        "current handle-only adoption cannot invent terminal participant policy"
    );
    let adopted = SandboxProvider::adopt_for_effect(
        &p,
        SandboxAdoption::new(&sandbox_spec, &recovered),
        awaken_provisioning_contract::SandboxEffect::Unattached,
    )
    .await
    .unwrap();
    assert_eq!(adopted.id(), "run-2");
    let proc = adopted.process("main").await.unwrap();
    assert_eq!(proc.id(), "main");
    // late attach fails closed on this tier
    assert!(adopted.attach(spec("x").mounts.remove(0)).await.is_err());
}

#[tokio::test]
async fn generic_container_adoption_rejects_restoration_without_a_frozen_spec() {
    // Phase-B spec-less adoption rule R13: C1 a Phase-A reader accepts a complete
    // future Some value; C2 it is attached to an ordinary live target; C3 generic
    // The legacy concrete adoption edge supplies no frozen SandboxSpec. Phase-A wire-reader
    // compatibility does not activate a Phase-B restore. C1+C2+C3 => E1 reject
    // before every runtime effect and E2 preserve the existing physical target.
    let runtime = Arc::new(FakeRuntime::default());
    let node_a = provider(runtime.clone());
    let created = node_a.create(&spec("reader-adopt")).await.unwrap();
    let ordinary = created.handle();
    let container_id = ordinary.container_payload().unwrap().container_id.clone();
    let future = add_future_restoration(&ordinary);
    drop(created);
    drop(node_a);
    let node_b = provider(runtime.clone());
    assert!(node_b.adopt(&future).await.is_err(), "R13/E1");
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.inspections, 0, "R13/E1 pre-inspect");
    assert!(state.spawned.is_empty(), "R13/E1");
    assert!(state.removals.is_empty(), "R13/E1");
    assert!(state.control_binding_calls.is_empty(), "R13/E1");
    assert_eq!(state.lease_touches, 0, "R13/E1");
    assert_eq!(state.restore_creates, 0, "R13/E1");
    assert_eq!(state.restore_disposals, 0, "R13/E1");
    assert!(state.restore_targets.is_empty(), "R13/E1");
    assert_eq!(state.alive.get(&container_id), Some(&true), "R13/E2");
}

#[tokio::test]
async fn container_provider_rejects_host_bind_before_runtime_inspection() {
    // Unsupported-provider rule R16: C1 Some+HostBind reaches the ordinary
    // container adopter. C1 => E1 reject before inspect/status/spawn/remove and
    // E2 leave every fake-runtime effect counter unchanged.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = provider(runtime.clone());
    assert!(
        provider.adopt(&future_host_bind_handle()).await.is_err(),
        "R16/E1"
    );
    let state = runtime.st.lock().unwrap();
    assert_eq!(state.inspections, 0, "R16/E2");
    assert!(state.alive.is_empty(), "R16/E2");
    assert!(state.created_command.is_empty(), "R16/E2");
    assert!(state.control_binding_calls.is_empty(), "R16/E2");
    assert!(state.spawned.is_empty(), "R16/E2");
    assert!(state.removals.is_empty(), "R16/E2");
    assert_eq!(state.lease_touches, 0, "R16/E2");
}

#[tokio::test]
async fn default_runtime_rejects_future_handle_before_remove() {
    // Adapter rule R17 covers Docker and Podman, which inherit the sole default
    // remove_with_handle path: C1 any provider-owned continuation value reaches
    // an unaware runtime. C1 => E1 reject and E2 never call remove/delete.
    let runtime = FakeRuntime::default();
    let handle = future_host_bind_handle();
    let payload = handle.container_payload().unwrap();
    let runtime_handle = payload.runtime_handle.as_ref().unwrap();
    assert!(
        runtime
            .remove_with_handle("future-container", Some(runtime_handle))
            .await
            .is_err(),
        "R17/E1"
    );
    assert!(runtime.st.lock().unwrap().removals.is_empty(), "R17/E2");
}

#[tokio::test]
async fn legacy_container_adoption_reemits_v1_without_invented_evidence() {
    // Compatibility rule: a live V1 locator may still be adopted when no
    // Repository proof is required, but crossing the provider boundary cannot
    // synthesize a V2 realization fingerprint or owned-path WAL.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = provider(runtime);
    let current = provider.create(&spec("legacy-container-v1")).await.unwrap();
    let current_handle = current.handle();
    let legacy = pc::SandboxHandle::container(
        current_handle.sandbox_id.clone(),
        current_handle.container_payload().unwrap().clone(),
    );
    let adopted = provider.adopt(&legacy).await.unwrap();
    let emitted = adopted.handle();
    assert_eq!(emitted, legacy);
    assert!(emitted.realization_fingerprint().is_none());
    assert!(emitted.owned_paths().is_none());
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
    let provider = provider(runtime.clone());
    let sandbox_spec = spec("live-inputs");
    let sandbox = provider.create_container(&sandbox_spec).await.unwrap();
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

    let handle = pc::Sandbox::handle(&sandbox);
    let adopted = SandboxProvider::adopt_for_effect(
        &provider,
        SandboxAdoption::new(&sandbox_spec, &handle),
        awaken_provisioning_contract::SandboxEffect::Unattached,
    )
    .await
    .unwrap();
    assert!(adopted.supports_live_mount_replacement(&[], std::slice::from_ref(&input)));
    adopted
        .attach(input.clone())
        .await
        .expect("recovery retains the resident Pod's projector capability");

    let mut outside = input.clone();
    outside.mount_path = "/workspace/report.html".into();
    assert!(!sandbox.supports_live_mount_replacement(&[], std::slice::from_ref(&outside)));
    assert!(pc::Sandbox::attach(&sandbox, outside).await.is_err());

    let mut escaped = input;
    escaped.mount_path = "/mnt/session/uploads/../secret".into();
    assert!(!adopted.supports_live_mount_replacement(&[], std::slice::from_ref(&escaped)));
    assert!(adopted.attach(escaped).await.is_err());
}

#[tokio::test]
async fn adopt_rejects_a_handle_whose_runtime_is_gone() {
    /* Recovery decision rule A1: a well-formed durable handle plus a live
     * runtime target is adoptable; A2: the same handle after physical teardown
     * is an orphan and must fail adoption so the existing reconciler can
     * re-place it. Merely completing locator decoding is not successful adopt. */
    let rt = Arc::new(FakeRuntime::default());
    let p = provider(rt);
    let sandbox_spec = spec("run-gone");
    let sandbox = p.create(&sandbox_spec).await.unwrap();
    let handle = sandbox.handle();
    sandbox.dispose_unattached().await.unwrap();
    assert!(
        SandboxProvider::adopt_for_effect(
            &p,
            SandboxAdoption::new(&sandbox_spec, &handle),
            awaken_provisioning_contract::SandboxEffect::Unattached
        )
        .await
        .is_err(),
        "A2"
    );
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
    environment.dispose_unattached().await.unwrap();

    // Backend create failure propagates.
    let rt = Arc::new(FakeRuntime {
        st: Mutex::new(FakeState {
            fail_create: true,
            ..Default::default()
        }),
    });
    assert!(provider(rt).create(&spec("run-5")).await.is_err());
}

#[tokio::test]
async fn successful_host_staging_follows_the_physical_container_lifetime() {
    /* Lifetime cause/effect table. C1 create succeeds with host binds; C2 the
     * process-local Sandbox wrapper is released; C3 explicit terminal disposal
     * succeeds. R1=C1+C2+!C3 => retain exact source bytes while the container is
     * alive; R2=C1+C3 => remove both container and staging, idempotently.
     * Definite/ambiguous create failures are covered by the adjacent outcome
     * table. Wrapper release is not evidence of physical container removal. */
    use pc::Sandbox;
    for dispose in [false, true] {
        let runtime = Arc::new(FakeRuntime::default());
        let provider = provider(runtime.clone());
        let sandbox = provider
            .create_container(&spec(if dispose {
                "stage-dispose"
            } else {
                "stage-drop"
            }))
            .await
            .unwrap();
        let container_id = sandbox.container_id.clone();
        let source = std::path::PathBuf::from(
            &runtime.st.lock().unwrap().created_binds[&container_id][0].source_ref,
        );
        let original = std::fs::read(&source).unwrap();
        if dispose {
            sandbox.dispose_unattached().await.unwrap();
            sandbox.dispose_unattached().await.unwrap();
        }
        drop(sandbox);
        drop(provider);
        assert_eq!(runtime.st.lock().unwrap().alive[&container_id], !dispose);
        if dispose {
            assert!(!source.exists(), "R2 terminal cleanup removes host staging");
            assert_eq!(runtime.st.lock().unwrap().removals.len(), 1);
        } else {
            // Clean this test's fake object even when exercising the old bug.
            let retained_bytes = std::fs::read(&source);
            runtime.remove(&container_id).await.unwrap();
            if let Some(root) = source.parent().filter(|root| root.exists()) {
                std::fs::remove_dir_all(root).unwrap();
            }
            assert_eq!(
                retained_bytes.unwrap(),
                original,
                "R1 retain live bind bytes"
            );
        }
    }
}

#[test]
fn runtime_error_messages_render() {
    assert!(RuntimeError::NotFound("c".into()).to_string().contains('c'));
    assert!(RuntimeError::Backend("x".into()).to_string().contains('x'));
    assert!(
        RuntimeError::MayHaveCommitted("x".into())
            .to_string()
            .contains("may have committed")
    );
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
        let base = ContainerProvider::for_test(rt.clone(), "ghcr.io/awaken/sandbox:latest")
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
    let p = ContainerProvider::for_test(rt.clone(), "ghcr.io/awaken/sandbox:latest")
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
async fn canonical_environment_spawns_agent_without_process_owning_terminal_cleanup() {
    /* Canonical Container launch/terminal-ownership table CO1. Causes: C1 the
     * Environment provider realizes one Session environment; C2 its agent exec
     * is running/terminal; C3 a writable Secret participant is present; C4 the
     * aggregate has/has not supplied an exact terminal preparation and disposal
     * authorization. Effects: E1 C1 returns one durable Environment plus an
     * independent channel/process; E2 C2 without C4 reports process status but
     * performs zero Secret read/write and zero Environment removal; E3 only C4
     * writes the Secret and removes the exact Environment. Rules: L1 C1=>E1;
     * L2 C1+C2+C3+!C4=>E2; L3 C1+C3+C4=>E3. NM2 N1-N6 owns the exhaustive
     * fenced failure/replay table; this case locks the formerly missing negative
     * process-terminal row while preserving basic launch and handle coverage. */
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#;
    let rt = Arc::new(
        FakeRuntime::default()
            .refreshing_credential(refreshed)
            .with_live_credential(refreshed),
    );
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"tokens":{"access_token":"old"}}"#.to_vec();
    let provider = provider_without_broker(rt.clone()).with_secret_broker(broker.clone());
    let mut requested = spec("run-oa");
    requested.mounts.push(writable_credential_mount());
    let environment = SandboxProvider::claim_for_effect(
        &provider,
        &requested,
        awaken_provisioning_contract::SandboxEffect::Unattached,
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("L1 canonical Environment creation");
    let handle = environment.handle();
    let AgentProcess { process, channel } = environment
        .spawn_agent_process(pc::Command {
            argv: command_of(&requested),
            cwd: String::new(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("L1 canonical agent exec");

    assert_eq!(process.id(), "exec-0", "L1 attempt process is not PID 1");
    process.signal(pc::Signal::Term).await.unwrap();
    assert_eq!(
        process.wait().await.unwrap(),
        pc::ExitStatus {
            code: Some(0),
            signaled: false,
        },
        "L2 process wait is process-only"
    );
    assert_eq!(
        process.poll().await.unwrap(),
        Some(pc::ExitStatus {
            code: Some(0),
            signaled: false,
        }),
        "L2 terminal replay remains process-only"
    );
    assert_eq!(
        rt.st.lock().unwrap().alive.get("cid-run-oa"),
        Some(&true),
        "L2 process terminal cannot remove the Session Environment"
    );
    assert_eq!(rt.st.lock().unwrap().live_credential_reads, 0, "L2");
    assert!(broker.writes.lock().unwrap().is_empty(), "L2");

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

    // The durable handle carries the container id for reattach.
    assert_eq!(handle.provider_kind(), "container");
    let payload = handle.container_payload().unwrap();
    assert_eq!(payload.container_id, "cid-run-oa");
    // Durable-handle decision rule H1: C1=a realized container has mounted
    // inputs; C2=the provider is later adopted from only its typed handle.
    // C1 => E1 every checkpoint exclusion is frozen without an opaque `extra`
    // map; C1+C2 => E2 the durable wire round-trip preserves the exact paths.
    // This prevents a replacement Worker from archiving independently owned
    // mounts after the old untyped extension was removed.
    assert_eq!(
        payload.continuation_excluded_paths,
        ["/data/in.txt", "/work", "/acp-config/.credentials.json"]
    );
    let durable = serde_json::to_vec(&handle).unwrap();
    let adopted: pc::SandboxHandle = serde_json::from_slice(&durable).unwrap();
    assert_eq!(
        adopted
            .container_payload()
            .unwrap()
            .continuation_excluded_paths,
        ["/data/in.txt", "/work", "/acp-config/.credentials.json"]
    );
    // A live duplex channel was opened (the ACP bridge would drive it).
    let _channel = channel;
    // The physical container is an environment keepalive, not the attempt agent.
    assert_eq!(
        rt.st.lock().unwrap().created_command.get("cid-run-oa"),
        Some(&environment_keepalive_command())
    );
    assert_eq!(
        rt.st.lock().unwrap().spawned[0].1,
        ["claude", "--acp"].map(str::to_string)
    );

    let terminal = pc::SandboxEffectFence::new(
        "canonical-environment-terminal",
        "worker-owner",
        "worker-incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    assert_eq!(
        environment
            .prepare_disposal_for_effect(
                &terminal,
                &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal)
            )
            .await
            .expect("L3 aggregate-owned terminal preparation"),
        terminal,
        "L3 Container reports its exact provider-prepared predecessor"
    );
    environment
        .dispose_for_effect(
            &disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("L3 aggregate-authorized physical cleanup");
    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[refreshed.to_vec()]
    );
    assert_eq!(
        rt.st.lock().unwrap().alive.get("cid-run-oa"),
        Some(&false),
        "L3 explicit terminal owner removes the Environment"
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
async fn durable_writable_secret_waits_for_explicit_environment_disposal() {
    // Credential terminal-ownership table CW1. Causes: C1 one Environment has
    // a durable writable Secret; C2 its child process exits; C3 the aggregate
    // supplies exact preparation plus disposal authorization. Effects: E1 C2
    // leaves the Environment and Secret authority untouched; E2 C3 writes the
    // refreshed material exactly once and removes the Environment; E3 polling
    // the already-terminal child cannot repeat writeback. Rules: W1 C1+C2=>E1;
    // W2 C1+C2+C3=>E2; W3 W2+poll=>E3.
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#;
    let rt = Arc::new(FakeRuntime::default().refreshing_credential(refreshed));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"tokens":{"access_token":"old"}}"#.to_vec();
    let provider =
        ContainerProvider::for_test(rt.clone(), "agent:latest").with_secret_broker(broker.clone());
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
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

    let environment = SandboxProvider::claim_for_effect(
        &provider,
        &spec,
        awaken_provisioning_contract::SandboxEffect::Unattached,
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("W1 canonical Environment creation");
    let AgentProcess {
        process,
        channel: _channel,
    } = environment
        .spawn_agent_process(pc::Command {
            argv: command_of(&spec),
            cwd: String::new(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("W1 canonical child process");
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
    process.wait().await.unwrap();
    assert!(broker.writes.lock().unwrap().is_empty(), "W1/E1");
    assert_eq!(
        rt.st.lock().unwrap().alive.get("cid-credential-refresh"),
        Some(&true),
        "W1/E1 process exit cannot remove the Environment"
    );
    let terminal = pc::SandboxEffectFence::new(
        "credential-refresh-terminal",
        "worker-owner",
        "worker-incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    environment
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("W2 aggregate-owned terminal preparation");
    environment
        .dispose_for_effect(
            &disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("W2 aggregate-authorized physical cleanup");
    assert_eq!(
        broker.writes.lock().unwrap().as_slice(),
        &[refreshed.to_vec()]
    );
    // Idempotent poll/wait cannot reseal the same refresh twice.
    process.poll().await.unwrap();
    assert_eq!(broker.writes.lock().unwrap().len(), 1, "W3/E3");
}

#[tokio::test]
async fn remote_runtime_harvests_live_credential_only_at_environment_disposal() {
    // Signalled-process table CW2. C1 a live credential changes remotely; C2
    // the child receives Term and reaches terminal; C3 the aggregate later
    // authorizes Environment disposal. E1 C2 performs no credential read/write
    // and keeps the Environment live; E2 C3 harvests the live value exactly
    // once. Rules: S1 C1+C2=>E1; S2 C1+C2+C3=>E2.
    let refreshed = br#"{"claudeAiOauth":{"accessToken":"new","refreshToken":"rotated"}}"#;
    let rt = Arc::new(FakeRuntime::default().with_live_credential(refreshed));
    let broker = Arc::new(RecordingSecretBroker::default());
    *broker.current.lock().unwrap() = br#"{"claudeAiOauth":{"accessToken":"old"}}"#.to_vec();
    let provider =
        ContainerProvider::for_test(rt.clone(), "agent:latest").with_secret_broker(broker.clone());
    let requested = writable_credential_spec("remote-credential-refresh");
    let environment = SandboxProvider::claim_for_effect(
        &provider,
        &requested,
        awaken_provisioning_contract::SandboxEffect::Unattached,
        awaken_provisioning_contract::SandboxRealizationIntent::Create,
    )
    .await
    .expect("S1 canonical Environment creation");
    let AgentProcess {
        process,
        channel: _channel,
    } = environment
        .spawn_agent_process(pc::Command {
            argv: command_of(&requested),
            cwd: String::new(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("S1 canonical child process");
    process.signal(pc::Signal::Term).await.unwrap();
    process.wait().await.unwrap();
    assert!(broker.writes.lock().unwrap().is_empty(), "S1/E1");
    assert_eq!(
        rt.st
            .lock()
            .unwrap()
            .alive
            .get("cid-remote-credential-refresh"),
        Some(&true),
        "S1/E1 process terminal cannot remove the Environment"
    );
    let terminal = pc::SandboxEffectFence::new(
        "remote-credential-refresh-terminal",
        "worker-owner",
        "worker-incarnation",
        1,
        u64::MAX,
    )
    .unwrap();
    environment
        .prepare_disposal_for_effect(
            &terminal,
            &awaken_provisioning_contract::test_support::EffectAuthority::new(&terminal),
        )
        .await
        .expect("S2 aggregate-owned terminal preparation");
    environment
        .dispose_for_effect(
            &disposal_authorization(&terminal),
            &awaken_provisioning_contract::test_support::EffectAuthority::new(
                disposal_authorization(&terminal).effect_fence(),
            ),
        )
        .await
        .expect("S2 aggregate-authorized physical cleanup");
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
    let provider = ContainerProvider::for_test(runtime.clone(), "agent:latest")
        .with_secret_broker(broker.clone());
    let sandbox = provider
        .create(&writable_credential_spec(
            "failed-remote-credential-harvest",
        ))
        .await
        .unwrap();
    assert!(
        sandbox.dispose_unattached().await.is_err(),
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
    let provider = ContainerProvider::for_test(runtime.clone(), "agent:latest")
        .with_secret_broker(broker.clone());
    let sandbox = provider
        .create(&writable_credential_spec("rejected-credential-writeback"))
        .await
        .unwrap();
    assert!(
        sandbox.dispose_unattached().await.is_err(),
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
        process_tree_supervision: false,
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

    let guard = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false, true)
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
async fn native_mount_projection_does_not_duplicate_plaintext_in_host_staging() {
    /* Native-materialization cause/effect table. Causes: C1 the runtime binds
     * host files / projects bytes through its native API; C2 the resolved mount
     * is ordinary content / a writable Secret. Effects: E1 host-bind creates
     * one guarded file; E2 native projection carries the same bytes only in the
     * plan and creates no host directory; E3 writable native Secret retains its
     * logical directory/file metadata for live writeback without a disk fallback.
     * Rules: N1 host(C1)+either(C2)=>E1; N2 native(C1)+content(C2)=>E2;
     * N3 native(C1)+writable-Secret(C2)=>E2+E3. The host rows are covered by
     * `resolve_and_stage_realizes_a_file_from_the_seed` and the writable test. */
    let mut spec = file_mount_spec(
        "native-secret",
        pc::MountSource::Secret {
            reference: "credential-1".into(),
            content_hash: None,
        },
        true,
    );
    spec.mounts[0].mount_path = "/acp-config/auth.json".into();
    spec.mounts[0].access = pc::MountAccess::ReadWrite;
    spec.mounts[0].lifetime = pc::MountLifetime::Durable;
    let mut plan = container_plan(&spec, "img", &["x".to_string()], None).unwrap();
    let seed = HashMap::from([("credential-1".into(), b"secret-bytes".to_vec())]);

    let staged = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false, false)
        .await
        .expect("native projection");

    assert!(staged.guard.is_none(), "N3 no host plaintext stage");
    assert_eq!(staged.secret_writebacks.len(), 1, "N3 writeback metadata");
    assert!(
        staged.secret_writebacks[0].staged_path.is_none(),
        "N3 live native bytes are the only writeback source"
    );
    assert_eq!(plan.binds[0].source_ref, "credential-1", "N3 no host path");
    assert_eq!(
        plan.binds[0].mount_path, "/acp-config",
        "N3 directory projection"
    );
    assert_eq!(
        plan.binds[0].credential_file_path.as_deref(),
        Some("/acp-config/auth.json"),
        "N3 exact harvest path"
    );
    // Feature decision table: unit tests and K8s native Secret projection consume
    // raw SecretBytes; standalone Docker/Podman use host staging. This assertion
    // owns exact bytes; strict Clippy with and without `k8s` owns the two build
    // rows, so feature unification cannot hide a dormant byte accessor.
    assert_eq!(
        plan.binds[0]
            .secret_content
            .as_ref()
            .expect("native Secret bytes")
            .expose(),
        b"secret-bytes",
        "N3 one in-plan projection"
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

    let _guard = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false, true)
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
        true,
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
    let e = resolve_and_stage(
        &spec,
        &mut plan.binds,
        &HashMap::new(),
        &None,
        &None,
        false,
        true,
    )
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
    let e = resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false, true)
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
    resolve_and_stage(&spec, &mut plan.binds, &seed, &None, &None, false, true)
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

    let staged = resolve_and_stage(
        &spec,
        &mut plan.binds,
        &HashMap::new(),
        &None,
        &None,
        false,
        true,
    )
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

    let error = resolve_and_stage(
        &spec,
        &mut plan.binds,
        &HashMap::new(),
        &None,
        &None,
        false,
        true,
    )
    .await
    .expect_err("corrupt binary content must fail closed");

    assert!(error.to_string().contains("hash mismatch"), "{error}");
}

/// Whether the environment asked its container to forget the execs it started.
fn reaped(runtime: &FakeRuntime) -> bool {
    runtime.st.lock().unwrap().spawned.iter().any(|(_, argv)| {
        argv.iter()
            .any(|arg| arg.contains(crate::exec_attempts::EXEC_PID_DIR))
    })
}

#[tokio::test]
async fn disposal_reaps_every_exec_edge_and_then_refuses_new_ones() {
    /* Exec responsibility table. Causes: C1 the environment started a process
     * through `Sandbox::spawn`; C2 through `spawn_agent`; C3 it never exec'd
     * at all; C4 disposal has begun; C5 the container is already stopped or
     * absent; C6 the container is still running and refuses to reap. Effects:
     * E1 disposal makes the container forget every attempt (the reaper runs);
     * E2 an environment that never exec'd is never asked to reap, so a runtime
     * that cannot exec keeps its fail-closed inherit unreachable; E3 once
     * disposal has begun every Session spawn edge refuses, because the reaper
     * enumerates once and a process started after that sweep would still be
     * writing when Secret write-back and the Memory harvest read the
     * workspace; E4 the environment is removed regardless, because a lost
     * harvest costs one round of bytes and a leaked container costs the host
     * permanently; E5 the skipped harvest is reported rather than silent.
     * Rules: R1 C1 => E1; R2 C2 => E1; R3 C3 => E2; R4 (C1|C2)+C4 => E3;
     * R5 C1+C5(stopped) => E4,E5; R6 C1+C6 => E4,E5; R7 C1+C5(absent) => E4,E5.
     *
     * C5 and C6 converge on purpose: an in-container read needs an exec, and
     * neither state can run one. They would diverge again the day a harvest
     * can read a stopped container without exec-ing it — a stopped container
     * IS quiet, which is the stronger guarantee, not the weaker one.
     *
     * R1 is the edge that had no record at all: `Sandbox::spawn` never
     * counted its attempt, so `reap_exec_orphans` read "nothing was started"
     * and skipped the sweep outright — which also meant R5/R6/R7 could not
     * arise, because the reaper never ran for that edge. */
    let mountless = || {
        let mut spec = spec("exec-record");
        spec.mounts.clear();
        spec.env.clear();
        spec
    };

    for (rule, agent_edge) in [("R1", false), ("R2", true)] {
        let runtime = Arc::new(FakeRuntime::default());
        let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
        let sandbox = provider.create_container(&mountless()).await.unwrap();
        if agent_edge {
            sandbox
                .spawn_agent(pc::Command::new(["sh", "-c", "true"]))
                .await
                .expect("agent exec starts");
        } else {
            pc::Sandbox::spawn(&sandbox, pc::Command::new(["sh", "-c", "true"]))
                .await
                .expect("process exec starts");
        }
        assert!(
            !reaped(runtime.as_ref()),
            "{rule} nothing reaped before disposal"
        );

        // R4: the gate closes at disposal, so prove it on this same live
        // environment before disposing it. This is the very call disposal
        // makes; that disposal makes it is asserted separately below.
        sandbox.seal_exec_attempts();
        assert!(
            matches!(
                pc::Sandbox::spawn(&sandbox, pc::Command::new(["sh", "-c", "true"])).await,
                Err(pc::SandboxError::Gone(_))
            ),
            "{rule}/R4 a sealed environment refuses the process edge",
        );
        assert!(
            matches!(
                sandbox
                    .spawn_agent(pc::Command::new(["sh", "-c", "true"]))
                    .await,
                Err(pc::SandboxError::Gone(_))
            ),
            "{rule}/R4 a sealed environment refuses the agent edge",
        );

        sandbox.dispose().await.expect("dispose");
        assert!(
            reaped(runtime.as_ref()),
            "{rule}/E1 disposal reaped the attempt"
        );
    }

    // R5: the container is already gone, so the obligation reaping carries is
    // already met and disposal must not be stranded over it. Fail-closed: only
    // an observation that the container is not running admits the tolerance.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
    let sandbox = provider.create_container(&mountless()).await.unwrap();
    pc::Sandbox::spawn(&sandbox, pc::Command::new(["sh", "-c", "true"]))
        .await
        .expect("R5 process exec starts");
    {
        let mut state = runtime.st.lock().unwrap();
        state.alive.insert("cid-exec-record".into(), false);
        state.fail_spawn = true;
    }
    assert!(
        sandbox.dispose().await.is_err(),
        "R5 a stopped container reports its skipped harvest",
    );
    assert!(
        runtime
            .st
            .lock()
            .unwrap()
            .removals
            .iter()
            .any(|(id, _)| id == "cid-exec-record"),
        "R5/E4 and is removed rather than stranded",
    );

    // R7: the container is not merely stopped but absent. Reaping cannot be
    // asked of what is gone, and its obligation is met a fortiori.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
    let sandbox = provider.create_container(&mountless()).await.unwrap();
    pc::Sandbox::spawn(&sandbox, pc::Command::new(["sh", "-c", "true"]))
        .await
        .expect("R7 process exec starts");
    runtime.st.lock().unwrap().alive.remove("cid-exec-record");
    let absent = sandbox.dispose().await;
    assert!(
        absent.is_err(),
        "R7 an absent container also reports the skipped harvest",
    );

    // R6: the container refuses to reap, so nothing can promise the workspace
    // is quiet. The in-container harvest is skipped and the caller is told —
    // but the environment is removed anyway, because a lost harvest costs one
    // round of bytes and a leaked container costs the host permanently.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
    let sandbox = provider.create_container(&mountless()).await.unwrap();
    pc::Sandbox::spawn(&sandbox, pc::Command::new(["sh", "-c", "true"]))
        .await
        .expect("R6 process exec starts");
    runtime.st.lock().unwrap().fail_spawn = true;
    let refused = sandbox.dispose().await;
    assert!(
        refused.is_err(),
        "R6/E5 a skipped harvest is reported, never silent",
    );
    assert!(
        format!("{}", refused.unwrap_err()).contains("skipped its in-container harvest"),
        "R6/E5 the report names what was lost",
    );
    assert!(
        runtime
            .st
            .lock()
            .unwrap()
            .removals
            .iter()
            .any(|(id, _)| id == "cid-exec-record"),
        "R6/E6 the environment is removed even so",
    );

    // R3: never exec'd, so the reaper is never demanded.
    let runtime = Arc::new(FakeRuntime::default());
    let provider = ContainerProvider::for_test(runtime.clone(), "fixture");
    let sandbox = provider.create_container(&mountless()).await.unwrap();
    sandbox.dispose().await.expect("dispose");
    assert!(!reaped(runtime.as_ref()), "R3/E2 no reap was demanded");
    // Disposal seals whether or not it had anything to reap: an environment
    // that never exec'd must not acquire its first process during cleanup.
    assert!(
        sandbox.exec_attempts_sealed(),
        "R3/E3 disposal closed the environment to new attempts",
    );
}
