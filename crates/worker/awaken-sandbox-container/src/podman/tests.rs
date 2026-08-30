//! Behavior tests for the Podman runtime adapter.
//!
//! This remains a child of `podman`, retaining access to the private adapter
//! seams and the original cause/effect decision tables.

use std::ffi::OsString;
use std::sync::{Arc, Mutex};

use awaken_provisioning_contract::ProcessHandle;

use crate::{NetworkMode, RootfsPlan};

use super::*;

struct FixedBroker;

#[async_trait]
impl pc::SecretBroker for FixedBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(b"podman-secret".to_vec())
    }

    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.materialize(reference).await
    }

    async fn write_back(&self, _reference: &str, _bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::new("not supported"))
    }
}

async fn materialized(command: pc::Command) -> pc::MaterializedCommand {
    pc::materialize_process_command(&[], command, None)
        .await
        .unwrap()
}

/// A scripted [`CommandExec`]: a handler maps `(bin, args)` to a canned output,
/// and every invocation's argv is recorded so tests can assert what was run.
type CommandHandler = dyn Fn(&[String]) -> CmdOutput + Send + Sync;

struct FakeExec {
    handler: Box<CommandHandler>,
    calls: Mutex<Vec<Vec<String>>>,
}

struct BlockingExec;

#[async_trait]
impl CommandExec for BlockingExec {
    async fn exec(&self, _bin: &str, _args: &[String]) -> std::io::Result<CmdOutput> {
        std::future::pending().await
    }
}

#[async_trait]
impl CommandExec for FakeExec {
    async fn exec(&self, _bin: &str, args: &[String]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push(args.to_vec());
        Ok((self.handler)(args))
    }
}

fn ok(stdout: &str) -> CmdOutput {
    CmdOutput {
        ok: true,
        status_code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn err(stderr: &str) -> CmdOutput {
    CmdOutput {
        ok: false,
        status_code: Some(125),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

/// Build a runtime whose executor replies per `handler`, plus a handle to the
/// recorded argv list.
fn runtime_with(
    port: u16,
    handler: impl Fn(&[String]) -> CmdOutput + Send + Sync + 'static,
) -> (PodmanRuntime, Arc<FakeExec>) {
    let fake = Arc::new(FakeExec {
        handler: Box::new(handler),
        calls: Mutex::new(Vec::new()),
    });
    let namespace = ContainerRealizationNamespace::from_stable_parts(["podman-tests"])
        .expect("constant test namespace");
    (
        PodmanRuntime::with_exec(namespace, port, fake.clone()),
        fake,
    )
}

#[tokio::test]
async fn observation_preserves_absence_backend_and_host_bind_recovery_causes() {
    // Cause/effect table: C1 `ps --all` succeeds with exact Running/Terminal/empty,
    // or the Podman process fails; C2 durable evidence is V2 exact/legacy; C3
    // Podman host-bind participants are reconstructible only by their creating
    // attempt. R1 exact Running from an unproven prior attempt=>Incompatible;
    // R2 exact Terminal=>Terminal(dispose); R3 exact empty=>Unavailable(Some id);
    // R4 backend failure remains indeterminate; R5 legacy empty cannot prove
    // deletion authority and therefore fails closed.
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&pc::SandboxSpec {
        scope: "s1".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        control_services: Default::default(),
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
    });
    let running = format!(
        r#"[{{"Id":"container-1","State":"running","Labels":{{"{MANAGED_SANDBOX_LABEL}":"1","{SANDBOX_ADOPTION_LABEL}":"{fingerprint}","{SANDBOX_REALIZATION_LABEL}":"{fingerprint}"}}}}]"#
    );
    let running_response = running.clone();
    let (runtime, _) = runtime_with(8080, move |_| ok(&running_response));
    assert!(
        matches!(
            ContainerRuntime::observe(
                &runtime,
                crate::ContainerObservationExpectation {
                    container_id: "container-1",
                    adoption_fingerprint: Some(&fingerprint),
                    realization_fingerprint: Some(&fingerprint),
                    runtime_handle: None,
                    effect_fence: None,
                },
            )
            .await
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "R1"
    );
    let terminal = running.replace("\"State\":\"running\"", "\"State\":\"exited\"");
    let (terminal_runtime, _) = runtime_with(8080, move |_| ok(&terminal));
    assert_eq!(
        ContainerRuntime::observe(
            &terminal_runtime,
            crate::ContainerObservationExpectation {
                container_id: "container-1",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: None,
                effect_fence: None,
            },
        )
        .await
        .unwrap(),
        pc::SandboxObservation::Terminal {
            physical_incarnation: "container-1".into(),
        },
        "R2"
    );
    let wrong_fingerprint = pc::SandboxRealizationFingerprint::from_spec(&pc::SandboxSpec {
        scope: "different".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        control_services: Default::default(),
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
    });
    assert!(
        matches!(
            ContainerRuntime::observe(
                &runtime,
                crate::ContainerObservationExpectation {
                    container_id: "container-1",
                    adoption_fingerprint: Some(&fingerprint),
                    realization_fingerprint: Some(&wrong_fingerprint),
                    runtime_handle: None,
                    effect_fence: None,
                },
            )
            .await
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "R3 wrong fingerprint"
    );
    let without_fingerprint = format!(
        r#"[{{"Id":"container-1","State":"running","Labels":{{"{MANAGED_SANDBOX_LABEL}":"1","{SANDBOX_ADOPTION_LABEL}":"{fingerprint}"}}}}]"#
    );
    let without_fingerprint_response = without_fingerprint.clone();
    let (missing_fingerprint, _) = runtime_with(8080, move |_| ok(&without_fingerprint_response));
    assert!(
        matches!(
            ContainerRuntime::observe(
                &missing_fingerprint,
                crate::ContainerObservationExpectation {
                    container_id: "container-1",
                    adoption_fingerprint: Some(&fingerprint),
                    realization_fingerprint: Some(&fingerprint),
                    runtime_handle: None,
                    effect_fence: None,
                },
            )
            .await
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "R3 missing fingerprint"
    );

    let (missing, _) = runtime_with(8080, |_| ok("[]"));
    assert_eq!(
        ContainerRuntime::observe(
            &missing,
            crate::ContainerObservationExpectation {
                container_id: "container-1",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: None,
                effect_fence: None,
            },
        )
        .await
        .unwrap(),
        pc::SandboxObservation::DefinitivelyUnavailable {
            physical_incarnation: Some("container-1".into()),
        },
        "R3"
    );
    assert!(
        matches!(
            ContainerRuntime::observe(
                &missing,
                crate::ContainerObservationExpectation {
                    container_id: "container-1",
                    adoption_fingerprint: None,
                    realization_fingerprint: None,
                    runtime_handle: None,
                    effect_fence: None,
                },
            )
            .await
            .unwrap(),
            pc::SandboxObservation::Incompatible { .. }
        ),
        "R5"
    );

    let (unavailable, _) = runtime_with(8080, |_| err("daemon unavailable"));
    assert!(
        ContainerRuntime::observe(
            &unavailable,
            crate::ContainerObservationExpectation {
                container_id: "container-1",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: None,
                effect_fence: None,
            },
        )
        .await
        .is_err(),
        "R4"
    );
}

#[tokio::test]
async fn package_preparation_has_a_fail_closed_deadline() {
    let namespace = ContainerRealizationNamespace::from_stable_parts(["podman-tests"])
        .expect("constant test namespace");
    let runtime = PodmanRuntime::with_exec(namespace, 8080, Arc::new(BlockingExec))
        .with_package_build_timeout(std::time::Duration::from_millis(10));
    let requirements = pc::PackageRequirements {
        managers: [("npm".to_owned(), vec!["cowsay@1.6.0".to_owned()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let error = ContainerRuntime::prepare_package_image(
        &runtime,
        "base:1",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect_err("a stuck package resolver must not activate the base image");
    assert!(
        error.to_string().contains("exceeded its deadline"),
        "{error}"
    );
}

fn plan() -> ContainerPlan {
    ContainerPlan {
        image: "img:latest".into(),
        command: vec!["/agent".into()],
        env: vec![],
        control_services: Default::default(),
        packages: Default::default(),
        binds: vec![],
        outputs_volume: "/out".into(),
        network: NetworkMode::None,
        egress_identity: crate::EgressRealizationIdentity {
            network: NetworkMode::None,
            proxy_endpoint: None,
            capability_ttl_secs: None,
            issuer_revision: None,
            ephemeral_capability: false,
        },
        requests: pc::ResourceRequests::default(),
        limits: pc::ResourceLimits::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        memory_mounts: vec![],
        rootfs: RootfsPlan::Image("img:latest".into()),
    }
}

/// Rootless-cgroup FMECA cause/effect graph. C1 XDG_RUNTIME_DIR contains the
/// user-manager Unix bus; C2 the endpoint is missing or a regular file; C3
/// the parent may carry an unrelated desktop-session bus. Effects: E1 select
/// the user-manager bus for every Podman subprocess (overriding C3); E2 do
/// not fabricate an address, leaving Podman to fail closed.
///
/// | Rule | Runtime bus | Ambient desktop bus | Effect |
/// |---|---|---|---|
/// | B1 | Unix socket | any | E1 |
/// | B2 | absent/non-socket | any | E2 |
#[test]
fn rootless_systemd_bus_decision_table_selects_only_user_manager_authority() {
    let runtime = tempfile::tempdir().expect("runtime dir");
    assert_eq!(
        rootless_systemd_bus(Some(runtime.path().as_os_str())),
        None,
        "B2 an absent endpoint cannot be fabricated",
    );

    std::fs::write(runtime.path().join("bus"), b"not a socket").expect("regular file");
    assert_eq!(
        rootless_systemd_bus(Some(runtime.path().as_os_str())),
        None,
        "B2 a regular file is not trusted as a user-manager bus",
    );
    std::fs::remove_file(runtime.path().join("bus")).expect("remove regular file");
    let _listener = std::os::unix::net::UnixListener::bind(runtime.path().join("bus"))
        .expect("user bus fixture");
    let expected: OsString = format!("unix:path={}", runtime.path().join("bus").display()).into();
    assert_eq!(
        rootless_systemd_bus(Some(runtime.path().as_os_str())),
        Some(expected),
        "B1 the canonical user-manager socket is selected for Podman",
    );
}

#[test]
fn signal_flag_maps_every_signal() {
    assert_eq!(signal_flag(pc::Signal::Term), "TERM");
    assert_eq!(signal_flag(pc::Signal::Kill), "KILL");
    assert_eq!(signal_flag(pc::Signal::Int), "INT");
}

#[test]
fn executable_is_constructor_owned_without_ambient_precedence() {
    // Cause/effect table:
    // | constructor input | executable |
    // | default | `podman` on PATH |
    // | explicit typed deployment value | exact supplied path |
    let rt = PodmanRuntime::new(9000);
    assert_eq!(rt.agent_port, 9000);
    assert_eq!(rt.bin, "podman");
    assert_eq!(
        PodmanRuntime::with_bin(9000, "/opt/podman").bin,
        "/opt/podman"
    );
}

#[tokio::test]
async fn run_maps_a_nonzero_exit_to_a_backend_error_naming_the_subcommand() {
    let (rt, _) = runtime_with(9000, |_| err("boom"));
    let e = rt.run(&["info".into()]).await.unwrap_err();
    assert!(
        matches!(e, RuntimeError::Backend(m) if m.contains("podman info") && m.contains("boom"))
    );
}

#[tokio::test]
async fn ping_succeeds_when_the_binary_responds() {
    let (rt, _) = runtime_with(9000, |_| ok("x86_64"));
    assert!(rt.ping().await.is_ok());
}

#[test]
fn registry_auth_file_is_scoped_to_pull_and_push() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let namespace = ContainerRealizationNamespace::from_stable_parts(["podman-tests"])
        .expect("constant test namespace");
    let rt = PodmanRuntime::with_exec(
        namespace,
        9000,
        Arc::new(FakeExec {
            handler: Box::new(|_| ok("")),
            calls: Mutex::new(Vec::new()),
        }),
    )
    .with_package_registry("registry.internal")
    .with_package_registry_auth_file(file.path())
    .unwrap();
    let pull = rt.registry_command("pull", "registry.internal/awaken-packages@sha256:abc");
    assert_eq!(pull[0], "pull");
    assert_eq!(pull[1], "--authfile");
    assert_eq!(pull[2], file.path().to_string_lossy());
    assert_eq!(pull[3], "registry.internal/awaken-packages@sha256:abc");
}

#[tokio::test]
async fn registry_mode_repairs_a_missing_remote_from_the_local_cache() {
    let (rt, fake) = runtime_with(9000, |args| match args.first().map(String::as_str) {
        Some("pull") => err("manifest unknown"),
        Some("push") => ok(""),
        Some("image") if args.get(1).map(String::as_str) == Some("exists") => ok(""),
        Some("image") if args.iter().any(|arg| arg.contains("RepoDigests")) => {
            ok("registry.internal/awaken-packages@sha256:remote")
        }
        Some("image") => ok("sha256:exact-base"),
        other => panic!("unexpected podman command: {other:?}"),
    });
    let rt = rt.with_package_registry("registry.internal");
    let requirements = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let image = ContainerRuntime::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .unwrap();
    assert_eq!(image, "registry.internal/awaken-packages@sha256:remote");
    let calls = fake.calls.lock().unwrap();
    let pull = calls
        .iter()
        .position(|args| args.first().map(String::as_str) == Some("pull"))
        .unwrap();
    let push = calls
        .iter()
        .position(|args| args.first().map(String::as_str) == Some("push"))
        .unwrap();
    assert!(pull < push, "remote probe must precede repair push");
    assert!(
        !calls
            .iter()
            .any(|args| args.first().map(String::as_str) == Some("build")),
        "a deterministic local hit repairs the registry without rebuilding"
    );
}

/// Podman package-image cause graph:
/// mutable base reference -> exact local image ID; exact ID + exact package
/// requirements -> content-addressed Containerfile/tag -> cache probe.
/// Cache miss builds exactly once; cache hit performs no build. A workload
/// container is never created by this operation.
///
/// | Rule | base inspect | cache | observable behavior |
/// |---|---|---|---|
/// | P1 | exact ID | miss | build once FROM exact ID; return derived ref |
/// | P2 | exact ID | hit | return derived ref without a build |
/// | P3 | missing/empty | n/a | fail before cache probe/build |
#[tokio::test]
async fn package_requirements_build_one_content_addressed_image_on_cache_miss() {
    let captured = Arc::new(Mutex::new(None::<String>));
    let captured_build = captured.clone();
    let (rt, fake) = runtime_with(9000, move |args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
            (Some("image"), Some("exists")) => err("not found"),
            (Some("build"), _) => {
                let file = args
                    .iter()
                    .position(|arg| arg == "--file")
                    .and_then(|index| args.get(index + 1))
                    .expect("build carries Containerfile");
                *captured_build.lock().unwrap() =
                    Some(std::fs::read_to_string(file).expect("Containerfile exists during build"));
                ok("")
            }
            other => panic!("unexpected podman command: {other:?}"),
        }
    });
    let requirements = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let image = ContainerRuntime::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect("cache miss builds");
    assert!(image.starts_with("localhost/awaken-packages:"));
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 3, "P1: inspect, cache probe, then build");
    assert_eq!(&calls[0][..2], &["image", "inspect"]);
    assert_eq!(&calls[1][..2], &["image", "exists"]);
    assert_eq!(calls[2].first().map(String::as_str), Some("build"));
    let file = captured.lock().unwrap().clone().unwrap();
    assert!(file.starts_with("FROM sha256:exact-base\n"), "P1: {file}");
    assert!(
        file.contains(r#"RUN ["/usr/bin/env","pip","install","--no-cache-dir","httpx==0.28.0"]"#)
    );
}

#[tokio::test]
async fn package_image_cache_hit_does_not_build_or_create_a_workload() {
    let (rt, fake) = runtime_with(9000, |args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok("sha256:exact-base"),
            (Some("image"), Some("exists")) => ok(""),
            other => panic!("P2 forbids build/run calls: {other:?}"),
        }
    });
    let requirements = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let image = ContainerRuntime::prepare_package_image(
        &rt,
        "python:3.13",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect("P2 cache hit");

    assert!(image.starts_with("localhost/awaken-packages:"));
    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "P2: inspect and cache probe only");
    assert_eq!(&calls[0][..2], &["image", "inspect"]);
    assert_eq!(&calls[1][..2], &["image", "exists"]);
}

#[tokio::test]
async fn missing_base_identity_fails_before_cache_or_build_side_effects() {
    let (rt, fake) = runtime_with(9000, |args| {
        match (
            args.first().map(String::as_str),
            args.get(1).map(String::as_str),
        ) {
            (Some("image"), Some("inspect")) => ok(""),
            other => panic!("P3 forbids cache/build calls: {other:?}"),
        }
    });
    let requirements = pc::PackageRequirements {
        managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
            .into_iter()
            .collect(),
        ..Default::default()
    };

    let error = ContainerRuntime::prepare_package_image(
        &rt,
        "missing:latest",
        &requirements,
        &pc::NetworkPolicy::Unrestricted,
    )
    .await
    .expect_err("P3 empty identity fails closed");

    assert!(error.to_string().contains("empty base-image identity"));
    assert_eq!(fake.calls.lock().unwrap().len(), 1, "P3: inspect only");
}

#[tokio::test]
async fn legacy_create_only_creates_an_absent_scope() {
    // Cause/effect table: C1 the stable scope query is empty/occupied; C2
    // the legacy call has no durable fence. R1 empty may use the canonical
    // create path and publish the port; R2 occupied must fail closed in the
    // shared kernel and never run `rm`.
    let (rt, fake) = runtime_with(7777, |args| match args.first().map(String::as_str) {
        Some("ps") => ok("[]"),
        Some("run") => ok("container-1"),
        other => panic!("unexpected Podman command: {other:?}"),
    });
    let expected = runtime_container_name(rt.realization_namespace.as_str(), "s1").unwrap();
    let id = rt.create("s1", &plan()).await.unwrap();
    assert_eq!(id, "container-1", "R1");
    let calls = fake.calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .all(|call| call.first().map(String::as_str) != Some("rm")),
        "R2"
    );
    let run = calls
        .iter()
        .find(|call| call.first().map(String::as_str) == Some("run"))
        .unwrap();
    let name_at = run.iter().position(|a| a == &expected).unwrap();
    assert!(
        run[name_at + 1..]
            .windows(2)
            .any(|pair| pair == ["-p", "127.0.0.1::7777"]),
        "R1"
    );
}

#[tokio::test]
async fn create_response_loss_is_typed_as_may_have_committed() {
    /* Mutation-outcome cause/effect table. C1 the stable scope is absent;
     * C2 `podman run` returns a failure after the adapter crosses the
     * mutation boundary; C3 the same authoritative re-observation still
     * sees no exact object. C1+C2+C3 => E1 MayHaveCommitted, never the
     * ordinary Backend rejection that authorizes participant teardown.
     * Preflight/discovery errors remain Backend in the existing observation
     * table. Docker and Kubernetes call the same `after_mutation` owner at
     * their corresponding create/start/remove effect edges. */
    let (runtime, _) = runtime_with(7777, |args| match args.first().map(String::as_str) {
        Some("ps") => ok("[]"),
        Some("run") => err("injected response loss"),
        other => panic!("unexpected Podman command: {other:?}"),
    });

    let error = runtime
        .create("may-have-committed", &plan())
        .await
        .expect_err("E1 response loss cannot be a definite rejection");
    assert!(matches!(error, RuntimeError::MayHaveCommitted(_)), "E1");
}

#[tokio::test]
async fn agent_addr_parses_the_published_host_port_taking_the_first_binding() {
    let (rt, _) = runtime_with(9000, |_| ok("127.0.0.1:49153\n[::]:49153"));
    let addr = rt.agent_addr("cid").await.unwrap();
    assert_eq!(addr, "127.0.0.1:49153".parse().unwrap());
}

#[tokio::test]
async fn agent_addr_errs_when_nothing_is_published_yet() {
    let (rt, _) = runtime_with(9000, |_| ok(""));
    assert!(rt.agent_addr("cid").await.is_err());
}

/// Cold-start bounded-retry-then-fail-closed: when the agent's port is NEVER
/// published (`podman port` keeps returning empty), `open_channel` must retry a
/// BOUNDED number of times and then fail closed rather than spin forever — so a
/// genuinely dead agent still surfaces an error. Driven entirely through the scripted
/// `CommandExec` (no daemon, no binary); `start_paused` auto-advances the backoff so
/// the ~6s bound resolves instantly and deterministically. This exercises the SAME
/// loop shape the (non-injectable, bollard-bound) `docker::open_channel` runs.
#[tokio::test(start_paused = true)]
async fn open_channel_retries_a_bounded_number_then_fails_closed() {
    // Every `podman port` reports nothing published → agent_addr errs each attempt.
    let (rt, fake) = runtime_with(9000, |_| ok(""));
    let e = rt.open_channel("cid").await;
    assert!(
        e.is_err(),
        "a never-reachable agent must fail closed, not hang"
    );
    // The retry is bounded (the loop is `for _ in 0..40`): exactly 40 port lookups
    // were attempted, then it gave up — never an unbounded spin.
    let port_attempts = fake
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|argv| argv.first().map(String::as_str) == Some("port"))
        .count();
    assert_eq!(
        port_attempts, 40,
        "open_channel must retry a bounded number of times then fail closed"
    );
}

#[tokio::test]
async fn inspect_maps_exact_json_status_and_preserves_backend_errors() {
    // Status cause/effect table: C1 exact Awaken container is running/terminal;
    // C2 the successful exact-id query is empty; C3 Podman returns an error.
    // R1 running=>Running; R2 terminal=>Gone; R3 empty=>Gone as non-destructive
    // status only; R4 backend error remains Err and is never collapsed to Gone.
    let (running, _) = runtime_with(9000, |_| {
        ok(r#"[{"Id":"cid","State":"running","Labels":{"awaken.sandbox":"1"}}]"#)
    });
    assert!(
        matches!(
            running.inspect("cid").await.unwrap(),
            ContainerState::Running
        ),
        "R1"
    );
    let (stopped, _) = runtime_with(9000, |_| {
        ok(r#"[{"Id":"cid","State":"exited","Labels":{"awaken.sandbox":"1"}}]"#)
    });
    assert!(
        matches!(stopped.inspect("cid").await.unwrap(), ContainerState::Gone),
        "R2"
    );
    let (missing, _) = runtime_with(9000, |_| ok("[]"));
    assert!(
        matches!(missing.inspect("cid").await.unwrap(), ContainerState::Gone),
        "R3"
    );
    let (unavailable, _) = runtime_with(9000, |_| err("daemon unavailable"));
    assert!(unavailable.inspect("cid").await.is_err(), "R4");
}

#[tokio::test]
async fn wait_parses_the_exit_code_and_rejects_garbage() {
    let (rt, _) = runtime_with(9000, |_| ok("0"));
    assert_eq!(rt.wait("cid").await.unwrap().code, Some(0));
    let (bad, _) = runtime_with(9000, |_| ok("not-a-number"));
    assert!(bad.wait("cid").await.is_err());
}

#[tokio::test]
async fn poll_is_none_while_running_and_carries_the_code_once_exited() {
    let (running, _) = runtime_with(9000, |_| ok("running 0"));
    assert_eq!(running.poll("cid").await.unwrap(), None);
    let (exited, _) = runtime_with(9000, |_| ok("exited 3"));
    assert_eq!(exited.poll("cid").await.unwrap().unwrap().code, Some(3));
    // Malformed second field → code None, still terminal.
    let (weird, _) = runtime_with(9000, |_| ok("exited"));
    assert_eq!(weird.poll("cid").await.unwrap().unwrap().code, None);
}

#[tokio::test]
async fn signal_forwards_the_mapped_flag() {
    let (rt, fake) = runtime_with(9000, |_| ok(""));
    rt.signal("cid", pc::Signal::Kill).await.unwrap();
    assert_eq!(
        *fake.calls.lock().unwrap().last().unwrap(),
        vec!["kill", "--signal", "KILL", "cid"]
    );
}

#[tokio::test]
async fn artifacts_are_out_of_band_and_touch_lease_requires_a_live_container() {
    // Lease cause/effect table: C1 artifact enumeration is out-of-band and has
    // no Podman effect; C2 exact status is running/absent/backend failure.
    // R1 C1=>empty without a CLI call; R2 running=>renewal succeeds after one
    // exact observation; R3 absent/error=>renewal fails closed.
    let (rt, fake) = runtime_with(9000, |_| {
        ok(r#"[{"Id":"cid","State":"running","Labels":{"awaken.sandbox":"1"}}]"#)
    });
    assert!(rt.artifacts("cid").await.unwrap().is_empty());
    assert!(fake.calls.lock().unwrap().is_empty(), "R1");
    assert!(rt.touch_lease("cid").await.is_ok());
    assert_eq!(fake.calls.lock().unwrap().len(), 1, "R2");
    let (gone, _) = runtime_with(9000, |_| ok("[]"));
    assert!(gone.touch_lease("cid").await.is_err(), "R3 absent");
    let (unavailable, _) = runtime_with(9000, |_| err("daemon unavailable"));
    assert!(unavailable.touch_lease("cid").await.is_err(), "R3 error");
}

#[tokio::test]
async fn read_artifact_returns_the_tar_stream_or_maps_the_error() {
    let (rt, fake) = runtime_with(9000, |_| ok("TARBYTES"));
    assert_eq!(
        rt.read_artifact("cid", "/out/f").await.unwrap(),
        b"TARBYTES"
    );
    assert_eq!(
        *fake.calls.lock().unwrap().last().unwrap(),
        vec!["cp", "cid:/out/f", "-"]
    );
    let (missing, _) = runtime_with(9000, |_| err("no such file"));
    assert!(missing.read_artifact("cid", "/nope").await.is_err());
}

#[tokio::test]
async fn remove_force_deletes_the_container() {
    let (rt, fake) = runtime_with(9000, |_| ok(""));
    rt.remove("cid").await.unwrap();
    assert_eq!(
        *fake.calls.lock().unwrap().last().unwrap(),
        vec!["rm", "-f", "cid"]
    );
}

fn exec_process(child: Option<Child>, bin: &str) -> PodmanExecProcess {
    PodmanExecProcess {
        id: "exec-test".into(),
        container_id: "container-test".into(),
        bin: bin.into(),
        pid_file: "/tmp/does-not-matter-for-scripted-bin".into(),
        state: tokio::sync::Mutex::new(PodmanExecState {
            child,
            status: None,
        }),
    }
}

#[tokio::test]
async fn exec_process_wait_and_poll_cache_the_terminal_status() {
    let child = OsCommand::new("sh")
        .args(["-c", "exit 7"])
        .spawn()
        .expect("spawn fixture");
    let process = exec_process(Some(child), "true");
    assert_eq!(process.id(), "exec-test");
    assert_eq!(process.wait().await.unwrap().code, Some(7));
    assert_eq!(process.wait().await.unwrap().code, Some(7));
    assert_eq!(process.poll().await.unwrap().unwrap().code, Some(7));
}

#[tokio::test]
async fn exec_process_poll_reports_running_then_terminal() {
    let child = OsCommand::new("sh")
        .args(["-c", "sleep 0.05; exit 3"])
        .spawn()
        .expect("spawn fixture");
    let process = exec_process(Some(child), "true");
    assert_eq!(process.poll().await.unwrap(), None);
    assert_eq!(process.wait().await.unwrap().code, Some(3));
}

#[tokio::test]
async fn detached_exec_process_fails_closed_and_signal_propagates_status() {
    // Cause/effect graph: C1 process handle attached/detached; C2 signal CLI
    // exits zero/non-zero/cannot spawn. Effects: E1 detached handles reject
    // every lifecycle operation; E2 attached+zero accepts; E3 attached with
    // non-zero or spawn failure rejects while the child remains observable.
    // Decision rules P1 detached=>E1; P2 attached+zero=>E2; P3
    // attached+non-zero=>E3; P4 attached+spawn-failure=>E3. FMECA: using a
    // detached fixture to test CLI status bypasses the authoritative child
    // state and can hide fail-open signaling (S6/O3/D5).
    let detached = exec_process(None, "true");
    assert!(detached.wait().await.is_err());
    assert!(detached.poll().await.is_err());
    assert!(detached.signal(pc::Signal::Term).await.is_err(), "P1/E1");

    let child = OsCommand::new("sh")
        .args(["-c", "sleep 10"])
        .spawn()
        .expect("spawn success fixture");
    let successful_signal = exec_process(Some(child), "true");
    successful_signal
        .signal(pc::Signal::Term)
        .await
        .expect("P2/E2");
    successful_signal
        .state
        .lock()
        .await
        .child
        .as_mut()
        .unwrap()
        .start_kill()
        .unwrap();
    successful_signal.wait().await.unwrap();

    let child = OsCommand::new("sh")
        .args(["-c", "sleep 10"])
        .spawn()
        .expect("spawn failure fixture");
    let failing_signal = exec_process(Some(child), "false");
    assert!(
        failing_signal.signal(pc::Signal::Int).await.is_err(),
        "P3/E3"
    );
    failing_signal
        .state
        .lock()
        .await
        .child
        .as_mut()
        .unwrap()
        .start_kill()
        .unwrap();
    failing_signal.wait().await.unwrap();

    let child = OsCommand::new("sh")
        .args(["-c", "sleep 10"])
        .spawn()
        .expect("spawn missing-binary fixture");
    let missing_binary = exec_process(Some(child), "/definitely/missing/podman");
    assert!(
        missing_binary.signal(pc::Signal::Kill).await.is_err(),
        "P4/E3"
    );
    missing_binary
        .state
        .lock()
        .await
        .child
        .as_mut()
        .unwrap()
        .start_kill()
        .unwrap();
    missing_binary.wait().await.unwrap();
}

#[tokio::test]
async fn exec_admission_rejects_empty_and_unsupported_piped_commands() {
    let (rt, _) = runtime_with(9000, |_| ok(""));

    assert!(
        rt.exec_process(
            "cid",
            pc::MaterializedCommand::new(Vec::<String>::new()),
            false
        )
        .is_err()
    );

    let mut piped = pc::MaterializedCommand::new(["echo", "value"]);
    piped.stdio = pc::Stdio::Piped;
    assert!(rt.exec_process("cid", piped, false).is_err());
}

#[tokio::test]
async fn spawn_and_attached_spawn_cover_stdio_cwd_and_inline_environment() {
    let (mut rt, _) = runtime_with(9000, |_| ok(""));
    // `true` is a deterministic stand-in for the Podman CLI. It ignores the
    // assembled `exec ...` argv while preserving the exact child stdio shape.
    rt.bin = "true".into();

    let mut inherited = pc::Command::new(["echo", "inherited"]);
    inherited.cwd = "/workspace".into();
    inherited.env.push(pc::EnvVar {
        name: "MODE".into(),
        value: pc::EnvValue::Inline {
            value: "test".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let inherited = materialized(inherited).await;
    let inherited = rt.spawn("cid", inherited).await.unwrap();
    assert_eq!(inherited.wait().await.unwrap().code, Some(0));

    let mut null = pc::MaterializedCommand::new(["echo", "discarded"]);
    null.stdio = pc::Stdio::Null;
    let null = rt.spawn("cid", null).await.unwrap();
    assert_eq!(null.wait().await.unwrap().code, Some(0));

    let mut piped = pc::MaterializedCommand::new(["agent", "--stdio"]);
    piped.stdio = pc::Stdio::Piped;
    let attached = rt.spawn_agent("cid", piped).await.unwrap();
    assert_eq!(attached.process.wait().await.unwrap().code, Some(0));
}

/// Podman secret-delivery cause graph:
///
/// C1 command contains a brokered process secret -> C2 the Worker resolves it
/// -> C3 the Podman adapter forwards only an adapter-owned alias in argv and
/// places the value in the CLI environment -> E1 the container wrapper can
/// restore the target name while the host command line remains secret-free.
/// The live Podman test proves the wrapper-to-target half of this boundary.
///
/// | Rule | C1 | C2 | value in argv | value in child env | Result |
/// |---|---|---|---|---|---|
/// | D1 | T | T | F | alias only | launch succeeds |
/// | D2 | T | T | T | * | helper rejects observation |
#[cfg(unix)]
#[tokio::test]
async fn process_secret_is_forwarded_by_alias_without_entering_podman_argv() {
    // A committed read-only fixture avoids the ETXTBSY race created by
    // writing and executing a temporary script while tests spawn commands
    // concurrently; that race confounds D1 with fixture failure (S3/O4/D2).
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/podman-secret-check.sh");

    let (mut rt, _) = runtime_with(9000, |_| ok(""));
    rt.bin = script.to_string_lossy().into_owned();
    let mut command = pc::Command::new(["echo", "value"]);
    command.stdio = pc::Stdio::Null;
    command.env.push(pc::EnvVar {
        name: "TOKEN".into(),
        value: pc::EnvValue::Secret {
            reference: "lease://exact".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let broker: Arc<dyn pc::SecretBroker> = Arc::new(FixedBroker);
    let command = pc::materialize_process_command(&[], command, Some(&broker))
        .await
        .unwrap();
    let process = rt.spawn("cid", command).await.unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));
}
