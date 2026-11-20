//! One provider-owned quiescence path and its scope/exit proof.
use super::*;
use awaken_provisioning_contract::Sandbox as _;

#[tokio::test]
async fn resident_hand_is_the_only_pid1_path_and_receives_no_platform_credential() {
    /*
     * Resident placement cause/effect graph and decision table.
     * Causes: C1 resident config absent/present; C2 executable blank; C3 port
     * zero; C4 Session spec contains a Process secret; C5 resource limits are
     * positive/zero; C6 the resident Hand executes Bash without an attached
     * process boundary. Effects: E1 legacy
     * keepalive command; E2 exactly one `hand --listen` command under the init reaper; E3 durable ledger
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

#[tokio::test(start_paused = true)]
async fn quiescence_checks_source_before_and_after_and_never_assumes_exit() {
    // Q1 configured source + both guards + observed zero exit -> proof;
    // Q2 absent provider capability/pre-guard failure -> no spawn;
    // Q3 spawn/error/nonzero/missing status/timeout -> no proof;
    // Q4 source changed after observed exit -> no proof; Q5 replay -> same
    // source, both guards repeated. Every row seals NEW Session execs, retains
    // the original handle, and performs no physical removal or mutable harvest.
    // The fixture proves orchestration; the real Docker/ACK cases own process
    // state, namespace and filesystem evidence.
    for rule in [
        "quiet",
        "unsupported",
        "before",
        "spawn",
        "nonzero",
        "unknown",
        "timeout",
        "after",
    ] {
        let runtime = Arc::new(FakeRuntime::default());
        {
            let mut state = runtime.st.lock().unwrap();
            state.resident_quiescence = rule != "unsupported";
            state.resident_guard_fail_at = match rule {
                "before" => Some(1),
                "after" => Some(2),
                _ => None,
            };
            state.fail_spawn = rule == "spawn";
            state.exec_outcome = match rule {
                "nonzero" => Some(pc::ExitStatus {
                    code: Some(125),
                    signaled: false,
                }),
                "unknown" => Some(pc::ExitStatus {
                    code: None,
                    signaled: false,
                }),
                _ => None,
            };
            state.exec_wait_forever = rule == "timeout";
        }
        let p = provider(runtime.clone())
            .with_resident_hand(ResidentHandConfig::new("/opt/awaken-sandbox", 7777).unwrap());
        assert_eq!(p.supports_resident_quiescence(), rule != "unsupported");
        let sandbox = p
            .create_container(&spec("resident-quiescence"))
            .await
            .unwrap();
        let handle = sandbox.handle();
        assert_eq!(
            sandbox.quiesce_resident().await.is_ok(),
            rule == "quiet",
            "{rule}"
        );
        assert_eq!(sandbox.handle(), handle);
        assert!(
            sandbox.spawn(pc::Command::new(["late"])).await.is_err(),
            "{rule}/admission"
        );
        {
            let state = runtime.st.lock().unwrap();
            assert!(state.removals.is_empty(), "{rule}");
            assert_eq!(
                state.spawned.len(),
                usize::from(!matches!(rule, "unsupported" | "before" | "spawn")),
                "{rule}"
            );
            assert_eq!(
                state.resident_guard_calls,
                if matches!(rule, "quiet" | "after") {
                    2
                } else {
                    1
                },
                "{rule}"
            );
        }
        if rule == "quiet" {
            // Trusted capture remains on the SAME tracked machinery spawn;
            // closing Session admission must not strand checkpoint export.
            let process = sandbox
                .spawn_agent_process(
                    pc::SessionProcessPurpose::Maintenance,
                    pc::Command::new(["tar", "--version"]),
                )
                .await
                .unwrap();
            assert_eq!(process.process.wait().await.unwrap().code, Some(0));
            sandbox.quiesce_resident().await.unwrap();
            assert_eq!(runtime.st.lock().unwrap().resident_guard_calls, 4, "Q5");
        }
    }
}
