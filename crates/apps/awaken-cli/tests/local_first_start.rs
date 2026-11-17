//! Process-level proof that a Local all-in-one initializes itself on its first
//! start, through the real `awaken` executable and with no migrate step.
//!
//! Causal decision table:
//! | Rule | data directory before `awaken all-in-one` | outcome |
//! | --- | --- | --- |
//! | P1 | missing | serves; marker and Session store created; the banner names the directory and the new Workspace |
//! | P2 | P1's directory after a clean stop | serves the same Workspace; no first-start banner |
//! | P3 | P1's directory deleted | serves a new Workspace; the banner names it |
//! | P4 | marker without a Session store | exits before listening with `session_storage_missing` and a copy-pasteable recovery command |

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn awaken() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_awaken")
        .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_awaken"))
        .map(PathBuf::from)
        .expect("Cargo must provide the awaken path when this integration test runs")
}

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    config: PathBuf,
    port: u16,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let data_dir = root.path().join("data");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = root.path().join("awaken.toml");
        std::fs::write(
            &config,
            format!(
                "data_dir = {:?}\nbind = \"127.0.0.1:{port}\"\nmode = \"local\"\nidentity_mode = \"no-login\"\nsandbox_tier = \"local\"\nacp_clis = []\n",
                data_dir.to_string_lossy()
            ),
        )
        .unwrap();
        Self {
            _root: root,
            home,
            data_dir,
            config,
            port,
        }
    }

    fn spawn(&self) -> (Child, mpsc::Receiver<String>) {
        let mut command = Command::new(awaken());
        command
            .args(["all-in-one", "--no-browser", "--config"])
            .arg(&self.config)
            .env_clear()
            .env("HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for name in ["PATH", "TMPDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("spawn awaken all-in-one");
        let stderr = child.stderr.take().unwrap();
        let (lines, received) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if lines.send(line).is_err() {
                    break;
                }
            }
        });
        (child, received)
    }

    fn marker(&self) -> Option<String> {
        std::fs::read_to_string(self.data_dir.join("platform-workspace-id")).ok()
    }
}

/// Collect stderr until the ready banner; the banner is printed only after the
/// public listener is bound, so it is the process's own readiness fact.
fn until_ready(lines: &mpsc::Receiver<String>) -> Vec<String> {
    let mut seen = Vec::new();
    // awaken-allow(wait): a cold debug AllInOne start opens every store and the
    // co-located Worker before its ready banner; the deadline only bounds a hung start.
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match lines.recv_timeout(remaining) {
            Ok(line) => {
                let ready = line.contains("Awaken is ready");
                seen.push(line);
                if ready {
                    return seen;
                }
            }
            Err(error) => panic!("awaken never became ready ({error}): {seen:#?}"),
        }
    }
}

fn http_status(port: u16, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response.lines().next().unwrap_or_default().to_owned()
}

fn stop(mut child: Child) {
    let pid = child.id().to_string();
    let signalled = Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .expect("send SIGTERM");
    assert!(signalled.success(), "SIGTERM {pid}");
    let (done, exited) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(child.wait());
    });
    // awaken-allow(wait): graceful drain after SIGTERM is bounded so a stuck
    // shutdown fails this test instead of leaking the child process.
    if exited.recv_timeout(Duration::from_secs(120)).is_err() {
        let _ = Command::new("kill").args(["-KILL", &pid]).status();
        panic!("awaken {pid} did not stop after SIGTERM");
    }
}

fn banner_workspace(lines: &[String], data_dir: &Path) -> Option<String> {
    let start = lines
        .iter()
        .position(|line| line.contains("New local installation (first start)"))?;
    let data = format!("Data       {}", data_dir.display());
    assert!(
        lines[start..].iter().any(|line| line.contains(&data)),
        "banner names the data directory: {lines:#?}"
    );
    lines[start..]
        .iter()
        .find_map(|line| line.trim().strip_prefix("Workspace  ").map(str::to_owned))
}

#[test]
fn local_all_in_one_initializes_itself_on_first_start_and_after_a_reset() {
    let fixture = Fixture::new();
    assert!(
        !fixture.data_dir.exists(),
        "P1 starts from a missing directory"
    );

    let (child, lines) = fixture.spawn();
    let first_lines = until_ready(&lines);
    assert!(
        http_status(fixture.port, "/v1/capabilities").starts_with("HTTP/1.1 "),
        "P1 serves"
    );
    stop(child);
    let first = fixture.marker().expect("P1 marker published");
    assert_eq!(
        banner_workspace(&first_lines, &fixture.data_dir).as_deref(),
        Some(first.as_str()),
        "P1 banner names the new Workspace"
    );
    assert!(
        fixture.data_dir.join("sessions.db").is_file(),
        "P1 Session store"
    );

    let (child, lines) = fixture.spawn();
    let second_lines = until_ready(&lines);
    assert!(
        http_status(fixture.port, "/v1/capabilities").starts_with("HTTP/1.1 "),
        "P2 serves"
    );
    stop(child);
    assert_eq!(
        banner_workspace(&second_lines, &fixture.data_dir),
        None,
        "P2 no banner"
    );
    assert_eq!(fixture.marker().as_deref(), Some(first.as_str()), "P2");

    std::fs::remove_dir_all(&fixture.data_dir).unwrap();
    let (child, lines) = fixture.spawn();
    let third_lines = until_ready(&lines);
    assert!(
        http_status(fixture.port, "/v1/capabilities").starts_with("HTTP/1.1 "),
        "P3 serves"
    );
    stop(child);
    let third = fixture.marker().expect("P3 marker published");
    assert_ne!(third, first, "P3 a reset is a new installation");
    assert_eq!(
        banner_workspace(&third_lines, &fixture.data_dir).as_deref(),
        Some(third.as_str()),
        "P3 banner"
    );
}

#[test]
fn local_partial_state_refuses_before_listening_with_a_recovery_command() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(&fixture.data_dir).unwrap();
    std::fs::write(
        fixture.data_dir.join("platform-workspace-id"),
        "workspace-partial",
    )
    .unwrap();
    let output = Command::new(awaken())
        .args(["all-in-one", "--no-browser", "--config"])
        .arg(&fixture.config)
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("run awaken all-in-one");
    assert!(!output.status.success(), "P4 refuses");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("session_storage_missing:"), "P4: {stderr}");
    let command = format!(
        "awaken database migrate --config {} --initialize-installation --initialization-reference local-recovery",
        fixture.config.display()
    );
    assert!(stderr.contains(&command), "P4 recovery command: {stderr}");
    assert!(
        !fixture.data_dir.join("sessions.db").exists(),
        "P4 no Session store"
    );

    let recovered = Command::new(awaken())
        .args(command.split(' ').skip(1))
        .env_clear()
        .env("HOME", &fixture.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("run the recovery command");
    assert!(
        recovered.status.success(),
        "P4 the printed command recovers: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        fixture.marker().as_deref(),
        Some("workspace-partial"),
        "P4 identity kept"
    );
}
