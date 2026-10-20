//! Real-kernel proof of the workload syscall boundary (Linux, bubblewrap).
//!
//! Every test is *differential*: one probe script runs on the host (no
//! boundary) and inside the sandbox, and a row asserts only a difference the
//! boundary itself causes. The host run proves the probe can observe the
//! allowed outcome, so a sandbox `EPERM` cannot be an artefact of the probe.
//!
//! Decision table (cause → effect):
//!
//! | Row | Launch | Probe | Host | Sandbox |
//! |---|---|---|---|---|
//! | S1 | Namespace, network None | `socket(AF_VSOCK)`, `socket(AF_ALG)` | not EPERM | EPERM |
//! | S2 | Namespace | `io_uring_setup(1, NULL)` | EFAULT | EPERM |
//! | S3 | Namespace | `unshare(CLONE_NEWUSER)` | not EPERM | EPERM |
//! | S4 | Namespace | `clone3(NULL, 0)` | EINVAL/EFAULT | ENOSYS (libc falls back) |
//! | S5 | Namespace | `keyctl(GET_KEYRING_ID, 0)` | not EPERM | EPERM |
//! | S6 | Namespace | thread start, `socketpair`, `socket(AF_UNIX)`, `socket(AF_INET)` | ok | ok (runtime rendezvous keeps working) |
//! | D1 | Namespace, network None vs Unrestricted | host resolver varlink socket visible | — | absent vs present |
//! | W1 | Workdir networkless shell vs unrestricted shell | connect to a host AF_UNIX listener | connects | EPERM vs connects |
//! | W2 | Workdir networkless shell | `/run` | — | empty tmpfs; S1–S5 hold; threads and `socketpair` ok |
#![cfg(target_os = "linux")]

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxReadiness as _;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};
use awaken_tool_contract::tool::RawTool;
use awaken_tool_contract::tool::ToolCall;

const EPERM: i64 = libc::EPERM as i64;
const ENOSYS: i64 = libc::ENOSYS as i64;
const RESOLVED_SOCKET: &str = "/run/systemd/resolve/io.systemd.Resolve";

/// Prints one JSON object of errno results (0 = success). argv[1] is the
/// syscall-number table, argv[2] an optional host AF_UNIX socket to connect to.
const PROBE: &str = r#"
import ctypes, errno, json, os, socket, sys, threading
nr = json.loads(sys.argv[1])
target = sys.argv[2] if len(sys.argv) > 2 else ""
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
def sc(name, *args):
    ctypes.set_errno(0)
    r = libc.syscall(ctypes.c_long(nr[name]), *[ctypes.c_long(a) for a in args])
    return 0 if r >= 0 else ctypes.get_errno()
def sock(family, kind=socket.SOCK_STREAM):
    try:
        socket.socket(family, kind).close()
        return 0
    except OSError as e:
        return e.errno
out = {
    "af_vsock": sock(40),
    "af_alg": sock(38, socket.SOCK_SEQPACKET),
    "af_unix": sock(socket.AF_UNIX),
    "af_inet": sock(socket.AF_INET),
    "io_uring_setup": sc("io_uring_setup", 1, 0),
    "unshare_user": sc("unshare", 0x10000000),
    "clone3": sc("clone3", 0, 0),
    "keyctl": sc("keyctl", 0, 0),
    "resolved_socket": os.path.exists("/run/systemd/resolve/io.systemd.Resolve"),
    "run_entries": sorted(os.listdir("/run")) if os.path.isdir("/run") else None,
}
t = threading.Thread(target=lambda: None); t.start(); t.join(); out["thread"] = 0
a, b = socket.socketpair(); a.close(); b.close(); out["socketpair"] = 0
if target:
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(target); s.close()
        out["connect_host_unix"] = 0
    except OSError as e:
        out["connect_host_unix"] = e.errno
print(json.dumps(out))
"#;

fn syscall_numbers() -> String {
    serde_json::json!({
        "io_uring_setup": libc::SYS_io_uring_setup,
        "unshare": libc::SYS_unshare,
        "clone3": libc::SYS_clone3,
        "keyctl": libc::SYS_keyctl,
    })
    .to_string()
}

async fn available(program: &str, args: &[&str]) -> bool {
    tokio::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Q1 provider's real launch profile and probe tools available -> run. Q2 any
/// prerequisite missing in developer mode -> explicit skip. Q3 missing in the
/// existing strict Namespace gate -> fail, never a passing early return. The
/// provider owns the bwrap/profile probe; this test owns only its probe tools.
async fn substrate() -> bool {
    let root = tempfile::tempdir().unwrap();
    let ready = NamespaceProvider::new(root.path())
        .probe_ready()
        .await
        .is_ok()
        && available("python3", &["-c", "import ctypes"]).await
        && available("bash", &["-c", "exit 0"]).await;
    assert!(
        ready || std::env::var_os("AWAKEN_TEST_REQUIRE_NAMESPACE_WORKSPACE").is_none(),
        "required syscall boundary substrate is unavailable: provider launch profile, python3 and bash"
    );
    ready
}

/// Q2/Q3 cause/effect table: no isolation launcher on the operator PATH;
/// optional developer run -> explicit skip with success; required Namespace
/// run -> failure naming the missing substrate. Both execute a real boundary
/// test in a separate process, so this process's PATH and parallel tests stay
/// unchanged. A required early-return mutant would turn Q3 into success.
#[tokio::test]
async fn required_boundary_never_converts_a_missing_substrate_to_success() {
    let empty_path = tempfile::tempdir().unwrap();
    for required in [false, true] {
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "namespace_workload_runs_under_the_syscall_profile",
                "--nocapture",
            ])
            .env("PATH", empty_path.path())
            .env_remove("AWAKEN_TEST_REQUIRE_NAMESPACE_WORKSPACE");
        if required {
            child.env("AWAKEN_TEST_REQUIRE_NAMESPACE_WORKSPACE", "1");
        }
        let output = child.output().await.unwrap();
        assert_eq!(output.status.success(), !required, "Q2/Q3 {required}");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            text.contains(if required {
                "required syscall boundary substrate is unavailable"
            } else {
                "skipping:"
            }),
            "Q2/Q3 {text}"
        );
    }
}

/// L2 the Worker PATH contains only a relative directory and the shell's cwd
/// contains a forged bwrap -> authoritative resolver refuses it; the retained
/// refusal reaches the Bash tool and no forged launcher runs. The participant
/// process isolates PATH from concurrent tests. The historical bare-name
/// fallback would execute the forged host program before shell failure.
#[tokio::test]
async fn workdir_launcher_refusal_cannot_fall_back_to_a_relative_path() {
    let directory = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "workdir_missing_trusted_launcher_participant",
            "--ignored",
            "--nocapture",
        ])
        .env("PATH", "bin")
        .env("AWAKEN_WORKDIR_LAUNCH_TEST", directory.path())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "L2 {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "isolated PATH participant of workdir_launcher_refusal_cannot_fall_back_to_a_relative_path"]
async fn workdir_missing_trusted_launcher_participant() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = std::path::PathBuf::from(
        std::env::var_os("AWAKEN_WORKDIR_LAUNCH_TEST").expect("participant directory"),
    );
    let sandbox = LocalProvider::new(&directory)
        .create_sandbox(&workdir_spec("relative-launcher", true))
        .await
        .unwrap();
    let bin = sandbox.workspace_path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let marker = directory.join("forged-launcher-ran");
    let forged = bin.join("bwrap");
    std::fs::write(
        &forged,
        format!("#!/bin/sh\nprintf attacked > '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tools = sandbox.rooted_tools();
    let bash = tools.iter().find(|tool| tool.id() == "bash").unwrap();
    let error = bash
        .invoke(ToolCall {
            call_id: "launcher-refusal".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command":"printf confined"}),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(&error, awaken_tool_contract::tool::ToolError::DefinitiveFailure(cause) if cause.contains("Worker's absolute PATH")),
        "L2 authoritative refusal: {error:?}"
    );
    assert!(!marker.exists(), "L2 no forged host program executed");
    pc::Sandbox::dispose_unattached(&sandbox).await.unwrap();
}

fn parse(stdout: &str) -> serde_json::Value {
    let line = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("probe printed no result: {stdout}"));
    serde_json::from_str(line).unwrap()
}

fn host_probe(target: Option<&str>) -> serde_json::Value {
    let mut command = std::process::Command::new("python3");
    command.args(["-c", PROBE, &syscall_numbers()]);
    if let Some(target) = target {
        command.arg(target);
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "host probe failed: {output:?}");
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn namespace_spec(scope: &str, network: pc::NetworkPolicy) -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: scope.into(),
        isolation: pc::IsolationClass::Namespace,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

async fn namespace_probe(network: pc::NetworkPolicy) -> serde_json::Value {
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider
        .create(&namespace_spec("syscall-boundary", network))
        .await
        .unwrap();
    let mut command = pc::Command::new([
        "sh",
        "-c",
        "python3 -c \"$1\" \"$2\" > /mnt/session/outputs/probe.json",
        "sh",
        PROBE,
        &syscall_numbers(),
    ]);
    command.stdio = pc::Stdio::Null;
    let process = sandbox.spawn(command).await.unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0), "probe exits 0");
    let artifacts = sandbox.artifacts().await.unwrap();
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("/probe.json"))
        .expect("probe result collected");
    let bytes = sandbox.read_artifact(&artifact.id).await.unwrap();
    sandbox.dispose_unattached().await.unwrap();
    parse(&String::from_utf8_lossy(&bytes))
}

fn workdir_spec(scope: &str, deny_tool_egress: bool) -> pc::SandboxSpec {
    pc::SandboxSpec {
        isolation: pc::IsolationClass::Workdir,
        outputs_path: "/outputs".into(),
        deny_tool_egress,
        ..namespace_spec(scope, pc::NetworkPolicy::Unrestricted)
    }
}

async fn workdir_bash_probe(deny_tool_egress: bool, target: &str) -> serde_json::Value {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path());
    let sandbox = provider
        .create_sandbox(&workdir_spec("syscall-workdir", deny_tool_egress))
        .await
        .unwrap();
    let tools: Vec<Arc<dyn RawTool>> = sandbox.rooted_tools();
    let bash = tools.iter().find(|tool| tool.id() == "bash").unwrap();
    let quote = |value: &str| format!("'{}'", value.replace('\'', r"'\''"));
    let command = format!(
        "python3 -c {} {} {}",
        quote(PROBE),
        quote(&syscall_numbers()),
        quote(target)
    );
    let output = bash
        .invoke(ToolCall {
            call_id: "probe".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({ "command": command }),
        })
        .await
        .unwrap();
    assert!(!output.is_error, "{}", output.text());
    parse(&output.text())
}

fn errno(result: &serde_json::Value, key: &str) -> i64 {
    result[key]
        .as_i64()
        .unwrap_or_else(|| panic!("{key} missing in {result}"))
}

/// Rows S1–S6 for one sandbox result against the host control.
fn assert_boundary_rows(
    host: &serde_json::Value,
    sandboxed: &serde_json::Value,
    unix_admitted: bool,
) {
    for (row, key, denied) in [
        ("S1", "af_vsock", EPERM),
        ("S1", "af_alg", EPERM),
        ("S2", "io_uring_setup", EPERM),
        ("S3", "unshare_user", EPERM),
        ("S4", "clone3", ENOSYS),
        ("S5", "keyctl", EPERM),
    ] {
        assert_ne!(
            errno(host, key),
            denied,
            "{row} host control for {key}: {host}"
        );
        assert_eq!(
            errno(sandboxed, key),
            denied,
            "{row} sandbox {key}: {sandboxed}"
        );
    }
    for key in ["thread", "socketpair", "af_inet"] {
        assert_eq!(errno(sandboxed, key), 0, "S6 {key}: {sandboxed}");
    }
    assert_eq!(errno(host, "af_unix"), 0);
    assert_eq!(
        errno(sandboxed, "af_unix") == 0,
        unix_admitted,
        "S6/W1 AF_UNIX creation: {sandboxed}"
    );
}

#[tokio::test]
async fn namespace_workload_runs_under_the_syscall_profile() {
    if !substrate().await {
        eprintln!("skipping: bwrap/userns, bash or python3 unavailable on this host");
        return;
    }
    let host = host_probe(None);
    let sandboxed = namespace_probe(pc::NetworkPolicy::None).await;
    assert_boundary_rows(&host, &sandboxed, true);
}

#[tokio::test]
async fn networkless_namespace_withholds_the_host_resolver_socket() {
    if !substrate().await {
        eprintln!("skipping: bwrap/userns, bash or python3 unavailable on this host");
        return;
    }
    if !std::path::Path::new(RESOLVED_SOCKET).exists() {
        eprintln!("skipping D1: host has no systemd-resolved varlink socket");
        return;
    }
    let denied = namespace_probe(pc::NetworkPolicy::None).await;
    assert_eq!(denied["resolved_socket"], false, "D1 None: {denied}");
    let open = namespace_probe(pc::NetworkPolicy::Unrestricted).await;
    assert_eq!(
        open["resolved_socket"], true,
        "D1 Unrestricted (control): {open}"
    );
}

#[tokio::test]
async fn networkless_workdir_shell_cannot_reach_host_daemon_sockets() {
    if !substrate().await {
        eprintln!("skipping: bwrap/userns, bash or python3 unavailable on this host");
        return;
    }
    // W1 causes: /tmp is masked by the shell; a host daemon socket must live
    // outside it, yet the sockaddr path must fit Linux SUN_LEN even when Cargo
    // uses a deeply nested target. /var/tmp is a short, host-visible control
    // path. Effects: unrestricted shell connects, networkless shell gets EPERM.
    let directory = tempfile::Builder::new()
        .prefix("awaken-daemon-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let path = directory.path().join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let target = path.to_str().unwrap();

    let host = host_probe(Some(target));
    assert_eq!(errno(&host, "connect_host_unix"), 0, "W1 host control");
    let open = workdir_bash_probe(false, target).await;
    assert_eq!(
        errno(&open, "connect_host_unix"),
        0,
        "W1 unrestricted shell: {open}"
    );

    let denied = workdir_bash_probe(true, target).await;
    assert_eq!(
        errno(&denied, "connect_host_unix"),
        EPERM,
        "W1 networkless shell: {denied}"
    );
    assert_boundary_rows(&host, &denied, false);
    assert_eq!(
        denied["resolved_socket"], false,
        "W2 host /run masked: {denied}"
    );
    assert_eq!(
        denied["run_entries"],
        serde_json::json!([]),
        "W2 empty /run: {denied}"
    );
    drop(listener);
}

/// L1 (launcher trust): the Session supplies the workload environment, `PATH`
/// included. Before the fix the launcher was looked up through that `PATH`, so
/// a Session could run its own `bwrap` — an unconfined host process — in place
/// of the isolation. The trusted launcher is resolved from the Worker's own
/// `PATH`: a forged `bwrap` first on the Session `PATH` never runs, and the
/// command still runs confined (it sees the sandbox root, not the host).
#[tokio::test]
async fn session_path_never_selects_the_isolation_launcher() {
    if !substrate().await {
        eprintln!("skipping: bwrap/userns, bash or python3 unavailable on this host");
        return;
    }
    use std::os::unix::fs::PermissionsExt as _;
    let forged = tempfile::Builder::new()
        .prefix("awaken-forged-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let marker = forged.path().join("forged-bwrap-ran");
    let launcher = forged.path().join("bwrap");
    std::fs::write(
        &launcher,
        format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();

    // L1 host control: a private sibling under HOME exists outside the
    // Session-declared runtime PATH root. The namespace may create /home as
    // an empty bind parent; its mere existence does not expose host HOME.
    let private_host = tempfile::Builder::new()
        .prefix("awaken-host-private-")
        .tempdir_in(std::path::PathBuf::from(
            std::env::var_os("HOME").expect("L1 operator HOME"),
        ))
        .unwrap();
    assert!(private_host.path().exists(), "L1 host control exists");
    let tmp = tempfile::tempdir().unwrap();
    let provider = NamespaceProvider::new(tmp.path());
    let sandbox = provider
        .create(&namespace_spec("launcher-trust", pc::NetworkPolicy::None))
        .await
        .unwrap();
    let mut command = pc::Command::new(vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "test ! -e \"$1\" && test -d /mnt/session/outputs && echo confined > /mnt/session/outputs/where".to_owned(),
        "sh".to_owned(),
        private_host.path().to_string_lossy().into_owned(),
    ]);
    command.stdio = pc::Stdio::Null;
    command.env.push(pc::EnvVar {
        name: "PATH".into(),
        value: pc::EnvValue::Inline {
            value: format!("{}:/usr/bin:/bin", forged.path().display()),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let launched = sandbox.spawn(command).await;
    assert!(
        !marker.exists(),
        "L1 forged launcher must never run, even if its missing identity report also rejects the spawn"
    );
    let process = launched.unwrap();
    assert_eq!(
        process.wait().await.unwrap().code,
        Some(0),
        "L1 ran confined"
    );
    assert!(
        !marker.exists(),
        "L1 the Session-supplied bwrap must never run"
    );
    let artifacts = sandbox.artifacts().await.unwrap();
    let where_ran = artifacts
        .iter()
        .find(|artifact| artifact.path.ends_with("/where"))
        .expect("confined command wrote its output");
    assert_eq!(
        sandbox.read_artifact(&where_ran.id).await.unwrap(),
        b"confined\n"
    );
    sandbox.dispose_unattached().await.unwrap();
}
