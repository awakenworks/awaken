//! Real Linux qualification without systemd or delegated cgroups.
//! No successful prerequisite skips; Worker integration is a separate gate.
#![cfg(target_os = "linux")]
use awaken_local_process::scoped::{ExecutionDeadline, ScopedCommand, ScopedProcess};
use awaken_provisioning_contract::{
    self as pc, MaterializedCommand, MaterializedEnvValue, MaterializedEnvVar, ProcessHandle as _,
};
use std::{path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const HELPER: &str = env!("CARGO_BIN_EXE_awaken-sandbox");

#[path = "linux_scoped/provider.rs"]
mod scoped_provider;

async fn launch(command: &MaterializedCommand, cwd: &Path, lifetime: Duration) -> ScopedProcess {
    ScopedProcess::spawn_authorized(
        Path::new("/usr/bin/bwrap"),
        Path::new(HELPER),
        ScopedCommand {
            materialized: command,
            cwd,
            stdio: command.stdio.into(),
        },
        ExecutionDeadline::after(lifetime).unwrap(),
        Duration::from_secs(5),
        |_, grant| grant.commit(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn scoped_loader_cwd_credentials_and_binary_stdio_are_exact() {
    // S1 sanitized launcher + namespace helper + GO => loader/main run inside
    // the boundary, exact nested physical cwd, explicit synthetic credential,
    // independent binary stdio; no inherited control socket. This is NOT real
    // vendor authentication or a fully installed Worker realization.
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("nested 工作目录");
    std::fs::create_dir(&cwd).unwrap();
    let loader = root.path().join("require-scope.so");
    assert!(
        tokio::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Werror"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scoped_loader.c"))
            .arg("-o")
            .arg(&loader)
            .status()
            .await
            .unwrap()
            .success()
    );
    let parent_ns = std::fs::read_link("/proc/self/ns/pid").unwrap();
    let mut command = MaterializedCommand::new([
        "/bin/bash",
        "-c",
        r#"
set -eu
test "$PWD" = "$EXPECTED_CWD"
test "$SCOPE_CREDENTIAL" = fixture-value
if env | grep -q '^HOME='; then exit 36; fi
for fd in /proc/self/fd/*; do
  case "$(readlink "$fd" || true)" in socket:*) exit 35 ;; esac
done
printf stderr-only >&2
/bin/cat
"#,
    ]);
    command.cwd = "/logical/path/must/not/be/reprojected".into();
    command.stdio = pc::Stdio::Piped;
    for (name, value) in [
        ("EXPECTED_PARENT_NS", parent_ns.to_str().unwrap()),
        ("EXPECTED_CWD", cwd.to_str().unwrap()),
        ("LD_PRELOAD", loader.to_str().unwrap()),
    ] {
        command.env.push(MaterializedEnvVar {
            name: name.into(),
            value: MaterializedEnvValue::Inline(value.into()),
        });
    }
    command.env.push(MaterializedEnvVar {
        name: "SCOPE_CREDENTIAL".into(),
        value: MaterializedEnvValue::Secret("fixture-value".to_owned().into()),
    });
    let mut process = launch(&command, &cwd, Duration::from_secs(15)).await;
    let bytes = b"no newline\0\xff\nlast";
    let mut input = process.take_stdin().unwrap();
    input.write_all(bytes).await.unwrap();
    drop(input);
    let (mut output, mut error) = (Vec::new(), Vec::new());
    tokio::time::timeout(
        Duration::from_secs(5),
        process.take_stdout().unwrap().read_to_end(&mut output),
    )
    .await
    .unwrap()
    .unwrap();
    process
        .take_stderr()
        .unwrap()
        .read_to_end(&mut error)
        .await
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));
    assert_eq!(output, bytes);
    assert_eq!(error, b"stderr-only");
}

#[tokio::test]
async fn scoped_graceful_signals_reach_workload_traps_before_tree_exit() {
    // S15 TERM/INT after trap readiness => exact matching workload trap and
    // normal exit, not a substituted SIGKILL. Repeated signals after observed
    // init exit remain idempotent. Detached-tree KILL is covered separately.
    for (signal, expected) in [(pc::Signal::Term, "term"), (pc::Signal::Int, "int")] {
        let mut command = MaterializedCommand::new([
            "/bin/bash",
            "-c",
            r#"trap 'printf term; exit 0' TERM
trap 'printf int; exit 0' INT
printf 'ready\n'
while :; do read -r -t 1 value || true; done"#,
        ]);
        command.stdio = pc::Stdio::Piped;
        let mut process = launch(&command, Path::new("/"), Duration::from_secs(10)).await;
        let _input = process.take_stdin().unwrap();
        let mut output = BufReader::new(process.take_stdout().unwrap());
        let mut ready = String::new();
        tokio::time::timeout(Duration::from_secs(2), output.read_line(&mut ready))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready, "ready\n");
        process.signal(signal).await.unwrap();
        let mut trapped = String::new();
        tokio::time::timeout(Duration::from_secs(3), output.read_to_string(&mut trapped))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(trapped, expected);
        assert_eq!(process.wait().await.unwrap().code, Some(0));
        for again in [pc::Signal::Term, pc::Signal::Int, pc::Signal::Kill] {
            process.signal(again).await.unwrap();
        }
    }
}

#[tokio::test]
async fn scoped_launch_denial_invalid_input_and_failed_exec_never_become_ready() {
    // S2 revoked/omitted grant => no marker; S3 invalid deadline => no launch;
    // S4 GO followed by exec/cwd failure => owned failing process, not ready;
    // S5 physical identity must be observable BEFORE granting execution.
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("must-not-execute");
    let mut command = MaterializedCommand::new([
        "/bin/sh",
        "-c",
        "printf executed > \"$1\"",
        "fixture",
        marker.to_str().unwrap(),
    ]);
    command.stdio = pc::Stdio::Null;
    for omitted in [false, true] {
        let result = ScopedProcess::spawn_authorized(
            Path::new("/usr/bin/bwrap"),
            Path::new(HELPER),
            ScopedCommand {
                materialized: &command,
                cwd: root.path(),
                stdio: command.stdio.into(),
            },
            ExecutionDeadline::after(Duration::from_secs(5)).unwrap(),
            Duration::from_secs(5),
            |identity, _grant| {
                assert_ne!(identity.namespace().inode, 0, "S5");
                assert!(!identity.is_quiescent().unwrap(), "S5 live before GO");
                if omitted {
                    Ok(())
                } else {
                    Err(std::io::ErrorKind::PermissionDenied.into())
                }
            },
        )
        .await;
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(!marker.exists(), "S2");
    }
    assert!(ExecutionDeadline::after(Duration::ZERO).is_err());
    assert!(ExecutionDeadline::after(Duration::MAX).is_err());
    // S10 malformed deployment/timeout inputs reject before any workload;
    // S11 successful GO followed by callback failure still returns ownership.
    for (helper, timeout) in [
        (Path::new("relative-helper"), Duration::from_secs(1)),
        (Path::new(HELPER), Duration::ZERO),
        (Path::new(HELPER), Duration::MAX),
    ] {
        let result = ScopedProcess::spawn_authorized(
            Path::new("/usr/bin/bwrap"),
            helper,
            ScopedCommand {
                materialized: &command,
                cwd: root.path(),
                stdio: command.stdio.into(),
            },
            ExecutionDeadline::after(Duration::from_secs(5)).unwrap(),
            timeout,
            |_, grant| grant.commit(),
        )
        .await;
        assert_eq!(
            result.err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!marker.exists());
    }
    // S14 cutoff expires after JOINED while GO is backpressured: a late GO
    // cannot run the command. Parent and helper clocks are independent checks;
    // observing a failed owned process must not become a ready workload.
    let started = std::time::Instant::now();
    let expired = ScopedProcess::spawn_authorized(
        Path::new("/usr/bin/bwrap"),
        Path::new(HELPER),
        ScopedCommand {
            materialized: &command,
            cwd: root.path(),
            stdio: command.stdio.into(),
        },
        ExecutionDeadline::after(Duration::from_secs(1)).unwrap(),
        Duration::from_secs(5),
        |_, grant| {
            if started.elapsed() < Duration::from_millis(1100) {
                Err(std::io::ErrorKind::WouldBlock.into())
            } else {
                grant.commit()
            }
        },
    )
    .await
    .unwrap();
    assert_ne!(
        tokio::time::timeout(Duration::from_secs(5), expired.wait())
            .await
            .unwrap()
            .unwrap()
            .code,
        Some(0)
    );
    assert!(!marker.exists(), "S14 expired before exec");
    let committed = ScopedProcess::spawn_authorized(
        Path::new("/usr/bin/bwrap"),
        Path::new(HELPER),
        ScopedCommand {
            materialized: &command,
            cwd: root.path(),
            stdio: command.stdio.into(),
        },
        ExecutionDeadline::after(Duration::from_secs(5)).unwrap(),
        Duration::from_secs(5),
        |_, grant| {
            grant.commit()?;
            Err(std::io::Error::other("failure after GO"))
        },
    )
    .await
    .unwrap();
    assert_eq!(committed.wait().await.unwrap().code, Some(0));
    assert_eq!(std::fs::read(&marker).unwrap(), b"executed");
    for (program, cwd) in [
        ("/awaken-missing-command", root.path().to_owned()),
        ("/bin/true", root.path().join("absent")),
    ] {
        let mut command = MaterializedCommand::new([program]);
        command.stdio = pc::Stdio::Null;
        let process = launch(&command, &cwd, Duration::from_secs(5)).await;
        assert_ne!(
            tokio::time::timeout(Duration::from_secs(5), process.wait())
                .await
                .unwrap()
                .unwrap()
                .code,
            Some(0)
        );
    }
}

fn tree_command(port: u16) -> MaterializedCommand {
    let script = r#"
set -eu
case "$1" in
  leader) setsid /bin/bash -c "$0" "$0" child & ;;
  child) setsid /bin/bash -c "$0" "$0" grandchild & ;;
esac
exec 3<>/dev/tcp/127.0.0.1/$SCOPED_TREE_PORT
read -r pid comm state ppid pgrp session rest < /proc/$$/stat
printf '%s %s %s %s\n' "$1" "$pid" "$pgrp" "$session" >&3
if read -r -t 30 -u 3 action; then
  if [ "$action" = kill-helper ]; then
    kill -KILL "$PPID"
    read -r -t 30 -u 3 ignored || true
  fi
fi
"#;
    let mut command = MaterializedCommand::new(["/bin/bash", "-c", script, script, "leader"]);
    command.stdio = pc::Stdio::Null;
    command.env.push(MaterializedEnvVar {
        name: "SCOPED_TREE_PORT".into(),
        value: MaterializedEnvValue::Inline(port.to_string()),
    });
    command
}

#[tokio::test]
#[ignore = "owner participant invoked by scoped_detached_trees_close_on_all_failure_triggers"]
async fn scoped_owner_participant() {
    let port = std::env::var("SCOPED_TREE_PORT").unwrap().parse().unwrap();
    let process = launch(&tree_command(port), Path::new("/"), Duration::from_secs(3)).await;
    if std::env::var("SCOPED_OWNER_RENEWED").as_deref() == Ok("1") {
        process
            .refresh_deadline_authorized(Duration::from_secs(1), |grant| {
                grant.commit(ExecutionDeadline::after(Duration::from_secs(6))?)
            })
            .await
            .unwrap();
        println!("SCOPED_RENEWAL_SENT");
    }
    let mut byte = [0];
    let _ = tokio::io::stdin().read(&mut byte).await;
}

type ObservedNode = (String, BufReader<tokio::net::TcpStream>);

async fn observe_tree(listener: &tokio::net::TcpListener) -> Vec<ObservedNode> {
    let mut nodes = Vec::new();
    let mut groups = std::collections::BTreeSet::new();
    let mut roles = std::collections::BTreeSet::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
            let fields = line.split_whitespace().collect::<Vec<_>>();
            assert_eq!(fields.len(), 4);
            assert!(roles.insert(fields[0].to_owned()));
            assert!(groups.insert(fields[2].to_owned()));
            if fields[0] != "leader" {
                assert_eq!(fields[1], fields[2]);
                assert_eq!(fields[1], fields[3]);
            }
            nodes.push((fields[0].to_owned(), stream));
        }
    })
    .await
    .expect("three running detached nodes before trigger");
    assert_tree_live(&nodes);
    nodes
}

fn assert_tree_live(nodes: &[ObservedNode]) {
    for (_, node) in nodes {
        assert_eq!(
            node.get_ref().try_read(&mut [0]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

async fn assert_tree_closed(nodes: Vec<ObservedNode>) {
    for (role, mut stream) in nodes {
        match stream.read(&mut [0]).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("{role}: descendant remained alive: {other:?}"),
        }
    }
}

#[tokio::test]
async fn scoped_cutoff_refresh_preserves_authorization_and_eventual_tree_exit() {
    // Decision table S16-S20. Current cutoff live + authorized newer => tree
    // survives ORIGINAL cutoff, then exits at the absolute new cutoff. Stale
    // update must not shorten it. Denied/omitted/expired-input/retry-revoked
    // updates must not extend it. Every row observes three detached members
    // and exact init exit, then rejects a resurrection without calling authority.
    // These are adapter tests; the production Session lease is not simulated.
    for rule in [
        "renew",
        "stale",
        "deny",
        "omit",
        "expired",
        "retry-revoked",
        "timeout",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let started = tokio::time::Instant::now();
        let process = launch(
            &tree_command(listener.local_addr().unwrap().port()),
            Path::new("/"),
            Duration::from_secs(2),
        )
        .await;
        let nodes = observe_tree(&listener).await;
        // S22 invalid timeout rejects before consulting authority. Persistent
        // backpressure times out without extending the original cutoff.
        for invalid_timeout in [Duration::ZERO, Duration::MAX] {
            let error = process
                .refresh_deadline_authorized(invalid_timeout, |_| {
                    panic!("invalid timeout must not reach authority")
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        let mut calls = 0;
        let expired = ExecutionDeadline::after(Duration::from_millis(1)).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        let result = process
            .refresh_deadline_authorized(Duration::from_millis(30), |grant| {
                calls += 1;
                match rule {
                    "renew" => grant.commit(ExecutionDeadline::after(Duration::from_secs(4))?),
                    "stale" => grant.commit(ExecutionDeadline::after(Duration::from_millis(100))?),
                    "deny" => Err(std::io::ErrorKind::PermissionDenied.into()),
                    "omit" => Ok(()),
                    "expired" => grant.commit(expired),
                    "retry-revoked" if calls == 1 => Err(std::io::ErrorKind::WouldBlock.into()),
                    "retry-revoked" => Err(std::io::ErrorKind::PermissionDenied.into()),
                    "timeout" => Err(std::io::ErrorKind::WouldBlock.into()),
                    _ => unreachable!(),
                }
            })
            .await;
        assert_eq!(result.is_ok(), matches!(rule, "renew" | "stale"), "{rule}");
        if let Err(error) = result {
            assert_eq!(
                error.kind(),
                if matches!(rule, "expired" | "timeout") {
                    std::io::ErrorKind::TimedOut
                } else {
                    std::io::ErrorKind::PermissionDenied
                }
            );
        }
        if rule == "timeout" {
            assert!(calls > 1, "backpressure must reauthorize");
        } else {
            assert_eq!(calls, if rule == "retry-revoked" { 2 } else { 1 });
        }
        tokio::time::sleep_until(
            started + Duration::from_millis(if rule == "renew" { 2300 } else { 500 }),
        )
        .await;
        assert_tree_live(&nodes);
        assert!(process.poll().await.unwrap().is_none());
        tokio::time::timeout(
            Duration::from_secs(if rule == "renew" { 3 } else { 2 }),
            async {
                assert_tree_closed(nodes).await;
                process.wait().await.unwrap();
            },
        )
        .await
        .expect("cutoff terminates all detached members and init");
        let mut called_after_exit = false;
        let result = process
            .refresh_deadline_authorized(Duration::from_secs(1), |grant| {
                called_after_exit = true;
                grant.commit(ExecutionDeadline::after(Duration::from_secs(10))?)
            })
            .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert!(!called_after_exit);
    }
}

#[tokio::test]
async fn scoped_detached_trees_close_on_all_failure_triggers() {
    // S6-S9: leader/child/grandchild have distinct process groups and hold their
    // OWN observer sockets. Trigger={scope kill, owner SIGKILL, helper SIGKILL,
    // cutoff, owner SIGSTOP}; all sockets must close before the 30s backstop.
    // S13 SIGSTOP keeps the owner and channel alive but unable to cooperate:
    // the helper's own cutoff must terminate the tree without parent progress.
    // This is a helper deadline, not proof of installed Session lease renewal.
    // Leader wait,
    // forwarding EOF and Drop are not the death oracle. Missing substrate fails.
    for trigger in [
        "scope",
        "owner",
        "helper",
        "deadline",
        "owner-stall",
        "owner-stall-renewed",
    ] {
        // S12 cancellation of a waiter retains the physical owner. Every
        // subsequent target stop must leave this neighboring scope alive.
        let mut neighbor_command = MaterializedCommand::new(["/bin/sleep", "25"]);
        neighbor_command.stdio = pc::Stdio::Null;
        let neighbor = launch(&neighbor_command, Path::new("/"), Duration::from_secs(25)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), neighbor.wait())
                .await
                .is_err()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut renewed_observed = None;
        let mut owner = None;
        let mut scope = None;
        if matches!(trigger, "owner" | "owner-stall" | "owner-stall-renewed") {
            owner = Some(
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "scoped_owner_participant",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("SCOPED_TREE_PORT", port.to_string())
                    .env(
                        "SCOPED_OWNER_RENEWED",
                        if trigger == "owner-stall-renewed" {
                            "1"
                        } else {
                            "0"
                        },
                    )
                    .stdin(std::process::Stdio::piped())
                    .stdout(if trigger == "owner-stall-renewed" {
                        std::process::Stdio::piped()
                    } else {
                        std::process::Stdio::null()
                    })
                    .stderr(std::process::Stdio::inherit())
                    .kill_on_drop(true)
                    .spawn()
                    .unwrap(),
            );
        } else {
            scope = Some(
                launch(
                    &tree_command(port),
                    Path::new("/"),
                    Duration::from_secs(if trigger == "deadline" { 3 } else { 25 }),
                )
                .await,
            );
        }
        let mut nodes = observe_tree(&listener).await;
        if trigger == "owner-stall-renewed" {
            // S21 renewal sent before SIGSTOP; all three nodes must survive the
            // old cutoff without owner progress, then expire at the new one.
            // The participant marker is a send receipt, never a helper ACK;
            // survival past the old deadline is the independent apply oracle.
            let mut output = BufReader::new(owner.as_mut().unwrap().stdout.take().unwrap());
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let mut line = String::new();
                    assert_ne!(output.read_line(&mut line).await.unwrap(), 0);
                    if line.trim() == "SCOPED_RENEWAL_SENT" {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            renewed_observed = Some(tokio::time::Instant::now());
        }
        match trigger {
            "owner" => owner.as_mut().unwrap().kill().await.unwrap(),
            "owner-stall" | "owner-stall-renewed" => {
                assert!(
                    tokio::process::Command::new("/bin/kill")
                        .args(["-STOP", &owner.as_ref().unwrap().id().unwrap().to_string()])
                        .status()
                        .await
                        .unwrap()
                        .success()
                );
            }
            "scope" => scope
                .as_ref()
                .unwrap()
                .signal(pc::Signal::Kill)
                .await
                .unwrap(),
            "helper" => nodes
                .iter_mut()
                .find(|(role, _)| role == "leader")
                .unwrap()
                .1
                .get_mut()
                .write_all(b"kill-helper\n")
                .await
                .unwrap(),
            "deadline" => {}
            _ => unreachable!(),
        }
        if trigger == "owner-stall-renewed" {
            tokio::time::sleep_until(renewed_observed.unwrap() + Duration::from_millis(3300)).await;
            assert_tree_live(&nodes);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            assert_tree_closed(nodes).await;
            if let Some(scope) = scope {
                scope.wait().await.unwrap();
            }
        })
        .await
        .expect("all descendants and init stop before backstop");
        if matches!(trigger, "owner-stall" | "owner-stall-renewed") {
            let owner = owner.as_mut().unwrap();
            assert!(
                owner.try_wait().unwrap().is_none(),
                "S13 owner did not exit"
            );
            owner.kill().await.unwrap();
        }
        assert!(
            neighbor.poll().await.unwrap().is_none(),
            "S12 neighbor survives {trigger}"
        );
        neighbor.signal(pc::Signal::Kill).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), neighbor.wait())
            .await
            .unwrap()
            .unwrap();
    }
}
