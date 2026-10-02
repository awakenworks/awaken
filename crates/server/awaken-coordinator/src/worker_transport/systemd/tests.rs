use super::*;
use crate::worker_transport::WorkerTransport;

fn running(unit: &str, invocation_id: &str) -> String {
    format!(
        "Id={unit}\nLoadState=loaded\nInvocationID={invocation_id}\nActiveState=active\nSubState=running\nMainPID=42\nControlGroup=/user.slice/app.slice/{unit}\nKillMode=control-group\nRestart=no\nRemainAfterExit=yes\n"
    )
}

#[test]
fn local_worker_launch_environment_admits_only_safe_single_line_inputs() {
    // Causes: C1 a process-placement key is in the closed allowlist; C2 its
    // value has a newline; C3 an ambient model/proxy credential key exists.
    // Effects: E1 C1+!C2 yields one exact assignment; E2 C2 refuses before
    // unit launch; E3 C3 is never projected into the Worker command.
    // | Rule | C1 | C2 | C3 | Effect |
    // | L1   | Y  | N  | -  | E1     |
    // | L2   | Y  | Y  | -  | E2     |
    // | L3   | N  | -  | Y  | E3     |
    assert_eq!(
        launch_assignment("HOME", std::ffi::OsStr::new("/tmp/owner")).unwrap(),
        std::ffi::OsString::from("HOME=/tmp/owner"),
        "L1/E1"
    );
    assert!(
        launch_assignment("HOME", std::ffi::OsStr::new("/tmp/owner\nforeign")).is_err(),
        "L2/E2"
    );
    for key in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "HTTPS_PROXY"] {
        assert!(!LOCAL_WORKER_ENVIRONMENT_KEYS.contains(&key), "L3/E3 {key}");
    }
}

#[test]
fn systemd_binding_and_exit_decision_table_rejects_replacements_and_incomplete_policy() {
    // C0 exact local-process incarnation derives one unit name for launcher
    // and evidence; malformed input is refused before a launch or bind. C1
    // exact unit/invocation; C2 running MainPID and matching cgroup; C3
    // control-group kill, no restart and retained status. R1=C1+C2+C3 binds.
    // R2 any missing/wrong cause refuses binding. C4 exact failed/exited state
    // with no PID/cgroup -> E1 exit; R3 C1+C3+C4=>E1. A later invocation or
    // merely inactive unit never borrows the old binding's stop receipt.
    let unit = "awaken-agent-worker-0123456789abcdef0123456789abcdef.service";
    assert_eq!(
        WorkerTransport::systemd_user_unit_name("0123456789abcdef0123456789abcdef").unwrap(),
        unit,
        "C0 exact shared unit coordinate"
    );
    assert!(
        WorkerTransport::systemd_user_unit_name("foreign/worker").is_err(),
        "C0 malformed identity is rejected"
    );
    let invocation = "fedcba9876543210fedcba9876543210";
    let boot_id = uuid::Uuid::nil().to_string();
    let source = running(unit, invocation);
    let observation = UnitObservation::parse(&source).unwrap();
    let binding = observation
        .bind(unit, &boot_id)
        .expect("R1 exact running unit");
    for changed in [
        source.replace("MainPID=42", "MainPID=0"),
        source.replace("Restart=no", "Restart=always"),
        source.replace("KillMode=control-group", "KillMode=process"),
        source.replace("RemainAfterExit=yes", "RemainAfterExit=no"),
        source.replace(&format!("app.slice/{unit}"), "app.slice/foreign.service"),
        source.replace(invocation, ""),
    ] {
        assert!(
            UnitObservation::parse(&changed)
                .unwrap()
                .bind(unit, &boot_id)
                .is_none(),
            "R2"
        );
    }
    assert!(
        observation.bind("foreign.service", &boot_id).is_none(),
        "R2 foreign unit"
    );
    let exited_text = source
        .replace("ActiveState=active", "ActiveState=failed")
        .replace("SubState=running", "SubState=failed")
        .replace("MainPID=42", "MainPID=0")
        .replace(
            &format!("ControlGroup=/user.slice/app.slice/{unit}"),
            "ControlGroup=",
        );
    let exited = UnitObservation::parse(&exited_text).unwrap();
    assert!(exited.exact_exit(&binding), "R3");
    let retired = UnitObservation::parse(&format!(
        "Id={unit}\nLoadState=not-found\nActiveState=inactive\nMainPID=0\nControlGroup=\n"
    ))
    .unwrap();
    assert!(
        retired.retired_after_receipt(unit),
        "R4 retired transient unit"
    );
    assert!(
        !exited.retired_after_receipt(unit),
        "R4 failed unit can still be restarted"
    );
    assert!(
        !UnitObservation::parse(
            &exited_text.replace(invocation, "0123456789abcdef0123456789abcdef")
        )
        .unwrap()
        .exact_exit(&binding),
        "R3 foreign invocation"
    );
    assert!(
        UnitObservation::parse("Id=x\nId=x\n").is_err(),
        "R2 duplicate property"
    );
}

#[test]
fn systemd_exit_receipt_is_durable_and_exact_after_adapter_reopen() {
    // C1 valid process binding; C2 independent exit observation has already
    // completed; C3 the adapter process restarts; C4 a foreign invocation is
    // queried. R1=C1+C2+C3 => same durable receipt; R2=C4 => reject instead of
    // treating another incarnation's bytes as local exit evidence.
    let root = tempfile::tempdir().unwrap();
    let evidence_dir = root.path().join("evidence");
    let adapter = SystemdWorkerEvidence::new("systemd-user", &evidence_dir).unwrap();
    let binding = ProcessBinding {
        unit: "awaken-agent-worker-0123456789abcdef0123456789abcdef.service".into(),
        invocation_id: "fedcba9876543210fedcba9876543210".into(),
        boot_id: current_boot_id().unwrap(),
        control_group:
            "/user.slice/app.slice/awaken-agent-worker-0123456789abcdef0123456789abcdef.service"
                .into(),
        main_pid: 42,
    };
    let receipt = ExitReceipt {
        binding: binding.clone(),
        observed_state: "failed:failed".into(),
    };
    adapter.write_receipt(&receipt).unwrap();
    drop(adapter);
    let reopened = SystemdWorkerEvidence::new("systemd-user", &evidence_dir).unwrap();
    assert_eq!(
        reopened.read_receipt(&binding).unwrap(),
        Some(receipt),
        "R1"
    );
    let foreign = ProcessBinding {
        invocation_id: "0123456789abcdef0123456789abcdef".into(),
        ..binding
    };
    assert!(reopened.read_receipt(&foreign).is_err(), "R2");
}

#[test]
fn boot_replacement_needs_a_changed_kernel_boot_and_no_reused_unit() {
    // Causes: C1 exact persisted Worker boot; C2 observed kernel boot differs;
    // C3 original unique unit name is now absent; C4 a new unit reuses that
    // name. Effects: E1 old host process cannot still run; E2 stay Unknown.
    // B1=C1+C2+C3&&!C4=>E1; B2=!C2|!C3|C4=>E2. A systemd manager restart
    // within one kernel boot satisfies neither C2 nor the positive rule.
    let unit = "awaken-agent-worker-0123456789abcdef0123456789abcdef.service";
    let invocation = "fedcba9876543210fedcba9876543210";
    let old_boot = uuid::Uuid::nil().to_string();
    let next_boot = uuid::Uuid::now_v7().to_string();
    let binding = UnitObservation::parse(&running(unit, invocation))
        .unwrap()
        .bind(unit, &old_boot)
        .unwrap();
    let gone = UnitObservation::parse(&format!(
        "Id={unit}\nLoadState=not-found\nActiveState=inactive\nMainPID=0\nControlGroup=\n"
    ))
    .unwrap();
    let reused = UnitObservation::parse(&running(unit, invocation)).unwrap();
    assert!(
        boot_replacement_is_closed(&binding, &next_boot, &gone),
        "B1"
    );
    assert!(
        !boot_replacement_is_closed(&binding, &old_boot, &gone),
        "B2 same boot"
    );
    assert!(
        !boot_replacement_is_closed(&binding, &next_boot, &reused),
        "B2 reused unit"
    );
}

#[tokio::test]
async fn live_systemd_unit_stop_survives_adapter_reopen() {
    // C0 the manager and trusted launcher executable are available; C1 actual
    // systemd user manager starts one unique, no-restart unit; C2
    // Coordinator binds before the process effect; C3 stop kills its cgroup;
    // C4 the adapter reopens from disk after systemd drops InvocationID; C5
    // process crash after durable receipt but before StopUnit; C6 the existing
    // Flow placement monitor requests graceful drain without writing a stop
    // receipt. L1=C1..C4 =>
    // exact retained exit; L2=C1+C2+C3+C5 => replay finishes the same stop.
    // Replacement or unavailable manager stays Unknown. Provider I/O is real.
    if std::env::var("AWAKEN_SYSTEMD_WORKER_E2E").as_deref() != Ok("1") {
        eprintln!("skipping live systemd Worker: AWAKEN_SYSTEMD_WORKER_E2E=1 is required");
        return;
    }
    assert!(WorkerTransport::systemd_user_available().await, "C0/E0");
    let systemd_run = WorkerTransport::systemd_user_run_executable().expect("C0 trusted runner");
    struct Probe(String);
    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", &self.0])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "reset-failed", &self.0])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    for response_lost in [false, true] {
        let incarnation_id = uuid::Uuid::now_v7().simple().to_string();
        let worker = WorkerIdentity::new("systemd-probe", &incarnation_id, 1);
        let unit = unit_name(&worker).unwrap();
        let _cleanup = Probe(unit.clone());
        let started = std::process::Command::new(&systemd_run)
            .args([
                "--user",
                &format!("--unit={unit}"),
                "--property=RemainAfterExit=yes",
                "--property=KillMode=control-group",
                "--property=Restart=no",
                "/bin/sleep",
                "600",
            ])
            .output()
            .unwrap();
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let root = tempfile::tempdir().unwrap();
        let evidence_dir = root.path().join("evidence");
        let adapter = SystemdWorkerEvidence::new("systemd-user", &evidence_dir).unwrap();
        let binding = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(attempt) = adapter.binding_for(&worker).await {
                    break serde_json::from_str::<ProcessBinding>(attempt.execution_ref()).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("L1/L2 running exact unit");
        assert!(
            WorkerTransport::systemd_user_unit_running(&incarnation_id)
                .await
                .unwrap(),
            "C6 placement sees the exact running invocation"
        );
        if response_lost {
            adapter
                .systemctl(&["kill", "--kill-who=all", "--signal=SIGKILL", &unit])
                .await
                .unwrap();
            let exited = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let observed = adapter.show(&unit).await.unwrap();
                    if observed.exact_exit(&binding) {
                        break observed;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("L2 exact process exit");
            adapter
                .write_receipt(&ExitReceipt {
                    binding: binding.clone(),
                    observed_state: format!(
                        "{}:{}",
                        exited.value("ActiveState"),
                        exited.value("SubState")
                    ),
                })
                .unwrap();
        } else {
            WorkerTransport::systemd_user_request_drain(&incarnation_id)
                .await
                .expect("C6 asks the original main process to drain");
            assert!(
                adapter.stop_binding(&binding).await.unwrap().is_some(),
                "L1 stop"
            );
        }
        drop(adapter);
        let reopened = SystemdWorkerEvidence::new("systemd-user", &evidence_dir).unwrap();
        assert!(
            reopened.stop_binding(&binding).await.unwrap().is_some(),
            "L1/L2 replay"
        );
        assert!(
            !WorkerTransport::systemd_user_unit_running(&incarnation_id)
                .await
                .unwrap(),
            "C6 retired unit cannot be reattached"
        );
        assert!(
            reopened.read_receipt(&binding).unwrap().is_some(),
            "L1/L2 persisted"
        );
    }
}
