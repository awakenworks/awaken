// @formal evidence acp.process_cleanup_requires_observed_exit_and_retained_owner
//! T10-C02 on native Windows: the ACP leader process has exited while a real
//! descendant it launched is still alive.
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
//! | D2   | exited     | alive      | reap must not report the tree stopped (GAP)           |
//! Negative control (executed): a reaper that maps "leader exited, tree
//! unknown" to stopped is detected by the same oracle check.
#![cfg(windows)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use awaken_provisioning_contract::ProcessHandle as _;

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
    // The leader starts its descendant and exits at once, without waiting:
    // an orphaned, still-running descendant is the condition under test.
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
}

struct Tree {
    leader: awaken_local_process::LocalProcess,
    descendant: TcpStream,
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let leader = awaken_local_process::LocalProcess::spawn(&mut command).unwrap();
    let descendant = tokio::task::spawn_blocking(move || accept_ready(&listener, &token))
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(20), leader.wait())
        .await
        .expect("the leader exits on its own")
        .unwrap();
    assert_eq!(status.code, Some(0), "leader exited normally");
    Tree { leader, descendant }
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
async fn mutant_reap(process: &awaken_local_process::LocalProcess) -> ReapClaim {
    match process.poll().await {
        Ok(Some(_)) => Ok(false),
        Ok(None) => Err("leader still running".into()),
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
    let observed = tree.leader.poll().await.unwrap();
    assert_eq!(
        observed.map(|status| status.code),
        Some(Some(0)),
        "D1: exit observed"
    );
    assert_eq!(
        tree.leader.id(),
        identity,
        "D1: identity unchanged after exit"
    );
    assert!(
        descendant_alive(&mut tree.descendant),
        "D1: the descendant outlives its leader and remains observable"
    );

    // Negative control, executed: unknown -> stopped is detected.
    let claim = mutant_reap(&tree.leader).await;
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
    // Teardown: closing the retained connection ends the descendant.
    drop(tree.descendant);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "product gap: on Windows LocalProcess has no Job Object and Supervisor::reap treats leader exit as stopped while a descendant is alive"]
async fn t10_c02_reap_after_leader_exit_does_not_claim_stopped_while_descendant_lives() {
    let mut tree = leader_exited_with_live_descendant().await;
    let identity = tree.leader.id().to_owned();
    let claim = awaken_run_executor_acp::Supervisor::reap(&tree.leader, Duration::from_millis(200))
        .await
        .map_err(|error| error.to_string());
    let alive = descendant_alive(&mut tree.descendant);
    assert_eq!(
        tree.leader.id(),
        identity,
        "the original operation identity is retained"
    );
    check_not_forged_stopped(&claim, alive).unwrap_or_else(|violation| panic!("{violation}"));
    drop(tree.descendant);
}
