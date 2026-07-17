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
        Some(role) => {
            eprintln!("awaken-sandbox: unknown role `{role}` (expected: acp)");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: awaken-sandbox <acp> [args...]");
            ExitCode::FAILURE
        }
    }
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
