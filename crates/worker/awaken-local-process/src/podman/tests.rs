//! Cause/effect tests for the canonical Podman child-process environment.

#[cfg(unix)]
use std::ffi::OsString;

use tokio::process::Command as OsCommand;

use super::*;

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
