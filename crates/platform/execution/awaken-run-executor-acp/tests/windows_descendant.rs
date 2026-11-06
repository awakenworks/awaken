// @formal evidence acp.process_cleanup_requires_observed_exit_and_retained_owner
//! T10-C02 on native Windows: the ACP leader process has exited while a real
//! descendant it launched is still alive before production polling requests
//! owned Job teardown. Observe leader exit on a retained kernel handle: calling
//! LocalProcess::wait/poll while arranging the fixture would change that state.
//!
//! Production units: `awaken_local_process::LocalProcess` (the ACP executor's
//! spawn/handle) and `awaken_run_executor_acp::Supervisor::reap` (the reap
//! ladder the executor's cleanup uses after a terminal frame). Liveness of the
//! descendant is observed over its own retained TCP connection (echo means
//! alive; EOF/reset means the OS terminated it), never by PID guessing.
//!
//! Decision table:
//! | rule | leader     | descendant | expected                                              |
//! | D1   | exited (0) | alive      | stable leader identity; exit observed; descendant echo |
//! | D2   | exited     | alive      | a leader-only stopped claim is rejected by the oracle |
//! | D3   | polled     | stopped    | production polling requests teardown; observe EOF     |
//! Negative control (executed): a reaper that maps "leader exited, tree
//! unknown" to stopped is detected by the same oracle check.
#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use awaken_provisioning_contract::ProcessHandle as _;
use tokio::io::AsyncWriteExt;
use winsafe::{HPROCESS, co, guard::CloseHandleGuard, prelude::*};

const ADDRESS: &str = "AWAKEN_ACP_DESCENDANT_ADDRESS";
const TOKEN: &str = "AWAKEN_ACP_DESCENDANT_TOKEN";

#[test]
#[ignore = "private child fixture launched only by the T10-C02 Windows tests"]
fn fixture_descendant_echoes_until_eof() {
    let (Ok(address), Ok(token)) = (std::env::var(ADDRESS), std::env::var(TOKEN)) else {
        return;
    };
    let mut stream =
        TcpStream::connect_timeout(&address.parse().unwrap(), Duration::from_secs(5)).unwrap();
    stream.write_all(token.as_bytes()).unwrap();
    let mut byte = [0];
    // No timer closes this connection: EOF is the parent's explicit teardown.
    while matches!(stream.read(&mut byte), Ok(1)) {
        if stream.write_all(&byte).is_err() {
            break;
        }
    }
}

#[test]
#[ignore = "private leader fixture launched only by the T10-C02 Windows tests"]
fn fixture_leader_spawns_descendant_then_exits() {
    if std::env::var(ADDRESS).is_err() {
        return;
    }
    // The parent retains this leader's kernel identity and authenticates the
    // descendant before releasing exit. It can then observe leader exit without
    // calling the production poll edge, which now tears down the owned Job.
    #[allow(clippy::zombie_processes)]
    std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "fixture_descendant_echoes_until_eof",
            "--quiet",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("leader launches its descendant");
    let mut acknowledgement = [0];
    std::io::stdin().read_exact(&mut acknowledgement).unwrap();
    assert_eq!(acknowledgement, [b'X']);
}

struct Tree {
    leader: awaken_local_process::LocalProcess,
    descendant: TcpStream,
    leader_exit: CloseHandleGuard<HPROCESS>,
}

fn token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{:032x}", nanos ^ u128::from(std::process::id()))
}

fn accept_ready(listener: &TcpListener, token: &str) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut connection = loop {
        match listener.accept() {
            Ok((connection, _)) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the actual descendant must connect"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("accept: {error}"),
        }
    };
    connection.set_nonblocking(false).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut seen = vec![0; token.len()];
    connection
        .read_exact(&mut seen)
        .expect("the descendant must publish readiness");
    assert_eq!(seen, token.as_bytes());
    connection
}

/// Observed liveness over the descendant's own connection.
fn descendant_alive(connection: &mut TcpStream) -> bool {
    connection
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    if connection.write_all(b"?").is_err() {
        return false;
    }
    let mut byte = [0];
    matches!(connection.read(&mut byte), Ok(1) if byte == *b"?")
}

fn assert_descendant_stopped(connection: &mut TcpStream) {
    connection
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut byte = [0];
    match connection.read(&mut byte) {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        observed => panic!("D3: owned descendant must close its connection: {observed:?}"),
    }
}

async fn leader_exited_with_live_descendant() -> Tree {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let token = token();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            "fixture_leader_spawns_descendant_then_exits",
            "--quiet",
        ])
        .env(ADDRESS, listener.local_addr().unwrap().to_string())
        .env(TOKEN, &token)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut leader = awaken_local_process::LocalProcess::spawn(&mut command).unwrap();
    let descendant = tokio::task::spawn_blocking(move || accept_ready(&listener, &token))
        .await
        .unwrap();
    // This PID came from our own launch, and the private stdin gate keeps that
    // exact incarnation alive until the kernel handle has been retained.
    let leader_exit = HPROCESS::OpenProcess(
        co::PROCESS::SYNCHRONIZE | co::PROCESS::QUERY_LIMITED_INFORMATION,
        false,
        leader.id().parse().unwrap(),
    )
    .unwrap();
    assert_eq!(
        leader_exit.WaitForSingleObject(Some(0)).unwrap(),
        co::WAIT::TIMEOUT,
        "the owned leader is still held at its exit gate"
    );
    leader.take_stdin().unwrap().write_all(b"X").await.unwrap();
    let leader_exit = tokio::task::spawn_blocking(move || {
        assert_eq!(
            leader_exit.WaitForSingleObject(Some(20_000)).unwrap(),
            co::WAIT::OBJECT_0,
            "the leader exits on its own"
        );
        assert_eq!(leader_exit.GetExitCodeProcess().unwrap(), 0);
        leader_exit
    })
    .await
    .unwrap();
    Tree {
        leader,
        descendant,
        leader_exit,
    }
}

/// A reaper's claim about the launched operation after cleanup.
type ReapClaim = Result<bool, String>;

/// T10-C02 oracle 0: `Ok(_)` from a reaper means the operation is stopped; it
/// may only be claimed when no descendant of that operation is observed alive.
fn check_not_forged_stopped(claim: &ReapClaim, descendant_alive: bool) -> Result<(), String> {
    match claim {
        Ok(escalated) if descendant_alive => Err(format!(
            "reap reported stopped (kill escalation: {escalated}) while a descendant is alive"
        )),
        _ => Ok(()),
    }
}

/// In-test mutant: maps "leader exit observed, tree unknown" to stopped.
fn mutant_reap(tree: &Tree) -> ReapClaim {
    // Deliberately observe only the leader, without triggering the real Job
    // teardown. The live descendant must refute this forged stopped claim.
    match tree.leader_exit.WaitForSingleObject(Some(0)) {
        Ok(co::WAIT::OBJECT_0) => Ok(false),
        Ok(_) => Err("leader still running".into()),
        Err(error) => Err(error.to_string()),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t10_c02_leader_exit_keeps_identity_and_does_not_hide_a_live_descendant() {
    let mut tree = leader_exited_with_live_descendant().await;
    let identity = tree.leader.id().to_owned();
    assert!(
        !identity.is_empty(),
        "D1: the operation has a stable identity"
    );
    assert!(
        descendant_alive(&mut tree.descendant),
        "D1: the descendant outlives its leader and remains observable"
    );

    // Negative control, executed: unknown -> stopped is detected.
    let claim = mutant_reap(&tree);
    assert_eq!(claim, Ok(false), "NC: the mutant claims a clean stop");
    let alive = descendant_alive(&mut tree.descendant);
    assert!(
        alive,
        "NC: the descendant is still alive after the mutant reap"
    );
    let violation = check_not_forged_stopped(&claim, alive)
        .expect_err("NC: mapping unknown to stopped must be detected");
    assert!(violation.contains("descendant is alive"), "NC: {violation}");
    assert_eq!(
        tree.leader.id(),
        identity,
        "D1: identity retained through cleanup"
    );
    // Only now invoke production polling: unlike the independent kernel
    // observation above, this edge requests owned descendant teardown.
    // A poll may request termination while the Job still has live members.
    // Only a completed owned-tree observation is allowed to expose status.
    let observed = tree.leader.poll().await.unwrap();
    if observed.is_some() {
        assert!(!descendant_alive(&mut tree.descendant));
    }
    let observed = tree.leader.wait().await.unwrap();
    assert_eq!(observed.code, Some(0), "D1: exit observed");
    assert_eq!(
        tree.leader.id(),
        identity,
        "D1: identity unchanged after exit"
    );
    assert_descendant_stopped(&mut tree.descendant);
    drop(tree.descendant);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t10_c02_reap_after_leader_exit_does_not_claim_stopped_while_descendant_lives() {
    let mut tree = leader_exited_with_live_descendant().await;
    let identity = tree.leader.id().to_owned();
    let claim = awaken_run_executor_acp::Supervisor::reap(&tree.leader, Duration::from_millis(200))
        .await
        .map_err(|error| error.to_string());
    let alive = descendant_alive(&mut tree.descendant);
    assert!(claim.is_ok(), "D3: real reaping must succeed: {claim:?}");
    assert!(!alive, "D3: real reaping must stop the owned descendant");
    assert_eq!(
        tree.leader.id(),
        identity,
        "the original operation identity is retained"
    );
    check_not_forged_stopped(&claim, alive).unwrap_or_else(|violation| panic!("{violation}"));
    drop(tree.descendant);
}
