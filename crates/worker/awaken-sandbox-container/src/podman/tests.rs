// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Behavior tests for the Podman runtime adapter.
//!
//! This remains a child of `podman`, retaining access to the private adapter
//! seams and the original cause/effect decision tables.

#[cfg(unix)]
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use awaken_provisioning_contract::ProcessHandle;

use crate::{NetworkMode, RootfsPlan};

use super::*;

#[tokio::test]
async fn restored_disposal_keeps_immutable_id_and_current_authority_at_each_adapter_response() {
    use awaken_provisioning_contract::test_support::FenceTamper as _;
    use pc::test_support::EffectAuthority;
    use std::time::Duration;
    // P1 five backend response positions x revoke/replace/expire/renew.
    // Rejection after exists/identity/mount reads preserves staging and sends
    // no rm; rejection after rm reports no success but cannot undo that commit.
    // P2 missing/empty physical IDs reject, never fall back to the mutable
    // restoration name. Every successful rm uses the observed immutable ID.
    // This scripted CommandExec changes authority before returning a response;
    // actual Pending/cancel schedules are owned by the neutral provider table.
    for (boundary, change, identity) in ["none", "exists", "identity", "mounts", "remove"]
        .into_iter()
        .flat_map(|boundary| {
            ["revoke", "replace", "expire", "renew"].map(|change| (boundary, change, "exact-id"))
        })
        .chain([("none", "renew", ""), ("none", "renew", "missing")])
    {
        let stage = tempfile::Builder::new()
            .prefix("awaken-acp-stage-")
            .tempdir()
            .unwrap();
        let payload = stage.path().join("retained");
        std::fs::write(&payload, b"original").unwrap();
        let plan = plan();
        let fingerprint = restoration_plan_fingerprint(&plan);
        let evidence = pc::SandboxRestorationEvidence::from_exact_parts(
            "effect",
            "generation",
            "checkpoint",
            "digest",
            "spec",
            "exclusions",
        )
        .unwrap();
        let authorization = crate::tests::disposal_authorization(
            &pc::SandboxEffectFence::new("restore-cleanup", "owner", "boot", 1, u64::MAX - 1)
                .unwrap(),
        );
        let authority = Arc::new(EffectAuthority::new(authorization.effect_fence()));
        let mut labels = serde_json::Map::new();
        for (key, value) in restoration_metadata(&evidence) {
            labels.insert(key.into(), serde_json::Value::String(value.into()));
        }
        labels.insert(
            crate::RESTORE_PLAN_LABEL.into(),
            serde_json::Value::String(fingerprint.clone()),
        );
        let mut info = serde_json::json!({"Id": identity, "Config":{"Labels":labels}, "State":{"Running":false}});
        if identity == "missing" {
            info.as_object_mut().unwrap().remove("Id");
        }
        let info = info.to_string();
        let mounts = serde_json::json!([{"Source": stage.path().join("live-inputs"), "Destination":crate::LIVE_INPUTS_ROOT}]).to_string();
        let changing = authority.clone();
        let (runtime, commands) = runtime_with(8080, move |args| {
            let selected = match args[0].as_str() {
                "container" => "exists",
                "inspect" if args[2] == "{{json .}}" => "identity",
                "inspect" => "mounts",
                "rm" => "remove",
                _ => panic!("unexpected command: {args:?}"),
            };
            if selected == boundary {
                let mut current = changing.current.lock().unwrap();
                match change {
                    "revoke" => *current = None,
                    "replace" => current.as_mut().unwrap().0.reassign(),
                    "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                    "renew" => current.as_mut().unwrap().0.renew(),
                    _ => unreachable!(),
                }
            }
            match selected {
                "identity" => ok(&info),
                "mounts" => ok(&mounts),
                _ => ok(""),
            }
        });
        let result = runtime
            .dispose_restore_target(
                "stable-target",
                &plan,
                &fingerprint,
                &evidence,
                &authorization,
                authority.as_ref(),
            )
            .await;
        let succeeds = identity == "exact-id" && (boundary == "none" || change == "renew");
        assert_eq!(result.is_ok(), succeeds, "{boundary}/{change}/{identity}");
        let removed = succeeds || (identity == "exact-id" && boundary == "remove");
        let calls = commands.calls.lock().unwrap();
        let removals: Vec<_> = calls.iter().filter(|args| args[0] == "rm").collect();
        assert_eq!(removals.len(), usize::from(removed));
        if let Some(args) = removals.first() {
            assert_eq!(args.last().unwrap(), "exact-id", "P2 immutable target");
        }
        assert_eq!(payload.exists(), !removed);
        if !removed {
            assert_eq!(std::fs::read(&payload).unwrap(), b"original");
        }
        for args in calls
            .iter()
            .filter(|args| args[0] == "inspect" && args[2] == "{{json .Mounts}}")
        {
            assert_eq!(
                args.last().unwrap(),
                "exact-id",
                "P2 all post-identity lookups stay pinned"
            );
        }
    }
}

#[cfg(unix)]
struct FixedBroker;

#[cfg(unix)]
#[async_trait]
impl pc::SecretBroker for FixedBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(b"podman-secret".to_vec())
    }

    async fn materialize_process(&self, reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        self.materialize(reference).await
    }

    async fn write_back(
        &self,
        _target: pc::SecretWritebackTarget<'_>,
        _bytes: Vec<u8>,
    ) -> Result<(), pc::SandboxError> {
        Err(pc::SandboxError::unsupported("not supported"))
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

fn observation_runtime(summary: &str, live_inputs: &std::path::Path) -> PodmanRuntime {
    let summary = summary.to_owned();
    let mounts = serde_json::json!([{
        "Source": live_inputs,
        "Destination": crate::LIVE_INPUTS_ROOT,
    }])
    .to_string();
    runtime_with(8080, move |args| {
        if args.first().map(String::as_str) == Some("inspect") {
            ok(&mounts)
        } else {
            ok(&summary)
        }
    })
    .0
}

#[tokio::test]
async fn observation_preserves_absence_backend_and_host_bind_recovery_causes() {
    // Cause/effect table: C1 `ps --all` succeeds with exact Running/Terminal/empty,
    // or the Podman process fails; C2 durable evidence is V2 exact/legacy; C3
    // the exact live-input bind resolves to a validated provider staging root or
    // is missing/foreign. R1 exact Running+C2+C3=>Ready and transfers only that
    // staging owner; R2 exact Terminal+C2+C3=>Terminal(dispose); R3 exact
    // empty=>Unavailable(Some id); R4 backend or invalid C3 remains
    // indeterminate/fail-closed; R5 legacy empty cannot prove deletion authority.
    let staging = tempfile::Builder::new()
        .prefix("awaken-acp-stage-podman-observation-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let live_inputs = staging.path().join("live-inputs");
    std::fs::create_dir(&live_inputs).unwrap();
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&pc::SandboxSpec {
        process_tree_supervision: false,
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
    let runtime = observation_runtime(&running, &live_inputs);
    assert_eq!(
        ContainerRuntime::observe(
            &runtime,
            crate::ContainerObservationExpectation {
                purpose: crate::ContainerObservationPurpose::Recovery,
                retained_attempt: None,
                container_id: "container-1",
                adoption_fingerprint: Some(&fingerprint),
                realization_fingerprint: Some(&fingerprint),
                runtime_handle: None,
                effect_fence: None,
            },
        )
        .await
        .unwrap(),
        pc::SandboxObservation::Ready,
        "R1"
    );
    let terminal = running.replace("\"State\":\"running\"", "\"State\":\"exited\"");
    let terminal_runtime = observation_runtime(&terminal, &live_inputs);
    assert_eq!(
        ContainerRuntime::observe(
            &terminal_runtime,
            crate::ContainerObservationExpectation {
                purpose: crate::ContainerObservationPurpose::Recovery,
                retained_attempt: None,
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
        process_tree_supervision: false,
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
                    purpose: crate::ContainerObservationPurpose::Recovery,
                    retained_attempt: None,
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
    let missing_fingerprint = observation_runtime(&without_fingerprint_response, &live_inputs);
    assert!(
        matches!(
            ContainerRuntime::observe(
                &missing_fingerprint,
                crate::ContainerObservationExpectation {
                    purpose: crate::ContainerObservationPurpose::Recovery,
                    retained_attempt: None,
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
                purpose: crate::ContainerObservationPurpose::Recovery,
                retained_attempt: None,
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
                    purpose: crate::ContainerObservationPurpose::Recovery,
                    retained_attempt: None,
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
                purpose: crate::ContainerObservationPurpose::Recovery,
                retained_attempt: None,
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
    // B1 requires a Unix user manager; B2 still executes on every platform.
    #[cfg(unix)]
    {
        std::fs::remove_file(runtime.path().join("bus")).expect("remove regular file");
        let _listener = std::os::unix::net::UnixListener::bind(runtime.path().join("bus"))
            .expect("user bus fixture");
        let expected: OsString =
            format!("unix:path={}", runtime.path().join("bus").display()).into();
        assert_eq!(
            rootless_systemd_bus(Some(runtime.path().as_os_str())),
            Some(expected),
            "B1 the canonical user-manager socket is selected for Podman",
        );
    }
}

#[tokio::test]
async fn podman_child_does_not_inherit_worker_service_lifetime() {
    // Cause/effect table: C1 caller is/is not a systemd invocation; C2 unrelated
    // deployment environment exists. R1/R2 both C1 rows -> only the Podman child
    // loses INVOCATION_ID, retains C2, and leaves its parent unchanged. Spawn
    // this same test in isolated processes so no global env mutation races other
    // tests. Actual systemd/Podman/network survival is owned by the Cowork pause
    // matrix; a child-environment check alone cannot qualify physical recovery.
    const CHILD: &str = "AWAKEN_TEST_PODMAN_SERVICE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let before = std::env::var_os("INVOCATION_ID");
        // R3 service + run/start -> peer scope with exact CLI argv. R4 no
        // invocation (including empty) OR query/create/exec/stop/rm -> original
        // call-owned child. No command receives credentials through extra argv.
        for verb in ["run", "start", "info", "create", "exec", "stop", "rm"] {
            let args = vec![verb.into(), "space $ literal ' value".into()];
            let command = podman_subcommand("podman fixture", &args);
            let scoped =
                before.as_ref().is_some_and(|id| !id.is_empty()) && matches!(verb, "run" | "start");
            assert_eq!(
                command.as_std().get_program(),
                if scoped {
                    "systemd-run"
                } else {
                    "podman fixture"
                }
            );
            let mut expected: Vec<std::ffi::OsString> = if scoped {
                [
                    "--user",
                    "--scope",
                    "--quiet",
                    "--collect",
                    "--",
                    "podman fixture",
                ]
                .into_iter()
                .map(Into::into)
                .collect()
            } else {
                Vec::new()
            };
            expected.extend(args.iter().map(Into::into));
            assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), expected);
        }
        if before.as_ref().is_some_and(|id| !id.is_empty()) {
            // R5 activation + unavailable manager -> reject, never run /bin/true
            // directly. This real negative boundary also detects removal of the
            // scope selection; no working systemd manager is needed for refusal.
            let output = podman_subcommand("/bin/true", &["start".into()])
                .env("XDG_RUNTIME_DIR", "/nonexistent/awaken-podman-test-runtime")
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    "unix:path=/nonexistent/awaken-podman-test-bus",
                )
                .output()
                .await;
            if let Ok(output) = output {
                assert!(!output.status.success(), "R5 no unsupervised fallback");
            }
        }
        let status = podman_command("sh")
            .args([
                "-c",
                r#"test -z "${INVOCATION_ID+x}" && test "$AWAKEN_TEST_PODMAN_SERVICE_CHILD" = fixture"#,
            ])
            .status()
            .await
            .unwrap();
        assert!(status.success(), "R1/R2 child scope and unrelated env");
        assert_eq!(std::env::var_os("INVOCATION_ID"), before, "parent intact");
        return;
    }
    for invocation in [None, Some(""), Some("worker-service-invocation")] {
        let mut child = OsCommand::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "podman::tests::podman_child_does_not_inherit_worker_service_lifetime",
                "--nocapture",
            ])
            .env(CHILD, "fixture")
            .env_remove("INVOCATION_ID");
        if let Some(invocation) = invocation {
            child.env("INVOCATION_ID", invocation);
        }
        let output = child.output().await.unwrap();
        assert!(
            output.status.success(),
            "R1/R2 {invocation:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the isolated child must execute its assertion",
        );
    }
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelled_readiness_probe_reaps_the_actual_cli_child() {
    // Cause/effect graph: C1 the real CommandExec has spawned its CLI child;
    // C2 the caller drops the probe; C3 the caller's timeout drops the probe.
    // R1 C1+C2 and R2 C1+C3 -> the OS child is reaped before another probe.
    // A PID-file barrier proves C1; a scripted binary reaches exec sleep, so
    // there is no container or daemon to confuse with the call-owned CLI.
    // Existing ping/nonzero tests own successful/error decoding. Linux /proc
    // checks actual process disappearance, not merely a dropped future flag.
    // The 2s natural exit also bounds a negative-control failure; wait for that
    // cleanup before asserting, rather than orphaning the intentionally broken
    // child. This is CLI cancellation evidence, not physical container shutdown.
    use std::time::Duration;
    for cancellation in ["drop", "timeout"] {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("readiness CLI");
        let pid_file = temp.path().join("readiness CLI.pid");
        // C4 parallel fork can inherit a just-written script's writable fd,
        // making exec fail ETXTBSY before C1. Reuse the committed immutable CLI
        // fixture through a private symlink; $0 keeps the PID barrier private.
        std::os::unix::fs::symlink(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/podman-cli.sh"),
            &binary,
        )
        .unwrap();
        let runtime = PodmanRuntime::with_bin(9000, binary.to_str().unwrap());
        let mut probe = Box::pin(runtime.probe_ready());
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut probe => panic!("C1 probe ended before cancellation: {result:?}"),
                    () = tokio::time::sleep(Duration::from_millis(2)) => {
                        if let Ok(value) = std::fs::read_to_string(&pid_file)
                            && let Ok(pid) = value.parse::<u32>()
                        {
                            break pid;
                        }
                    }
                }
            }
        })
        .await
        .expect("C1 child reached PID barrier");
        let process = PathBuf::from(format!("/proc/{pid}"));
        assert!(process.exists(), "C1 actual child is live");
        if cancellation == "drop" {
            drop(probe);
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(10), probe)
                    .await
                    .is_err()
            );
        }
        let gone_promptly = tokio::time::timeout(Duration::from_millis(500), async {
            while process.exists() {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .is_ok();
        if !gone_promptly {
            tokio::time::timeout(Duration::from_secs(5), async {
                while process.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("negative-control fixture reached its natural exit and was reaped");
        }
        assert!(
            gone_promptly,
            "{cancellation}: cancelled CLI must be reaped, not left running"
        );
    }
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
        Some("create") => ok("container-1"),
        Some("start") => ok("container-1"),
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
    let create = calls
        .iter()
        .find(|call| call.first().map(String::as_str) == Some("create"))
        .unwrap();
    let name_at = create.iter().position(|a| a == &expected).unwrap();
    assert!(
        create[name_at + 1..]
            .windows(2)
            .any(|pair| pair == ["-p", "127.0.0.1::7777"]),
        "R1"
    );
}

#[tokio::test]
async fn fenced_prepare_reserves_then_exact_activation_starts_once() {
    // Cause/effect decision table: C1 scope is absent/prepared/ready; C2 the
    // observed locator is exact/foreign; C3 activation is absent/present.
    // R1 absent prepare issues exactly one `create` and zero `start`; R2 exact
    // prepared activation issues one `start` and returns only after the same
    // locator is Ready; R3 create response, repeated observation and Ready
    // replay never issue a second create/start; R4 the compatibility composer
    // consumes the same phases and returns that active locator without a second
    // activation. Foreign-locator rejection is owned by the shared existing-
    // realization decision table.
    let running = Arc::new(AtomicBool::new(false));
    let created = Arc::new(AtomicBool::new(false));
    let running_response = running.clone();
    let created_response = created.clone();
    let spec = pc::SandboxSpec {
        process_tree_supervision: false,
        scope: "phased".into(),
        isolation: pc::IsolationClass::Container,
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::None,
        outputs_path: pc::WorkspaceLayout::OUTPUTS_ROOT.into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        control_services: Default::default(),
        lease_ttl_secs: None,
    };
    let fingerprint = pc::SandboxRealizationFingerprint::from_spec(&spec);
    let fence = pc::SandboxEffectFence::new("effect", "owner", "runtime", 1, u64::MAX).unwrap();
    let attempt = crate::ContainerCreateAttempt::new("attempt");
    let intent = ContainerRealizationIntent::Create;
    let context = ContainerRealizationContext::new(
        &spec.scope,
        &fingerprint,
        Some(&fence),
        &intent,
        &attempt,
    );
    let fingerprint_label = fingerprint.to_string();
    let (runtime, calls) = runtime_with(7777, move |args| match args[0].as_str() {
        "ps" if !created_response.load(Ordering::SeqCst) => ok("[]"),
        "ps" => {
            let state = if running_response.load(Ordering::SeqCst) {
                "running"
            } else {
                "created"
            };
            ok(&format!(
                r#"[{{"Id":"container-1","State":"{state}","Labels":{{"{MANAGED_SANDBOX_LABEL}":"1","{SANDBOX_ADOPTION_LABEL}":"{fingerprint_label}","{SANDBOX_REALIZATION_LABEL}":"{fingerprint_label}","{SANDBOX_ATTEMPT_LABEL}":"attempt","{SANDBOX_EFFECT_LABEL}":"effect","{SANDBOX_EFFECT_OWNER_LABEL}":"owner","{SANDBOX_EFFECT_RUNTIME_LABEL}":"runtime","{SANDBOX_EFFECT_EPOCH_LABEL}":"1","{SANDBOX_EFFECT_EXPIRY_LABEL}":"{}"}}}}]"#,
                u64::MAX
            ))
        }
        "create" => {
            created_response.store(true, Ordering::SeqCst);
            ok("container-1")
        }
        "inspect" => ok("[]"),
        "start" => {
            running_response.store(true, Ordering::SeqCst);
            ok("container-1")
        }
        command => panic!("unexpected Podman command: {command}"),
    });
    let prepared = runtime
        .prepare_for_effect(&context, &plan(), &fingerprint)
        .await
        .unwrap();
    assert_eq!(prepared.locator, "container-1", "R1");
    assert_eq!(
        prepared.disposition,
        crate::PreparedContainerDisposition::FreshReservation,
        "R1"
    );
    assert_eq!(
        calls
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|args| args[0] == "start")
            .count(),
        0,
        "R1"
    );
    let active = runtime
        .activate_for_effect(&context, &plan(), &fingerprint, &prepared.locator)
        .await
        .unwrap();
    assert_eq!(active, prepared.locator, "R2");
    {
        let recorded = calls.calls.lock().unwrap();
        assert_eq!(
            recorded.iter().filter(|args| args[0] == "create").count(),
            1,
            "R3"
        );
        assert_eq!(
            recorded.iter().filter(|args| args[0] == "start").count(),
            1,
            "R2/R3"
        );
    }
    assert_eq!(
        runtime
            .create_for_effect(&context, &plan(), &fingerprint)
            .await
            .unwrap(),
        prepared.locator,
        "R4"
    );
    let recorded = calls.calls.lock().unwrap();
    assert_eq!(
        recorded.iter().filter(|args| args[0] == "create").count(),
        1,
        "R4"
    );
    assert_eq!(
        recorded.iter().filter(|args| args[0] == "start").count(),
        1,
        "R4"
    );
}

#[tokio::test]
async fn create_response_loss_is_typed_as_may_have_committed() {
    /* Mutation-outcome cause/effect table. C1 the stable scope is absent;
     * C2 `podman create` returns a failure after the adapter crosses the
     * mutation boundary; C3 the same authoritative re-observation still
     * sees no exact object. C1+C2+C3 => E1 MayHaveCommitted, never the
     * ordinary Backend rejection that authorizes participant teardown.
     * Preflight/discovery errors remain Backend in the existing observation
     * table. Docker and Kubernetes call the same `after_mutation` owner at
     * their corresponding create/start/remove effect edges. */
    let (runtime, _) = runtime_with(7777, |args| match args.first().map(String::as_str) {
        Some("ps") => ok("[]"),
        Some("create") => err("injected response loss"),
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
async fn exec_pending_wait_keeps_poll_and_signal_available() {
    // P5 actual local CLI child waits for stdin; wait is polled before another
    // caller polls/signals. Pending wait must not hold the child mutex. P6 drop
    // that waiter, deliver input to the same child, and retain exact exit 7.
    // This uses the existing scripted CLI boundary: true accepts the signal
    // command, it does not attest actual container-side signal delivery (KRP2
    // and real Podman lifecycle tests own that proof). Pipe input fixes liveness
    // without a sleep guess, and kill_on_drop bounds a failing fixture.
    let mut child = OsCommand::new("sh")
        .args(["-c", "read input; exit 7"])
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let process = exec_process(Some(child), "true");
    let mut waiting = Box::pin(process.wait());
    std::future::poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending(), "P5 wait entered");
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), process.poll())
            .await
            .expect("P5 poll remains available")
            .unwrap(),
        None
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        process.signal(pc::Signal::Term),
    )
    .await
    .expect("P5 signal preflight remains available")
    .unwrap();
    drop(waiting);
    tokio::io::AsyncWriteExt::write_all(&mut input, b"finish\n")
        .await
        .unwrap();
    assert_eq!(
        process.wait().await.unwrap().code,
        Some(7),
        "P6 original exit"
    );
    assert_eq!(
        process.poll().await.unwrap().unwrap().code,
        Some(7),
        "P6 stable observation"
    );
}

/// A live child for the signal fixtures.
///
/// `exec` so the shell is replaced instead of forking: `start_kill` then
/// reaches the process that is actually sleeping. Without it the shell dies
/// and its `sleep` is orphaned onto init, still holding the test binary's
/// stdout until it exits on its own — which is what nextest reported as a
/// leak. The fixture's output is nobody's, so it is not inherited either.
fn signal_fixture_child() -> tokio::process::Child {
    OsCommand::new("sh")
        .args(["-c", "exec sleep 10"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("signal fixture child")
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

    let child = signal_fixture_child();
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

    let child = signal_fixture_child();
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

    let child = signal_fixture_child();
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
    let inherited = rt
        .spawn(crate::AdmittedExec::unrecorded("cid"), inherited)
        .await
        .unwrap();
    assert_eq!(inherited.wait().await.unwrap().code, Some(0));

    let mut null = pc::MaterializedCommand::new(["echo", "discarded"]);
    null.stdio = pc::Stdio::Null;
    let null = rt
        .spawn(crate::AdmittedExec::unrecorded("cid"), null)
        .await
        .unwrap();
    assert_eq!(null.wait().await.unwrap().code, Some(0));

    let mut piped = pc::MaterializedCommand::new(["agent", "--stdio"]);
    piped.stdio = pc::Stdio::Piped;
    let attached = rt
        .spawn_agent(crate::AdmittedExec::unrecorded("cid"), piped)
        .await
        .unwrap();
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
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/podman-cli.sh");

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
    let process = rt
        .spawn(crate::AdmittedExec::unrecorded("cid"), command)
        .await
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));
}

#[tokio::test]
async fn package_build_network_reaches_the_podman_build_argv() {
    // The same cause/effect table Docker's `NetworkMode` selection obeys, read
    // off the real `podman build` argv. B1 Unrestricted without the opt-in ->
    // the engine's own build network, so no `--network` flag at all;
    // B2 Unrestricted with the opt-in -> `--network host`; B3 None -> always
    // `--network none`, opt-in or not; B4 Allowlist -> refused before any
    // build. The knob existed on Docker alone until the maximal-feature gate
    // compiled this tier, so the deployment setting silently did nothing here.
    async fn build_argv(network: pc::NetworkPolicy, host_opt_in: bool) -> Option<Vec<String>> {
        let (rt, fake) = runtime_with(9000, |args| match args.first().map(String::as_str) {
            Some("image") if args.get(1).map(String::as_str) == Some("exists") => {
                err("no such image")
            }
            Some("image") => ok("sha256:exact-base"),
            Some("build") => ok(""),
            other => panic!("unexpected podman command: {other:?}"),
        });
        let rt = rt.with_package_build_host_network(host_opt_in);
        let requirements = pc::PackageRequirements {
            managers: [("pip".into(), vec!["httpx==0.28.0".into()])]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let outcome =
            ContainerRuntime::prepare_package_image(&rt, "python:3.13", &requirements, &network)
                .await;
        if matches!(network, pc::NetworkPolicy::Allowlist { .. }) {
            let error = outcome.expect_err("B4 an allowlist has no no-bypass build network");
            assert!(
                error.to_string().contains("no-bypass allowlist network"),
                "B4 states why it refused: {error}"
            );
        } else {
            outcome.expect("B1-B3 build the package image");
        }
        let calls = fake.calls.lock().unwrap();
        calls
            .iter()
            .find(|args| args.first().map(String::as_str) == Some("build"))
            .cloned()
    }

    fn network_flag(argv: &[String]) -> Option<&str> {
        argv.iter()
            .position(|arg| arg == "--network")
            .and_then(|index| argv.get(index + 1))
            .map(String::as_str)
    }

    let b1 = build_argv(pc::NetworkPolicy::Unrestricted, false)
        .await
        .expect("B1 builds");
    assert_eq!(
        network_flag(&b1),
        None,
        "B1 keeps the engine's build network"
    );
    let b2 = build_argv(pc::NetworkPolicy::Unrestricted, true)
        .await
        .expect("B2 builds");
    assert_eq!(
        network_flag(&b2),
        Some("host"),
        "B2 opts in to the host network"
    );
    for opt_in in [false, true] {
        let b3 = build_argv(pc::NetworkPolicy::None, opt_in)
            .await
            .expect("B3 builds");
        assert_eq!(
            network_flag(&b3),
            Some("none"),
            "B3 a denied Environment denies its build too, opt-in {opt_in}"
        );
    }
    for opt_in in [false, true] {
        assert!(
            build_argv(
                pc::NetworkPolicy::Allowlist {
                    hosts: vec!["pypi.org".into()],
                },
                opt_in,
            )
            .await
            .is_none(),
            "B4 refuses before any build, opt-in {opt_in}"
        );
    }

    let namespace = ContainerRealizationNamespace::from_stable_parts(["podman-tests"])
        .expect("constant test namespace");
    let rt = PodmanRuntime::with_exec(
        namespace,
        9000,
        Arc::new(FakeExec {
            handler: Box::new(|_| ok("")),
            calls: Mutex::new(Vec::new()),
        }),
    );
    assert!(
        !rt.package_build_host_network,
        "the opt-in is off by default"
    );
    assert!(
        rt.with_package_build_host_network(true)
            .package_build_host_network,
        "the deployment selects it explicitly"
    );
}
