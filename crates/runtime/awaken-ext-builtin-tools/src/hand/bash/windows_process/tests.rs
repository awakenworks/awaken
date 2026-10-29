//! Real retained Windows identities witness control during wait and cancellation.
use super::Child;
use std::{
    io::{Read, Write},
    os::windows::process::CommandExt,
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use winsafe::{HPROCESS, co, guard::CloseHandleGuard, prelude::*};

const HELPER: &str = "hand::bash::windows_process::tests::process_tree_child";
const MODE: &str = "AWAKEN_BASH_TREE_TEST_MODE";
const MARKER: &str = "AWAKEN_BASH_TREE_TEST_MARKER";

fn command(marker: &std::path::Path, mode: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", HELPER, "--ignored"])
        .env(MODE, mode)
        .env(MARKER, marker)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn started(mode: &str) -> (tempfile::TempDir, Child, CloseHandleGuard<HPROCESS>) {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("leaf");
    let mut child = Child::spawn(command(&marker, mode)).unwrap();
    let mut reader = BufReader::new(child.stderr.take().unwrap());
    let mut pid = String::new();
    let bytes = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut pid))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes > 0);
    assert_eq!(std::fs::read_to_string(marker).unwrap(), pid.trim());
    let leaf = HPROCESS::OpenProcess(co::PROCESS::SYNCHRONIZE, false, pid.trim().parse().unwrap())
        .unwrap();
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::TIMEOUT
    );
    (directory, child, leaf)
}

#[test]
#[ignore = "private process fixture selected only by its exact native parent"]
fn process_tree_child() {
    let Ok(mode) = std::env::var(MODE) else {
        return;
    };
    if mode == "leaf" {
        std::thread::sleep(Duration::from_secs(60));
        return;
    }
    let marker = std::path::PathBuf::from(std::env::var_os(MARKER).unwrap());
    let mut leaf = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", HELPER, "--ignored"])
        .env(MODE, "leaf")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(marker, leaf.id().to_string()).unwrap();
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{}", leaf.id()).unwrap();
    stderr.flush().unwrap();
    if mode == "leader-exit" {
        let mut acknowledgement = [0];
        std::io::stdin().read_exact(&mut acknowledgement).unwrap();
        assert_eq!(acknowledgement, [b'X']);
        std::process::exit(0);
    }
    leaf.wait().unwrap();
}

#[tokio::test]
async fn cloned_control_terminates_the_original_tree_while_wait_owns_the_child() {
    // W1 wait has taken the box, W2 leader and exact descendant are alive,
    // W3 independent original control requests termination. W1+W2+W3 must
    // finish the active wait and its leaf, without terminating a foreign peer.
    // The old slot-only control acknowledges W3 without sending any signal.
    let (directory, mut child, leaf) = started("leader").await;
    let mut foreign = command(&directory.path().join("foreign"), "leaf")
        .spawn()
        .unwrap();
    let foreign_handle =
        HPROCESS::OpenProcess(co::PROCESS::SYNCHRONIZE, false, foreign.id().unwrap()).unwrap();
    let control = child.process_tree();
    let mut waiting = Box::pin(child.wait());
    tokio::select! {
        biased;
        result = &mut waiting => panic!("W2 live leader returned early: {result:?}"),
        () = std::future::ready(()) => (),
    }
    assert!(
        control.slot().is_none(),
        "W1 actual active wait owns the box"
    );
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::TIMEOUT
    );
    control.start_kill().unwrap();
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .expect("W3 cloned control must remain effective during wait")
        .unwrap();
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::OBJECT_0,
        "wait return is physical owned-tree completion"
    );
    assert_eq!(
        foreign_handle.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::TIMEOUT,
        "foreign peer is outside original ownership"
    );
    foreign.start_kill().unwrap();
    foreign.wait().await.unwrap();
}

#[tokio::test]
async fn cancelled_wait_retains_shutdown_control_and_joins_original_quiescence() {
    // W4 cancellation drops the wait-owned box while a control clone remains.
    // Its retained handle must neither suppress cleanup nor let shutdown claim
    // success from an empty Option without observing the original Job's exit.
    let (_directory, mut child, leaf) = started("leader").await;
    let control = child.process_tree();
    let mut waiting = Box::pin(child.wait());
    tokio::select! {
        biased;
        result = &mut waiting => panic!("live leader returned early: {result:?}"),
        () = std::future::ready(()) => (),
    }
    assert!(control.slot().is_none());
    drop(waiting);
    tokio::time::timeout(Duration::from_secs(10), child.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::OBJECT_0
    );
    // Retain and exercise the same control beyond shutdown, never a PID reopen.
    control.start_kill().unwrap();
}

#[tokio::test]
async fn natural_leader_exit_returns_only_after_its_retained_leaf_has_exited() {
    let (_directory, mut child, leaf) = started("leader-exit").await;
    let control = child.process_tree();
    let pid = control.slot().as_ref().unwrap().id().unwrap();
    let leader = HPROCESS::OpenProcess(co::PROCESS::SYNCHRONIZE, false, pid).unwrap();
    child.stdin.as_mut().unwrap().write_all(b"X").await.unwrap();
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            leader.WaitForSingleObject(Some(10_000)).unwrap(),
            co::WAIT::OBJECT_0
        )
    })
    .await
    .unwrap();
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::TIMEOUT,
        "no production polling arranged the fixture"
    );
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.code(), Some(0));
    assert_eq!(
        leaf.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::OBJECT_0,
        "terminal status requires original owned-tree completion"
    );
}

#[tokio::test]
async fn owner_drop_terminates_its_tree_even_when_a_control_clone_survives() {
    let (_directory, child, leaf) = started("leader").await;
    let control = child.process_tree();
    drop(child);
    // The original slot still owns the box through the retained clone. The
    // dropped Bash owner must nevertheless send termination immediately.
    assert!(control.slot().is_some());
    tokio::task::spawn_blocking(move || {
        assert_eq!(
            leaf.WaitForSingleObject(Some(10_000)).unwrap(),
            co::WAIT::OBJECT_0
        );
    })
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        control.control.wait_for_quiescence(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(control.control.is_quiescent().unwrap());
}

#[tokio::test]
async fn cancelled_session_discard_retains_the_cleanup_owner_until_confirmation() {
    use super::super::{BashSession, ProcessGroupGuard, discard_session};
    use std::sync::{Arc, atomic::AtomicBool};

    // Exercise the real caller as well as the child adapter. A cancellation
    // must not clear the Option which owns unconfirmed cleanup. A fully ready
    // cleanup may clear it; require an actual pending witness in this run.
    for _ in 0..8 {
        let (_directory, mut child, _leaf) = started("leader").await;
        let group = ProcessGroupGuard::new(child.process_tree());
        let mut session = Some(BashSession {
            stdin: child.stdin.take().unwrap(),
            stdout: child.stdout.take().unwrap(),
            stderr: None,
            child,
            invalidated: Arc::new(AtomicBool::new(false)),
            process_group: group,
        });
        let mut discarding = Box::pin(discard_session(&mut session));
        let pending = tokio::select! {
            biased;
            result = &mut discarding => { result.unwrap(); false },
            () = std::future::ready(()) => true,
        };
        drop(discarding);
        if pending {
            let retained = session
                .as_ref()
                .expect("pending cleanup keeps its Session owner");
            assert!(
                retained
                    .invalidated
                    .load(std::sync::atomic::Ordering::SeqCst)
            );
            tokio::time::timeout(Duration::from_secs(10), discard_session(&mut session))
                .await
                .unwrap()
                .unwrap();
            assert!(session.is_none(), "confirmed cleanup releases the owner");
            return;
        }
        assert!(session.is_none());
    }
    panic!("native cleanup cancellation witness was not reached");
}
