//! `awaken-sandbox` — the execution-plane binary (opposite the control-plane
//! `awaken`). The first arg selects one closed execution-plane role.
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

mod control_forwarder;
mod git_credential;

use std::process::ExitCode;

use awaken_sandbox::bridge::{AcpBridge, parse_acp_args};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("acp") => run_acp(&args[1..]).await,
        Some("hand") => run_hand(&args[1..]).await,
        Some("git-credential") => run_git_credential(&args[1..]).await,
        Some("control-forwarder") => run_control_forwarder(&args[1..]).await,
        Some("control-forwarder-ready") => run_control_forwarder_ready(&args[1..]),
        Some(role) => {
            eprintln!(
                "awaken-sandbox: unknown role `{role}` (expected: acp | hand | git-credential | control-forwarder | control-forwarder-ready)"
            );
            ExitCode::FAILURE
        }
        None => {
            eprintln!(
                "usage: awaken-sandbox <acp|hand|git-credential|control-forwarder|control-forwarder-ready> [args...]"
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
