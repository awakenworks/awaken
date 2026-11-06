use super::*;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "isolated PATH participant invoked by native_probe_public_readiness_tracks_host_policy"]
async fn native_probe_readiness_participant() {
    use pc::SandboxReadiness as _;
    let launcher = std::env::var_os("AWAKEN_PROBE_TEST_LAUNCHER").unwrap();
    let enabled = std::path::PathBuf::from(launcher).with_extension("enabled");
    let root = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(root.path());
    assert!(provider.probe_ready().await.is_err(), "P9 initially denied");
    std::fs::write(&enabled, b"enabled").unwrap();
    assert!(provider.probe_ready().await.is_ok(), "P10 newly enabled");
    std::fs::remove_file(&enabled).unwrap();
    assert!(provider.probe_ready().await.is_err(), "P11 revoked again");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_probe_public_readiness_tracks_host_policy() {
    // P9-P11 exercise PUBLIC provider readiness in a separate process with an
    // exact fixture launcher. Initial deny -> enable -> revoke must be observed
    // on the same provider. Caching either success or failure violates a row.
    // PATH is never changed in this test process, so concurrent tests cannot
    // consume this fixture. The separate real Namespace test proves the kernel.
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let launcher = root.path().join("bwrap");
    std::fs::write(&launcher, b"#!/bin/sh\nfor arg do\n  if [ \"$arg\" = --unshare-pid ] && [ ! -f \"$0.enabled\" ]; then exit 41; fi\ndone\n").unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = TokioCommand::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "namespace::probe_tests::native_probe_readiness_participant",
            "--ignored",
            "--nocapture",
        ])
        .env("PATH", root.path())
        .env("AWAKEN_PROBE_TEST_LAUNCHER", &launcher)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_probe_uses_the_real_profile_and_rechecks_changed_substrate() {
    // Cause/effect table P1-P4: user namespace works, PID namespace is denied
    // or allowed, and permission changes between observations. The historical
    // user-only probe returns true (P1); the production profile must fail (P2),
    // then succeed after enablement (P3), then fail after revocation (P4).
    // The controlled launcher models host permission, NOT kernel enforcement;
    // namespace_hand_stdio owns actual parent-death/kernel qualification.
    let root = tempfile::tempdir().unwrap();
    let enabled = root.path().join("pid-enabled");
    let argv = linux_probe_argv();
    for flag in [
        "--unshare-user",
        "--disable-userns",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-net",
        "--die-with-parent",
        "--new-session",
        "--proc",
    ] {
        assert_eq!(argv.iter().filter(|arg| *arg == flag).count(), 1, "{flag}");
    }
    assert!(argv.ends_with(&["--".into(), "/usr/bin/true".into()]));
    assert!(
        argv.windows(3).any(|arguments| {
            arguments
                == [
                    "--ro-bind-fd",
                    &process::runtime_root_fd(0).unwrap().to_string(),
                    PROBE_RUNTIME_ROOT,
                ]
        }),
        "P12 real profile requires a pinned runtime directory"
    );
    let mut fixture = vec![
        "/bin/sh".into(), "-c".into(),
        "enabled=$1; shift; for arg do if [ \"$arg\" = --unshare-pid ] && [ ! -f \"$enabled\" ]; then exit 41; fi; done".into(),
        "probe-fixture".into(), enabled.to_str().unwrap().into(),
    ];
    let timeout = Duration::from_secs(1);
    fixture.push("--unshare-user".into());
    assert!(run_native_probe_argv(&fixture, timeout).await, "P1");
    fixture.pop();
    fixture.extend_from_slice(&argv[1..]);
    assert!(!run_native_probe_argv(&fixture, timeout).await, "P2");
    std::fs::write(&enabled, b"enabled").unwrap();
    assert!(run_native_probe_argv(&fixture, timeout).await, "P3");
    std::fs::remove_file(&enabled).unwrap();
    assert!(!run_native_probe_argv(&fixture, timeout).await, "P4");
}

/// P12 namespaces alone are available but the launcher rejects descriptor
/// binds -> full readiness fails. P13 the same launcher accepts the required
/// option -> succeeds. This proves option admission, not kernel enforcement;
/// the real syscall/Namespace suites own that independent evidence.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn readiness_requires_runtime_directory_fd_support() {
    let argv = linux_probe_argv();
    for (script, expected) in [
        (
            "for arg do if [ \"$arg\" = --ro-bind-fd ]; then exit 42; fi; done",
            false,
        ),
        ("exit 0", true),
    ] {
        let mut fixture = vec!["/bin/sh".into(), "-c".into(), script.into(), "probe".into()];
        fixture.extend_from_slice(&argv[1..]);
        assert_eq!(
            run_native_probe_argv(&fixture, Duration::from_secs(1)).await,
            expected,
            "P12/P13"
        );
    }
}

#[tokio::test]
async fn native_probe_rejects_failure_and_bounds_a_stuck_launcher() {
    // P5 exact zero exit => ready; P6 nonzero exit, missing executable, empty
    // command, disabled deadline => unavailable. P7 hung launcher => timeout
    // and termination request, never readiness. The fixture uses exec (no
    // detached descendants); whole-tree death has its own OS integration test.
    // P8 probes carry no ambient HOME/loader/token environment into the child.
    // A real process start can exceed one second under Rosetta and concurrent
    // builds. P5/P6 classify the exit without a latency claim; only P7 owns
    // the short timeout bound, so its 100 ms deadline remains unchanged.
    let normal_timeout = Duration::from_secs(5);
    for (argv, expected) in [
        (
            vec![
                "/bin/sh",
                "-c",
                "test -z \"${HOME+x}\" && test -z \"${LD_PRELOAD+x}\"",
            ],
            true,
        ),
        (vec!["/bin/sh", "-c", "exit 23"], false),
        (vec!["/awaken-missing-probe"], false),
        (vec![], false),
    ] {
        let argv = argv.into_iter().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(run_native_probe_argv(&argv, normal_timeout).await, expected);
    }
    let argv = ["/bin/sh", "-c", "exec /bin/sleep 30"].map(str::to_owned);
    assert!(!run_native_probe_argv(&argv, Duration::ZERO).await);
    let hung_timeout = Duration::from_millis(100);
    let started = Instant::now();
    assert!(!run_native_probe_argv(&argv, hung_timeout).await);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "P7 bounded wait"
    );
}
