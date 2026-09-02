//! `awaken-sandbox` — the execution-plane binary (opposite the control-plane
//! `awaken`). The first arg selects the role the pod runs: `acp`, `hand`, or an
//! internal filesystem effect used by the host's container adapter.
//!
//!   awaken-sandbox acp [--listen ADDR] <cli> [cli-args...]
//!       Bridge a dialed TCP socket to a process-as-container ACP CLI's stdio. This is
//!       the sandbox image ENTRYPOINT; the CLI argv is the container `Cmd`.
//!   awaken-sandbox hand <--unix PATH|--listen ADDR|--dial ADDR|--nats URL [SUBJECT]>
//!       Serve the neutral tool-execution endpoint (ADR-0044/0045). `--features hand`.
//!   awaken-sandbox git-credential --socket PATH <get|store|erase>
//!       One-shot Git credential helper over a Session-owned control service.
//!   awaken-sandbox control-forwarder --unix PATH --listen LOOPBACK_ADDR --ready PATH
//!       Pod-local, payload-opaque Unix/TCP channel forwarder.
//!   awaken-sandbox control-forwarder-ready --marker PATH
//!       Exec-readiness check for the current forwarder generation marker.
//!   awaken-sandbox repository-publish-noreplace <STAGE> <DESTINATION>
//!       Atomically publish one owned staged directory without replacing a name.
//!   awaken-sandbox read-tree-nofollow <ROOT>
//!       Snapshot regular files below one exact directory without following links.

mod control_forwarder;
mod git_credential;

use std::process::ExitCode;

use awaken_sandbox::bridge::{AcpBridge, parse_acp_args};
use awaken_sandbox_fs::{
    directory_identity_nofollow, publish_directory_noreplace, read_regular_tree_nofollow,
};

struct OwnedCheckDirectory(std::path::PathBuf);

impl OwnedCheckDirectory {
    fn create(purpose: &str) -> Result<Self, String> {
        let identity = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("read system clock: {error}"))?
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            ".awaken-{purpose}-check-{}-{identity}",
            std::process::id()
        ));
        std::fs::create_dir(&root)
            .map_err(|error| format!("create {purpose} check directory: {error}"))?;
        Ok(Self(root))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for OwnedCheckDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("acp") => run_acp(&args[1..]).await,
        Some("hand") => run_hand(&args[1..]).await,
        Some("git-credential") => run_git_credential(&args[1..]).await,
        Some("control-forwarder") => run_control_forwarder(&args[1..]).await,
        Some("control-forwarder-ready") => run_control_forwarder_ready(&args[1..]),
        Some("repository-publish-noreplace") => run_repository_publish_noreplace(&args[1..]),
        Some("read-tree-nofollow") => run_read_tree_nofollow(&args[1..]),
        Some(role) => {
            eprintln!(
                "awaken-sandbox: unknown role `{role}` (expected: acp | hand | git-credential | control-forwarder | control-forwarder-ready | repository-publish-noreplace | read-tree-nofollow)"
            );
            ExitCode::FAILURE
        }
        None => {
            eprintln!(
                "usage: awaken-sandbox <acp|hand|git-credential|control-forwarder|control-forwarder-ready|repository-publish-noreplace|read-tree-nofollow> [args...]"
            );
            ExitCode::FAILURE
        }
    }
}

fn run_control_forwarder_ready(args: &[String]) -> ExitCode {
    match control_forwarder::check_ready(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox control-forwarder-ready: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_git_credential(args: &[String]) -> ExitCode {
    match git_credential::run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox git-credential: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run_control_forwarder(args: &[String]) -> ExitCode {
    match control_forwarder::run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox control-forwarder: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_repository_publish_noreplace(args: &[String]) -> ExitCode {
    if args == ["--check"] {
        return match check_repository_publisher() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("awaken-sandbox repository-publish-noreplace: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let [stage, destination] = args else {
        eprintln!("usage: awaken-sandbox repository-publish-noreplace <STAGE> <DESTINATION>");
        return ExitCode::FAILURE;
    };
    match publish_directory_noreplace(
        std::path::Path::new(stage),
        std::path::Path::new(destination),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox repository-publish-noreplace: {error}");
            ExitCode::FAILURE
        }
    }
}

fn check_repository_publisher() -> Result<(), String> {
    let owned_root = OwnedCheckDirectory::create("repository-publisher")?;
    let root = owned_root.path();
    let stage = root.join("stage");
    let destination = root.join("destination");
    std::fs::create_dir(&stage).map_err(|error| format!("create check stage: {error}"))?;
    std::fs::create_dir(&destination)
        .map_err(|error| format!("create occupied check destination: {error}"))?;
    if publish_directory_noreplace(&stage, &destination).is_ok()
        || !stage.is_dir()
        || !destination.is_dir()
    {
        return Err("kernel no-replace collision check failed".into());
    }
    std::fs::remove_dir(&destination)
        .map_err(|error| format!("remove owned check destination: {error}"))?;
    publish_directory_noreplace(&stage, &destination).map_err(|error| error.to_string())?;
    if stage.exists() || !destination.is_dir() {
        return Err("kernel no-replace absent-destination check failed".into());
    }
    Ok(())
}

fn run_read_tree_nofollow(args: &[String]) -> ExitCode {
    if args == ["--check"] {
        return match check_tree_reader() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("awaken-sandbox read-tree-nofollow: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let [root] = args else {
        eprintln!("usage: awaken-sandbox read-tree-nofollow <ROOT>");
        return ExitCode::FAILURE;
    };
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    match write_tree_archive(std::path::Path::new(root), &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox read-tree-nofollow: {error}");
            ExitCode::FAILURE
        }
    }
}

fn write_tree_archive(
    root: &std::path::Path,
    output: &mut dyn std::io::Write,
) -> Result<(), String> {
    let identity = directory_identity_nofollow(root).map_err(|error| error.to_string())?;
    let files = read_regular_tree_nofollow(root, identity, std::path::Path::new(""))
        .map_err(|error| error.to_string())?;
    let mut archive = tar::Builder::new(output);
    for file in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(file.bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive
            .append_data(&mut header, file.relative_path, file.bytes.as_slice())
            .map_err(|error| error.to_string())?;
    }
    archive.finish().map_err(|error| error.to_string())?;
    archive
        .into_inner()
        .map_err(|error| error.to_string())?
        .flush()
        .map_err(|error| error.to_string())
}

fn check_tree_reader() -> Result<(), String> {
    // Cause/effect decision table: C1 exact directory identity; C2 nested
    // regular file; C3 symlink entry. R1 C1+C2+!C3 snapshots exact bytes;
    // R2 C1+C2+C3 fails closed instead of emitting a partial archive.
    let owned_root = OwnedCheckDirectory::create("tree-reader")?;
    let nested = owned_root.path().join("nested");
    std::fs::create_dir(&nested).map_err(|error| error.to_string())?;
    std::fs::write(nested.join("value"), b"exact").map_err(|error| error.to_string())?;
    let identity =
        directory_identity_nofollow(owned_root.path()).map_err(|error| error.to_string())?;
    let files = read_regular_tree_nofollow(owned_root.path(), identity, std::path::Path::new(""))
        .map_err(|error| error.to_string())?;
    if files.len() != 1
        || files[0].relative_path != std::path::Path::new("nested/value")
        || files[0].bytes != b"exact"
    {
        return Err("descriptor-relative tree snapshot differs from exact fixture".into());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("value", nested.join("alias"))
            .map_err(|error| error.to_string())?;
        if read_regular_tree_nofollow(owned_root.path(), identity, std::path::Path::new("")).is_ok()
        {
            return Err("descriptor-relative tree snapshot followed a symlink".into());
        }
    }
    Ok(())
}

#[cfg(feature = "hand")]
async fn run_hand(args: &[String]) -> ExitCode {
    use awaken_sandbox::hand::{parse_hand_args, serve};
    if args == ["--check"] {
        return ExitCode::SUCCESS;
    }
    let bind = match parse_hand_args(args) {
        Ok(bind) => bind,
        Err(msg) => {
            eprintln!("awaken-sandbox hand: {msg}");
            return ExitCode::FAILURE;
        }
    };
    match serve(bind).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("awaken-sandbox hand: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "hand"))]
async fn run_hand(_args: &[String]) -> ExitCode {
    eprintln!("awaken-sandbox: the `hand` role needs a build with `--features hand`");
    ExitCode::FAILURE
}

async fn run_acp(args: &[String]) -> ExitCode {
    let (listen, argv) = match parse_acp_args(args) {
        Ok(parsed) => parsed,
        Err(msg) => {
            eprintln!("awaken-sandbox acp: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let (bridge, local) = match AcpBridge::bind(&listen).await {
        Ok(bound) => bound,
        Err(e) => {
            eprintln!("awaken-sandbox acp: bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "awaken-sandbox acp: bridging {} <-> {:?} on {local}",
        argv.join(" "),
        argv.first()
    );
    match bridge.run(&argv).await {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("awaken-sandbox acp: bridge failed: {e}");
            ExitCode::FAILURE
        }
    }
}
