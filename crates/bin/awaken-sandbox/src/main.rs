//! `awaken-sandbox` — the execution-plane binary (opposite the control-plane
//! `awaken`). The first arg selects the role the pod runs. Slice 1 ships `acp`; `hand`
//! and `memoryd` land in later slices.
//!
//!   awaken-sandbox acp [--listen ADDR] <cli> [cli-args...]
//!       Bridge a dialed TCP socket to a process-as-container ACP CLI's stdio. This is
//!       the sandbox image ENTRYPOINT; the CLI argv is the container `Cmd`.

use std::process::ExitCode;

use awaken_sandbox::bridge::{AcpBridge, parse_acp_args};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("acp") => run_acp(&args[1..]).await,
        Some("hand") => run_hand(&args[1..]).await,
        Some(role) => {
            eprintln!("awaken-sandbox: unknown role `{role}` (expected: acp | hand)");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: awaken-sandbox <acp|hand> [args...]");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "hand")]
async fn run_hand(args: &[String]) -> ExitCode {
    use awaken_sandbox::hand::{parse_hand_args, serve};
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
