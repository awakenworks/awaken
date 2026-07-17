//! The ACP `bridge` role: a stdio<->TCP bridge for a process-as-container agent CLI.
//!
//! The ACP CLIs (claude/codex/gemini/opencode) speak ACP over **stdio**, but the
//! container tier reaches the agent over **TCP** (a published port the host dials).
//! This bridge closes that gap: it is the sandbox image's ENTRYPOINT
//! (`awaken-sandbox acp --listen 0.0.0.0:8080`), the CLI argv arrives as the container
//! `Cmd` (`command_of(spec)`), and per pod it spawns the CLI and pipes its stdio to the
//! dialed socket. The CLI is spawned IMMEDIATELY (before the dial) so a warm-pool
//! container has its CLI booted and waiting on stdin by hand-out time.

use std::net::SocketAddr;
use std::process::Stdio;

use tokio::net::TcpListener;
use tokio::process::Command;

/// The default listen address (the container-tier agent port, `CONTAINER_AGENT_PORT`).
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

/// Split `acp` role args into the listen address and the CLI argv it spawns. The form
/// is `[--listen ADDR] <cli> [cli-args...]`; `--listen` defaults to [`DEFAULT_LISTEN`].
/// A missing CLI argv is an error (there is nothing to bridge to).
pub fn parse_acp_args(args: &[String]) -> Result<(String, Vec<String>), String> {
    let mut listen = DEFAULT_LISTEN.to_string();
    let mut rest = args;
    if let [flag, addr, tail @ ..] = args
        && flag == "--listen"
    {
        listen = addr.clone();
        rest = tail;
    }
    if rest.is_empty() {
        return Err("acp bridge requires a CLI argv to spawn (e.g. `... acp claude --acp`)".into());
    }
    Ok((listen, rest.to_vec()))
}

/// A bound ACP bridge listener.
pub struct AcpBridge {
    listener: TcpListener,
}

impl AcpBridge {
    /// Bind the bridge's listener, returning it and the actual local address (so a
    /// caller/test that binds an ephemeral `:0` port learns where to dial).
    pub async fn bind(addr: &str) -> std::io::Result<(Self, SocketAddr)> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        Ok((Self { listener }, local))
    }

    /// Spawn the CLI (process-as-container), accept ONE connection (one container == one
    /// agent process), and bridge the socket to the CLI's stdio until the CLI exits.
    /// Returns the CLI's exit code. `stderr` inherits so agent logs reach the pod log.
    pub async fn run(self, argv: &[String]) -> std::io::Result<i32> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut cli_stdin = child.stdin.take().expect("piped stdin");
        let mut cli_stdout = child.stdout.take().expect("piped stdout");

        let (sock, _peer) = self.listener.accept().await?;
        let (mut sock_rd, mut sock_wr) = sock.into_split();

        // socket -> CLI stdin, concurrently. Aborted once the CLI is gone (below), so a
        // still-open socket never keeps us alive past the agent.
        let to_cli = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut sock_rd, &mut cli_stdin).await;
            // Dropping `cli_stdin` here closes the CLI's stdin (EOF), so a client that
            // half-closes its write end lets the CLI finish its turn and exit.
        });

        // CLI stdout -> socket. This drives the lifetime: it returns when the CLI's
        // stdout hits EOF (the CLI exited AND its pipe drained), so the final reply is
        // always flushed to the host before we tear down.
        let _ = tokio::io::copy(&mut cli_stdout, &mut sock_wr).await;

        to_cli.abort();
        let status = child.wait().await?;
        Ok(status.code().unwrap_or(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn parse_acp_args_splits_listen_and_cli_argv() {
        let (listen, argv) = parse_acp_args(&[
            "--listen".into(),
            "0.0.0.0:9000".into(),
            "claude".into(),
            "--acp".into(),
        ])
        .expect("parses");
        assert_eq!(listen, "0.0.0.0:9000");
        assert_eq!(argv, vec!["claude".to_string(), "--acp".to_string()]);

        // No --listen: default port, everything is the CLI argv.
        let (listen, argv) = parse_acp_args(&["gemini".into()]).expect("parses");
        assert_eq!(listen, DEFAULT_LISTEN);
        assert_eq!(argv, vec!["gemini".to_string()]);

        // A missing CLI argv fails closed.
        assert!(parse_acp_args(&["--listen".into(), "x:1".into()]).is_err());
        assert!(parse_acp_args(&[]).is_err());
    }

    #[tokio::test]
    async fn the_bridge_pipes_a_dialed_socket_to_the_cli_stdio() {
        let (bridge, addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        // A fake stdio "CLI": read one line from stdin, reply on stdout, exit — the
        // shape of an ACP CLI's newline-framed turn, without needing a real agent.
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "read line; printf 'reply:%s\\n' \"$line\"".to_string(),
        ];
        let server = tokio::spawn(async move { bridge.run(&argv).await });

        let mut sock = TcpStream::connect(addr).await.expect("dial the bridge");
        sock.write_all(b"hello\n").await.expect("write prompt");
        sock.flush().await.ok();

        let mut got = String::new();
        let mut buf = [0u8; 64];
        loop {
            let n = sock.read(&mut buf).await.expect("read reply");
            if n == 0 {
                break;
            }
            got.push_str(&String::from_utf8_lossy(&buf[..n]));
            if got.contains("reply:hello") {
                break;
            }
        }

        let exit = server.await.expect("join").expect("bridge run");
        assert!(
            got.contains("reply:hello"),
            "the CLI's stdout reply reached the dialed socket: {got:?}"
        );
        assert_eq!(exit, 0, "the bridge returns the CLI's exit code");
    }
}
