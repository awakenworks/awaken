//! Real cgroup-v2/systemd substrate qualification. Explicit Linux gate only;
//! this is not evidence that production Worker/Session integration exists.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use awaken_local_process::LocalProcess;
use awaken_local_process::cgroup::{CgroupIdentity, LinuxProcessScope};
use awaken_provisioning_contract::ProcessHandle as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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

fn payload_command(identity: CgroupIdentity, path: &std::path::Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--ignored", "--exact", PARTICIPANT, "--nocapture"])
        .env("AWAKEN_CGROUP_TEST_MODE", "payload")
        .env("AWAKEN_CGROUP_TEST_PATH", path)
        .env("AWAKEN_CGROUP_TEST_DEVICE", identity.device.to_string())
        .env("AWAKEN_CGROUP_TEST_INODE", identity.inode.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

async fn payload(
    scope: &LinuxProcessScope,
    path: &std::path::Path,
) -> (LocalProcess, BufReader<tokio::process::ChildStdout>) {
    let permit = scope.launch_permit().await.unwrap();
    let mut command = payload_command(permit.identity(), path);
    let mut child = LocalProcess::spawn(&mut command).unwrap();
    let mut output = BufReader::new(child.take_stdout().unwrap());
    // Parent retains admission until the trusted bootstrap entered the exact
    // scope. Both acknowledgements come from the real exec'd Bash tree.
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
    drop(permit);
    (child, output)
}

#[tokio::test]
#[ignore = "trusted participant invoked by the explicit systemd gate"]
async fn linux_cgroup_participant() {
    let mode = std::env::var("AWAKEN_CGROUP_TEST_MODE").unwrap();
    if mode == "payload" {
        use std::os::unix::process::CommandExt;
        let scope = LinuxProcessScope::open(&PathBuf::from(
            std::env::var_os("AWAKEN_CGROUP_TEST_PATH").unwrap(),
        ))
        .unwrap();
        scope
            .enter_current_process(CgroupIdentity {
                device: std::env::var("AWAKEN_CGROUP_TEST_DEVICE")
                    .unwrap()
                    .parse()
                    .unwrap(),
                inode: std::env::var("AWAKEN_CGROUP_TEST_INODE")
                    .unwrap()
                    .parse()
                    .unwrap(),
            })
            .unwrap();
        // No workload code before membership. Ignore TERM and split sessions;
        // stdout EOF must come from scope teardown, not leader/group signaling.
        let error = std::process::Command::new("/bin/bash")
            .args([
                "-c",
                r#"
trap '' TERM
printf 'LEADER\n'
setsid /bin/bash -c 'trap "" TERM; printf "DETACHED\n"; sleep 45' &
wait
"#,
            ])
            .exec();
        panic!("exec payload: {error}");
    }
    let parent = own_cgroup();
    let path = parent.join("owned");
    let neighbor_path = parent.join("neighbor");
    std::fs::create_dir(&path).unwrap();
    std::fs::create_dir(&neighbor_path).unwrap();
    let scope = LinuxProcessScope::open(&path).unwrap();
    let neighbor = LinuxProcessScope::open(&neighbor_path).unwrap();
    if mode == "crash" {
        let permit = scope.launch_permit().await.unwrap();
        let mut command = payload_command(permit.identity(), &path);
        // All descendants inherit the OUTER observer's pipe directly. An owner
        // forwarding task would give false EOF when only the owner dies.
        command.stdout(Stdio::inherit());
        let process = LocalProcess::spawn(&mut command).unwrap();
        println!("OWNER_READY");
        // No close, Rust Drop or voluntary child exit is on the asserted path.
        let _retained = (permit, process, neighbor);
        std::future::pending::<()>().await;
        unreachable!();
    }
    assert_eq!(mode, "control");
    let (process, mut output) = payload(&scope, &path).await;
    // CG18 parent-scope kill includes nested cgroups, not just members listed
    // in the parent's cgroup.procs. The neighbor remains outside that subtree.
    let nested_path = path.join("nested");
    std::fs::create_dir(&nested_path).unwrap();
    let nested = LinuxProcessScope::open(&nested_path).unwrap();
    let (nested_process, mut nested_output) = payload(&nested, &nested_path).await;
    let (neighbor_process, mut neighbor_output) = payload(&neighbor, &neighbor_path).await;
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
    std::fs::remove_dir(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let replacement = LinuxProcessScope::open(&path).unwrap();
    assert_ne!(replacement.identity(), scope.identity());
    let (replacement_process, _replacement_output) = payload(&replacement, &path).await;
    assert!(scope.stop(Duration::from_secs(1)).await.is_err());
    assert!(replacement_process.poll().await.unwrap().is_none());
    replacement.stop(Duration::from_secs(3)).await.unwrap();
    neighbor.stop(Duration::from_secs(3)).await.unwrap();
    std::fs::remove_dir(&path).unwrap();
    std::fs::remove_dir(&neighbor_path).unwrap();
    println!("CONTROL_CONTRACT_PASSED");
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
