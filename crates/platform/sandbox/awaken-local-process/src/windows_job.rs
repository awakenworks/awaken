//! One shared native mechanism owns launch, control and observed tree exit.
use tokio::process::Command;

pub(super) type OwnedChild = awaken_process_win32::OwnedProcessTree;

pub(super) fn spawn(command: &mut Command) -> std::io::Result<OwnedChild> {
    OwnedChild::spawn(command)
}

#[cfg(test)]
mod tests {
    use crate::{LocalProcess, ProcessHandle, Signal};
    use std::io::{Read, Write};
    use std::os::windows::process::CommandExt;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use winsafe::{HPROCESS, co, guard::CloseHandleGuard, prelude::*};

    const HELPER: &str = "windows_job::tests::tree_child";

    fn command(marker: &std::path::Path, mode: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--ignored"])
            .env("AWAKEN_TEST_TREE_MODE", mode)
            .env("AWAKEN_TEST_TREE_MARKER", marker)
            .kill_on_drop(true)
            .stdin(if mode == "leader_exit" {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        command
    }

    async fn descendant(
        marker: &std::path::Path,
        stderr: tokio::process::ChildStderr,
    ) -> CloseHandleGuard<HPROCESS> {
        // The owning leader commits this exact PID before its private pipe
        // notification. A generic arrival or a different process cannot pass.
        let mut reader = BufReader::new(stderr);
        let mut text = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut text))
            .await
            .expect("descendant readiness")
            .unwrap();
        assert!(read > 0, "leader closed before descendant readiness");
        assert_eq!(std::fs::read_to_string(marker).unwrap(), text.trim());
        let pid = text.trim().parse::<u32>().unwrap();
        // Retain the exact live incarnation before teardown; a recycled PID
        // can never make the exit assertion pass.
        let handle = HPROCESS::OpenProcess(co::PROCESS::SYNCHRONIZE, false, pid).unwrap();
        assert_eq!(
            handle.WaitForSingleObject(Some(0)).unwrap(),
            co::WAIT::TIMEOUT
        );
        handle
    }

    async fn exited(handle: CloseHandleGuard<HPROCESS>) {
        // Wait on the retained kernel object itself, without a private poll
        // loop or blocking the runtime responsible for process teardown.
        tokio::task::spawn_blocking(move || {
            assert_eq!(
                handle.WaitForSingleObject(Some(10_000)).unwrap(),
                co::WAIT::OBJECT_0
            );
        })
        .await
        .expect("owned descendant exit");
    }

    #[test]
    #[ignore = "subprocess fixture selected only by its exact parent test"]
    fn tree_child() {
        let Ok(mode) = std::env::var("AWAKEN_TEST_TREE_MODE") else {
            return;
        };
        let marker = std::path::PathBuf::from(std::env::var_os("AWAKEN_TEST_TREE_MARKER").unwrap());
        if mode == "owner" {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let _entered = runtime.enter();
            let mut leader = command(&marker, "leader");
            leader.stderr(Stdio::inherit());
            let process = LocalProcess::spawn(&mut leader).unwrap();
            runtime.block_on(process.wait()).unwrap();
        } else if mode == "leader" || mode == "leader_exit" {
            let mut leaf = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", HELPER, "--ignored"])
                .env("AWAKEN_TEST_TREE_MODE", "leaf")
                .env("AWAKEN_TEST_TREE_MARKER", &marker)
                .creation_flags(0x08000000)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::fs::write(marker, leaf.id().to_string()).unwrap();
            let mut stderr = std::io::stderr();
            writeln!(stderr, "{}", leaf.id()).unwrap();
            stderr.flush().unwrap();
            if mode == "leader_exit" {
                // The parent retains the exact live leaf handle before this
                // leader exits. Bypass Drop to exercise the kernel Job owner.
                let mut acknowledgement = [0u8; 1];
                std::io::stdin().read_exact(&mut acknowledgement).unwrap();
                assert_eq!(acknowledgement, [b'X']);
                std::process::exit(0);
            }
            leaf.wait().unwrap();
        } else if mode == "leaf" {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[tokio::test]
    async fn job_cancellation_and_drop_close_owned_descendants() {
        // J1 cancellation, J2 handle drop: both kill the descendant; direct-child
        // kill alone would leave the independently sleeping leaf alive.
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let marker = directory.path().join("leaf");
            let mut process = LocalProcess::spawn(&mut command(&marker, "leader")).unwrap();
            let leaf = descendant(&marker, process.take_stderr().unwrap()).await;
            if cancel {
                process.signal(Signal::Kill).await.unwrap();
                tokio::time::timeout(Duration::from_secs(10), process.wait())
                    .await
                    .unwrap()
                    .unwrap();
            }
            drop(process);
            exited(leaf).await;
        }
    }

    #[tokio::test]
    async fn natural_leader_exit_requests_owned_descendant_teardown() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaf");
        let mut process = LocalProcess::spawn(&mut command(&marker, "leader_exit")).unwrap();
        let leaf = descendant(&marker, process.take_stderr().unwrap()).await;
        process.take_stdin().unwrap().write_all(b"X").await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code, Some(0));
        assert!(!status.signaled);
        exited(leaf).await;
    }

    #[tokio::test]
    async fn kernel_job_closes_when_owner_is_terminated_without_drop() {
        // J3 kill the process owning the Job, bypassing Rust Drop. Windows must
        // close the Job handle and its entire retained descendant tree.
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("leaf");
        let mut owner = command(&marker, "owner").spawn().unwrap();
        let leaf = descendant(&marker, owner.stderr.take().unwrap()).await;
        owner.start_kill().unwrap();
        owner.wait().await.unwrap();
        exited(leaf).await;
    }
}
