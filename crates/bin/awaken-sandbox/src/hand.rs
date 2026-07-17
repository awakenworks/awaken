//! The `hand` role (ADR-0044/0045): serve the neutral tool-execution channel — the
//! brain runs its tool calls HERE, on the hand, in the sandbox. Two binds:
//!
//!   awaken-sandbox hand --unix <path>     # a unix socket in a bind-mount rendezvous —
//!                                         # the ONLY transport across a `--network none`
//!                                         # sandbox boundary (C5); the brain dials it
//!                                         # with `DialAddr::Unix`.
//!   awaken-sandbox hand --listen <addr>   # a TCP endpoint (cross-node / k8s directed).
//!
//! Each accepted brain connection gets a fresh [`HandSession`] over the executable hand
//! tools (read/write/edit/glob/grep/bash); [`serve_hand`] drives it until the peer hangs
//! up. FAT role (tools + executor + transport) — behind the `hand` feature.

use std::os::unix::fs::PermissionsExt;

use awaken_connection_plan::{ConnectionPlan, bind_tcp, bind_unix};
use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_tool_relay::{HandSession, serve_hand};

/// Where the hand serves the executor channel.
pub enum HandBind {
    /// A unix socket at a filesystem path (works under `--network none`).
    Unix(String),
    /// A TCP address (cross-node reachable).
    Tcp(String),
}

/// Parse the `hand` role args: `--unix <path>` or `--listen <addr>`.
pub fn parse_hand_args(args: &[String]) -> Result<HandBind, String> {
    match args {
        [flag, path, ..] if flag == "--unix" => Ok(HandBind::Unix(path.clone())),
        [flag, addr, ..] if flag == "--listen" => Ok(HandBind::Tcp(addr.clone())),
        _ => Err("hand requires `--unix <path>` or `--listen <addr>`".into()),
    }
}

/// Bind and serve the executor channel until the process is torn down. Each accepted
/// connection is served concurrently over a fresh [`HandSession`].
pub async fn serve(bind: HandBind) -> Result<(), String> {
    match bind {
        HandBind::Unix(path) => {
            let listener = bind_unix(&ConnectionPlan::unix_listen(&path))
                .map_err(|e| format!("hand bind unix://{path}: {e}"))?;
            // The brain (host side) may run as a different uid than this in-container
            // hand, and a unix `connect()` needs write permission on the socket. Make
            // the socket world-connectable — access is gated by the PRIVATE rendezvous
            // DIRECTORY the composition bind-mounts (0700), not by the socket, the
            // standard unix-rendezvous posture.
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777))
            {
                eprintln!("awaken-sandbox hand: chmod {path}: {e} (brain may not connect)");
            }
            eprintln!("awaken-sandbox hand: serving the executor channel on unix://{path}");
            loop {
                let channel = listener
                    .accept()
                    .await
                    .map_err(|e| format!("hand accept: {e}"))?;
                tokio::spawn(async move {
                    let session = HandSession::new(executable_hand_tools());
                    let _ = serve_hand(channel, session).await;
                });
            }
        }
        HandBind::Tcp(addr) => {
            let listener = bind_tcp(&ConnectionPlan::tcp_listen(&addr))
                .await
                .map_err(|e| format!("hand bind tcp://{addr}: {e}"))?;
            eprintln!("awaken-sandbox hand: serving the executor channel on tcp://{addr}");
            loop {
                let channel = listener
                    .accept()
                    .await
                    .map_err(|e| format!("hand accept: {e}"))?;
                tokio::spawn(async move {
                    let session = HandSession::new(executable_hand_tools());
                    let _ = serve_hand(channel, session).await;
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hand_args_reads_unix_and_tcp_binds() {
        assert!(matches!(
            parse_hand_args(&["--unix".into(), "/rv/hand.sock".into()]).unwrap(),
            HandBind::Unix(p) if p == "/rv/hand.sock"
        ));
        assert!(matches!(
            parse_hand_args(&["--listen".into(), "0.0.0.0:9000".into()]).unwrap(),
            HandBind::Tcp(a) if a == "0.0.0.0:9000"
        ));
        assert!(parse_hand_args(&[]).is_err());
        assert!(parse_hand_args(&["--unix".into()]).is_err());
    }
}
