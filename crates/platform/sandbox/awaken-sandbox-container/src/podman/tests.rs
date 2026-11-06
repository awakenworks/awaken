// @formal evidence session_realization.concurrent_phase_drivers_are_serialized_per_session
//! Behavior tests for the Podman runtime adapter.
//!
//! This remains a child of `podman`, retaining access to the private adapter
//! seams and the original cause/effect decision tables.

use awaken_container_contract::RuntimeError;
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
        let authorization = crate::test_support::disposal_authorization(
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
        let mounts = serde_json::json!([{"Source": stage.path().join("live-inputs"), "Destination":awaken_container_contract::LIVE_INPUTS_ROOT}]).to_string();
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
                let mut current = changing
                    .current
                    .lock()
                    .unwrap_or_else(|poisoned| panic!("{poisoned}"));
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
        "Destination": awaken_container_contract::LIVE_INPUTS_ROOT,
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
                purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                    purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                    purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                    purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
                purpose: awaken_provisioning_contract::SandboxObservationPurpose::Recovery,
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
    let attempt = awaken_provisioning_contract::SandboxCreateAttempt::new("attempt");
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
async fn startup_evidence_resolves_the_image_and_reads_delegated_controllers() {
    // ADR-0092 decision 5 on the Podman engine. Causes: C1 the default image
    // is present / pullable / neither; C2 the delegated cgroup controllers.
    // Effects: E1 resource limits advertised; E2 withheld; E3 start-up fails
    // (no image means no isolation tier remains).
    // | Rule | C1 | C2 | effect |
    // | I1 | present | cpu+memory+pids | E1, no pull |
    // | I2 | pullable | rootless memory only | E2 after one pull |
    // | I3 | neither | any | E3 |
    let full = r#"["cpu","io","memory","pids"]"#;
    let partial = r#"["memory"]"#;
    for (rule, present, pullable, controllers, expected) in [
        ("I1", true, true, full, Some(true)),
        ("I2", false, true, partial, Some(false)),
        ("I3", false, false, full, None),
    ] {
        let (runtime, fake) = runtime_with(8080, move |args| {
            match args
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["image", "exists", ..] if present => ok(""),
                ["image", "exists", ..] => err("no such image"),
                ["pull", ..] if pullable => ok(""),
                ["pull", ..] => err("manifest unknown"),
                ["info", "--format", "{{json .Host.CgroupControllers}}"] => ok(controllers),
                ["info", ..] => ok("amd64"),
                other => err(&format!("unexpected {other:?}")),
            }
        });
        let provider = crate::ContainerProvider::new(Arc::new(runtime), "sandbox:test");
        let attested = provider.attest_startup().await;
        match expected {
            Some(limits) => {
                assert_eq!(attested.expect(rule).resource_limits, limits, "{rule}");
                assert_eq!(
                    provider.runtime_sandbox_capabilities().resource_limits,
                    limits,
                    "{rule}"
                );
            }
            None => assert!(attested.is_err(), "{rule}"),
        }
        let pulls = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.first().map(String::as_str) == Some("pull"))
            .count();
        assert_eq!(
            pulls,
            usize::from(!present),
            "{rule} pulls only a missing image"
        );
    }
}
