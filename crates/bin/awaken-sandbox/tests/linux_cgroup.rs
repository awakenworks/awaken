//! Real bootstrap + cgroup-v2/systemd qualification. Explicit Linux gate only;
//! this is not evidence that production Worker/Session integration exists.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use awaken_local_process::LocalProcess;
use awaken_local_process::cgroup::{CgroupIdentity, LinuxProcessScope};
use awaken_provisioning_contract::{
    MaterializedCommand, MaterializedEnvValue, MaterializedEnvVar, ProcessHandle as _,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const PARTICIPANT: &str = "linux_cgroup_participant";

fn own_cgroup() -> PathBuf {
    let membership = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let path = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .unwrap();
    PathBuf::from("/sys/fs/cgroup").join(path)
}

fn helper() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_BIN_EXE_awaken-sandbox"))
}

fn payload_command() -> MaterializedCommand {
    let mut command = MaterializedCommand::new([
        "/bin/bash",
        "-c",
        r#"
trap '' TERM
printf 'LEADER\n'
/usr/bin/setsid /bin/bash -c 'trap "" TERM; printf "DETACHED\n"; /bin/sleep 45' &
wait
"#,
    ]);
    command.stdio = awaken_provisioning_contract::Stdio::Piped;
    command
}

async fn payload(
    scope: &LinuxProcessScope,
) -> (LocalProcess, BufReader<tokio::process::ChildStdout>) {
    let mut child = LocalProcess::spawn_scoped(
        scope,
        helper(),
        &payload_command(),
        std::path::Path::new("/"),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let mut output = BufReader::new(child.take_stdout().unwrap());
    // Production bootstrap owns admission/membership/GO; these are workload
    // readiness lines, never substitutes for that private handshake.
    for expected in ["LEADER", "DETACHED"] {
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(5), output.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert!(!line.is_empty(), "payload exited before {expected}");
            if line.trim() == expected {
                break;
            }
        }
    }
    (child, output)
}

#[tokio::test]
#[ignore = "trusted participant invoked by the explicit systemd gate"]
async fn linux_cgroup_participant() {
    let mode = std::env::var("AWAKEN_CGROUP_TEST_MODE").unwrap();
    let parent = own_cgroup();
    let path = parent.join("owned");
    let neighbor_path = parent.join("neighbor");
    std::fs::create_dir(&path).unwrap();
    std::fs::create_dir(&neighbor_path).unwrap();
    let scope = LinuxProcessScope::open(&path).unwrap();
    let neighbor = LinuxProcessScope::open(&neighbor_path).unwrap();
    if mode == "crash" {
        let mut command = payload_command();
        // All descendants inherit the OUTER observer's pipe directly. An owner
        // forwarding task would give false EOF when only the owner dies.
        command.stdio = awaken_provisioning_contract::Stdio::Inherit;
        let process = LocalProcess::spawn_scoped(
            &scope,
            helper(),
            &command,
            std::path::Path::new("/"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        println!("OWNER_READY");
        // No close, Rust Drop or voluntary child exit is on the asserted path.
        let _retained = (process, neighbor);
        std::future::pending::<()>().await;
        unreachable!();
    }
    assert_eq!(mode, "control");
    bootstrap_contract(&scope, &parent).await;
    let (process, mut output) = payload(&scope).await;
    // CG18 parent-scope kill includes nested cgroups, not just members listed
    // in the parent's cgroup.procs. The neighbor remains outside that subtree.
    let nested_path = path.join("nested");
    std::fs::create_dir(&nested_path).unwrap();
    let nested = LinuxProcessScope::open(&nested_path).unwrap();
    let (nested_process, mut nested_output) = payload(&nested).await;
    let (neighbor_process, mut neighbor_output) = payload(&neighbor).await;
    let wrong = CgroupIdentity {
        inode: scope.identity().inode ^ 1,
        ..scope.identity()
    };
    assert!(
        scope.enter_current_process(wrong).is_err(),
        "CG4 wrong identity cannot move owner"
    );
    assert_eq!(own_cgroup(), parent);

    let permit = scope.launch_permit().await.unwrap();
    assert_eq!(
        scope
            .stop(Duration::from_millis(20))
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut,
        "CG5 in-flight admission blocks close"
    );
    drop(permit);
    // Cancelling the lock waiter must not orphan the mutex or erase children.
    {
        let _permit = scope.launch_permit().await.unwrap();
        let stop = scope.stop(Duration::from_secs(3));
        tokio::pin!(stop);
        tokio::select! {
            biased;
            result = &mut stop => panic!("CG6 stop passed active permit: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        // End the actual future's scope, not just its Pin<&mut _> binding.
    }
    let (first, retry) = tokio::join!(
        scope.stop(Duration::from_secs(3)),
        scope.stop(Duration::from_secs(3))
    );
    assert_eq!(
        first.unwrap(),
        scope.identity(),
        "CG7 close observes subtree empty"
    );
    assert_eq!(
        retry.unwrap(),
        scope.identity(),
        "CG8 concurrent close/retry converge"
    );
    assert!(
        scope.launch_permit().await.is_err(),
        "CG9 close is absorbing"
    );
    assert!(
        LocalProcess::spawn_scoped(
            &scope,
            helper(),
            &payload_command(),
            std::path::Path::new("/"),
            Duration::from_secs(1),
        )
        .await
        .is_err(),
        "B18 closed scope rejects the public scoped launcher"
    );
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(3), output.read_to_string(&mut rest))
        .await
        .unwrap()
        .unwrap();
    assert!(process.poll().await.unwrap().is_some());
    tokio::time::timeout(
        Duration::from_secs(3),
        nested_output.read_to_string(&mut rest),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(nested_process.poll().await.unwrap().is_some(), "CG18");
    assert!(
        neighbor_process.poll().await.unwrap().is_none(),
        "CG10 neighboring scope survives"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), neighbor_output.read_u8())
            .await
            .is_err()
    );

    // CG11 removal/recreate ABA: the old descriptor cannot target new members.
    std::fs::remove_dir(&nested_path).unwrap();
    let stale_launch = LinuxProcessScope::open(&path).unwrap();
    std::fs::remove_dir(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let replacement = LinuxProcessScope::open(&path).unwrap();
    assert_ne!(replacement.identity(), scope.identity());
    // B13 old path replaced before bootstrap join => no GO and no workload;
    // the other open descriptor is a deliberate stale-owner fault injection.
    assert!(
        LocalProcess::spawn_scoped(
            &stale_launch,
            helper(),
            &payload_command(),
            std::path::Path::new("/"),
            Duration::from_secs(5),
        )
        .await
        .is_err()
    );
    let (replacement_process, _replacement_output) = payload(&replacement).await;
    assert!(scope.stop(Duration::from_secs(1)).await.is_err());
    assert!(replacement_process.poll().await.unwrap().is_none());
    replacement.stop(Duration::from_secs(3)).await.unwrap();
    neighbor.stop(Duration::from_secs(3)).await.unwrap();
    std::fs::remove_dir(&path).unwrap();
    std::fs::remove_dir(&neighbor_path).unwrap();
    println!("CONTROL_CONTRACT_PASSED");
}

async fn bootstrap_contract(scope: &LinuxProcessScope, parent: &std::path::Path) {
    // B8 exact join + GO => workload starts IN the scope with physical nested
    // cwd, only explicit env (including a synthetic credential), exact binary
    // stdin/stdout and independent stderr. Neither helper protocol nor ambient
    // owner variables may leak into those business streams. No real credential
    // account, vendor authentication, or remote endpoint is qualified here.
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("nested 工作目录");
    std::fs::create_dir(&cwd).unwrap();
    // B17 loader constructors must also execute after membership, not merely
    // the shell's main program. The explicit Linux gate requires a C compiler;
    // an unavailable prerequisite fails rather than silently skipping this row.
    let loader = root.path().join("require-scope.so");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scoped_loader.c");
    assert!(
        Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&loader)
            .status()
            .await
            .unwrap()
            .success()
    );
    let mut command = MaterializedCommand::new([
        "/bin/bash",
        "-c",
        r#"
read -r membership < /proc/self/cgroup
test "$membership" = "$EXPECTED_SCOPE" || exit 31
test "$PWD" = "$EXPECTED_CWD" || exit 32
test "$SCOPE_CREDENTIAL" = 'fixture-value' || exit 33
test -z "${AWAKEN_CGROUP_TEST_MODE+x}" || exit 34
printf 'stderr-only' >&2
/bin/cat
"#,
    ]);
    command.stdio = awaken_provisioning_contract::Stdio::Piped;
    command.cwd = "/logical/path/must/not/be/reprojected".into();
    for (name, value) in [
        (
            "EXPECTED_SCOPE",
            format!(
                "0::/{}/owned",
                parent.strip_prefix("/sys/fs/cgroup").unwrap().display()
            ),
        ),
        ("EXPECTED_CWD", cwd.to_str().unwrap().to_owned()),
        ("LD_PRELOAD", loader.to_str().unwrap().to_owned()),
    ] {
        command.env.push(MaterializedEnvVar {
            name: name.into(),
            value: MaterializedEnvValue::Inline(value),
        });
    }
    command.env.push(MaterializedEnvVar {
        name: "SCOPE_CREDENTIAL".into(),
        value: MaterializedEnvValue::Secret("fixture-value".to_owned().into()),
    });
    let mut process =
        LocalProcess::spawn_scoped(scope, helper(), &command, &cwd, Duration::from_secs(5))
            .await
            .unwrap();
    let mut input = process.take_stdin().unwrap();
    let bytes = b"no newline\0\xff\nlast";
    input.write_all(bytes).await.unwrap();
    drop(input);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_end(&mut stdout)
        .await
        .unwrap();
    process
        .take_stderr()
        .unwrap()
        .read_to_end(&mut stderr)
        .await
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0), "B8");
    assert_eq!(
        stdout, bytes,
        "B8 no handshake or credential bytes in stdout"
    );
    assert_eq!(stderr, b"stderr-only", "B8 independent stderr");

    // B9 missing executable/cwd after GO => process failure, not launch-ready
    // success. B10 a helper which never connects => bounded launch timeout.
    for (program, physical_cwd) in [
        ("/awaken-does-not-exist", cwd.clone()),
        ("/bin/true", cwd.join("absent")),
    ] {
        let mut command = MaterializedCommand::new([program]);
        command.stdio = awaken_provisioning_contract::Stdio::Null;
        let process = LocalProcess::spawn_scoped(
            scope,
            helper(),
            &command,
            &physical_cwd,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_ne!(process.wait().await.unwrap().code, Some(0), "B9");
    }
    let marker = root.path().join("must-not-execute");
    let mut command = MaterializedCommand::new([
        "/bin/sh",
        "-c",
        "printf executed > \"$1\"",
        "scoped-test",
        marker.to_str().unwrap(),
    ]);
    command.stdio = awaken_provisioning_contract::Stdio::Null;
    assert_eq!(
        LocalProcess::spawn_scoped(
            scope,
            std::path::Path::new("/bin/true"),
            &command,
            &cwd,
            Duration::from_millis(50)
        )
        .await
        .err()
        .unwrap()
        .kind(),
        std::io::ErrorKind::TimedOut,
        "B10"
    );
    // B11 held admission + cancellation/timeout => no bootstrap; the permit
    // remains usable. B12 invalid helper/deadline rejects before admission.
    let permit = scope.launch_permit().await.unwrap();
    {
        let launch =
            LocalProcess::spawn_scoped(scope, helper(), &command, &cwd, Duration::from_secs(5));
        tokio::pin!(launch);
        tokio::select! {
            biased;
            _ = &mut launch => panic!("B11 launch passed held permit"),
            () = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        LocalProcess::spawn_scoped(scope, helper(), &command, &cwd, Duration::from_millis(10))
            .await
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    for (binary, timeout) in [
        (std::path::Path::new("relative"), Duration::from_secs(1)),
        (helper(), Duration::ZERO),
        (helper(), Duration::MAX),
    ] {
        assert_eq!(
            LocalProcess::spawn_scoped(scope, binary, &command, &cwd, timeout)
                .await
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidInput,
            "B12"
        );
    }
    drop(permit);
    assert!(
        !marker.exists(),
        "B10/B11 rejected launch cannot run the workload"
    );

    // B19 two queued launches + concurrent close => successful grants remain
    // inside the killed scope; losing admission returns error. No surviving
    // workload or new grant is permitted after the empty observation.
    let race_path = parent.join("launch-race");
    std::fs::create_dir(&race_path).unwrap();
    let race = LinuxProcessScope::open(&race_path).unwrap();
    let command = payload_command();
    let (first, second, stopped) = tokio::join!(
        LocalProcess::spawn_scoped(&race, helper(), &command, &cwd, Duration::from_secs(5)),
        LocalProcess::spawn_scoped(&race, helper(), &command, &cwd, Duration::from_secs(5)),
        race.stop(Duration::from_secs(5)),
    );
    assert_eq!(stopped.unwrap(), race.identity());
    for result in [first, second] {
        match result {
            Ok(mut process) => {
                let mut bytes = Vec::new();
                tokio::time::timeout(
                    Duration::from_secs(2),
                    process.take_stdout().unwrap().read_to_end(&mut bytes),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(process.poll().await.unwrap().is_some());
            }
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe,
                "B19 only closed admission is an acceptable losing launch"
            ),
        }
    }
    assert!(race.launch_permit().await.is_err());
    std::fs::remove_dir(&race_path).unwrap();
}

struct UnitCleanup(String);
impl Drop for UnitCleanup {
    fn drop(&mut self) {
        // Exact task-owned transient unit only; no wildcard/process-table kill.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", &self.0])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[tokio::test]
#[ignore = "requires Linux user systemd with delegated cgroup v2; explicit gate fails if absent"]
async fn linux_systemd_process_scope_contract() {
    // Cause/effect matrix: control={active launch, cancelled waiter, concurrent
    // stop, post-close launch, removed/recreated inode}; effects CG4..CG11 above.
    // CG12 main-only SIGKILL + KillMode=control-group => external systemd kills
    // the detached tree without running owner Drop. Lack of systemd/delegation,
    // early participant exit, missing readiness or retained stdout => FAIL.
    // This qualifies the OS adapter only, not Worker registration, Session
    // adoption, Native tool routing or hostile processes escaping delegation.
    for mode in ["control", "crash"] {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unit = format!(
            "awaken-cgroup-test-{}-{nonce:x}.service",
            std::process::id()
        );
        let _cleanup = UnitCleanup(unit.clone());
        let mut manager = Command::new("systemd-run")
            .args([
                "--user",
                "--quiet",
                "--pipe",
                "--wait",
                "--collect",
                "--property=Type=exec",
                "--property=KillMode=control-group",
                "--property=Delegate=yes",
                "--property=TimeoutStopSec=1s",
                "--property=SendSIGKILL=yes",
                "--property=RuntimeMaxSec=30s",
            ])
            .arg(format!("--unit={unit}"))
            .arg(format!("--setenv=AWAKEN_CGROUP_TEST_MODE={mode}"))
            .arg(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", PARTICIPANT, "--nocapture"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(manager.stdout.take().unwrap());
        let mut captured = String::new();
        if mode == "crash" {
            let mut ready = std::collections::BTreeSet::new();
            loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line))
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    !line.is_empty(),
                    "CG12 owner failed before ready: {captured}"
                );
                captured.push_str(&line);
                if ["OWNER_READY", "LEADER", "DETACHED"].contains(&line.trim()) {
                    ready.insert(line.trim().to_owned());
                }
                if ready.len() == 3 {
                    break;
                }
            }
            assert!(manager.try_wait().unwrap().is_none());
            assert!(
                Command::new("systemctl")
                    .args([
                        "--user",
                        "kill",
                        "--kill-whom=main",
                        "--signal=SIGKILL",
                        &unit
                    ])
                    .status()
                    .await
                    .unwrap()
                    .success()
            );
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            output.read_to_string(&mut captured),
        )
        .await
        .unwrap()
        .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), manager.wait())
            .await
            .unwrap()
            .unwrap();
        if mode == "control" {
            assert!(status.success(), "CG control failed: {captured}");
            assert!(captured.contains("CONTROL_CONTRACT_PASSED"));
        } else {
            assert!(!status.success(), "CG12 owner was killed, not graceful");
        }
    }
}
