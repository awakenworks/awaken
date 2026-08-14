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
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::task::JoinError;
use tokio::time::timeout;

/// The default listen address (the container-tier agent port, `CONTAINER_AGENT_PORT`).
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

/// Maximum time a freshly started bridge waits for its single host connection.
/// This deliberately exceeds the default warm-pool idle lifetime so an assigned
/// warm Pod is not reaped before its host has had a chance to dial it.
pub const DEFAULT_ACCEPT_TIMEOUT: Duration = Duration::from_secs(600);

#[cfg(not(test))]
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_millis(100);

/// A terminal bridge failure.  In particular, signal termination and failures in
/// either copy direction are explicit errors and can never become exit code zero.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error("failed to spawn ACP CLI: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("ACP host did not connect before the accept deadline")]
    AcceptTimeout,
    #[error("failed to accept ACP host connection: {0}")]
    Accept(#[source] std::io::Error),
    #[error("ACP CLI exited before the host connected (exit code {0})")]
    ExitedBeforeConnect(i32),
    #[error("ACP CLI terminated by signal")]
    SignalTermination,
    #[error("ACP bridge {direction} copy failed: {source}")]
    Copy {
        direction: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("ACP bridge {direction} task failed: {source}")]
    CopyTask {
        direction: &'static str,
        #[source]
        source: JoinError,
    },
    #[error("ACP bridge {0} copy did not drain before the deadline")]
    CopyDrainTimeout(&'static str),
    #[error("failed to wait for ACP CLI: {0}")]
    Wait(#[source] std::io::Error),
    #[error("ACP CLI did not exit after its stream closed")]
    ExitTimeout,
}

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
    accept_timeout: Duration,
}

impl AcpBridge {
    /// Bind the bridge's listener, returning it and the actual local address (so a
    /// caller/test that binds an ephemeral `:0` port learns where to dial).
    pub async fn bind(addr: &str) -> std::io::Result<(Self, SocketAddr)> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        Ok((
            Self {
                listener,
                accept_timeout: DEFAULT_ACCEPT_TIMEOUT,
            },
            local,
        ))
    }

    /// Spawn the CLI (process-as-container), accept ONE connection (one container == one
    /// agent process), and bridge the socket to the CLI's stdio until the CLI exits.
    /// Returns the CLI's exit code. `stderr` inherits so agent logs reach the pod log.
    pub async fn run(self, argv: &[String]) -> Result<i32, BridgeError> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(BridgeError::Spawn)?;
        let mut cli_stdin = child.stdin.take().expect("piped stdin");
        let mut cli_stdout = child.stdout.take().expect("piped stdout");

        enum Startup {
            Accepted(std::io::Result<(tokio::net::TcpStream, SocketAddr)>),
            Exited(std::io::Result<std::process::ExitStatus>),
        }

        let startup = timeout(self.accept_timeout, async {
            tokio::select! {
                accepted = self.listener.accept() => Startup::Accepted(accepted),
                status = child.wait() => Startup::Exited(status),
            }
        })
        .await;

        let (sock, _peer) = match startup {
            Err(_) => {
                stop_child(&mut child).await;
                return Err(BridgeError::AcceptTimeout);
            }
            Ok(Startup::Accepted(Ok(accepted))) => accepted,
            Ok(Startup::Accepted(Err(error))) => {
                stop_child(&mut child).await;
                return Err(BridgeError::Accept(error));
            }
            Ok(Startup::Exited(status)) => {
                let status = status.map_err(BridgeError::Wait)?;
                return match status.code() {
                    Some(code) => Err(BridgeError::ExitedBeforeConnect(code)),
                    None => Err(BridgeError::SignalTermination),
                };
            }
        };
        let (mut sock_rd, mut sock_wr) = sock.into_split();

        // socket -> CLI stdin, concurrently. Aborted once the CLI is gone (below), so a
        // still-open socket never keeps us alive past the agent.
        let mut to_cli = tokio::spawn(async move {
            tokio::io::copy(&mut sock_rd, &mut cli_stdin).await?;
            // Dropping `cli_stdin` here closes the CLI's stdin (EOF), so a client that
            // half-closes its write end lets the CLI finish its turn and exit.
            Ok::<_, std::io::Error>(())
        });

        // CLI stdout -> socket. This drives the lifetime: it returns when the CLI's
        // stdout hits EOF (the CLI exited AND its pipe drained), so the final reply is
        // always flushed to the host before we tear down.
        let mut from_cli = tokio::spawn(async move {
            tokio::io::copy(&mut cli_stdout, &mut sock_wr).await?;
            Ok::<_, std::io::Error>(())
        });

        // Whichever terminal event happens first determines the cleanup path.  A
        // closed stream gets a bounded grace period for the process to exit; a copy
        // failure kills and reaps it immediately.  Thus neither a dead peer nor a
        // misbehaving CLI can strand the bridge Pod indefinitely.
        let status = tokio::select! {
            status = child.wait() => {
                let status = status.map_err(BridgeError::Wait)?;
                let copied = await_copy_bounded(&mut from_cli, "CLI-to-socket").await;
                to_cli.abort();
                copied?;
                status
            }
            copied = &mut to_cli => {
                if let Err(error) = copy_result(copied, "socket-to-CLI") {
                    stop_child(&mut child).await;
                    from_cli.abort();
                    return Err(error);
                }
                let status = match wait_for_exit(&mut child).await {
                    Ok(status) => status,
                    Err(error) => {
                        from_cli.abort();
                        return Err(error);
                    }
                };
                await_copy_bounded(&mut from_cli, "CLI-to-socket").await?;
                status
            }
            copied = &mut from_cli => {
                if let Err(error) = copy_result(copied, "CLI-to-socket") {
                    stop_child(&mut child).await;
                    to_cli.abort();
                    return Err(error);
                }
                let status = match wait_for_exit(&mut child).await {
                    Ok(status) => status,
                    Err(error) => {
                        to_cli.abort();
                        return Err(error);
                    }
                };
                to_cli.abort();
                status
            }
        };

        status.code().ok_or(BridgeError::SignalTermination)
    }
}

fn copy_result(
    result: Result<Result<(), std::io::Error>, JoinError>,
    direction: &'static str,
) -> Result<(), BridgeError> {
    result
        .map_err(|source| BridgeError::CopyTask { direction, source })?
        .map_err(|source| BridgeError::Copy { direction, source })
}

async fn await_copy(
    task: &mut tokio::task::JoinHandle<Result<(), std::io::Error>>,
    direction: &'static str,
) -> Result<(), BridgeError> {
    copy_result(task.await, direction)
}

async fn await_copy_bounded(
    task: &mut tokio::task::JoinHandle<Result<(), std::io::Error>>,
    direction: &'static str,
) -> Result<(), BridgeError> {
    match timeout(CHILD_EXIT_TIMEOUT, await_copy(task, direction)).await {
        Ok(result) => result,
        Err(_) => {
            task.abort();
            Err(BridgeError::CopyDrainTimeout(direction))
        }
    }
}

async fn wait_for_exit(child: &mut Child) -> Result<std::process::ExitStatus, BridgeError> {
    match timeout(CHILD_EXIT_TIMEOUT, child.wait()).await {
        Ok(status) => status.map_err(BridgeError::Wait),
        Err(_) => {
            stop_child(child).await;
            Err(BridgeError::ExitTimeout)
        }
    }
}

async fn stop_child(child: &mut Child) {
    let _ = child.start_kill();
    let _ = timeout(CHILD_EXIT_TIMEOUT, child.wait()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn parse_acp_args_splits_listen_and_cli_argv() {
        // Cause/effect graph: C1=explicit listen, C2=CLI argv present;
        // E1=explicit/default address selected, E2=argv preserved, E3=fail closed.
        // Decision table: (C1,C2)=(T,T)->E1+E2; (F,T)->default+E2;
        // (*,F)->E3. These assertions cover all feasible rules.
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
        // Cause/effect graph: C1=host connects before deadline, C2=both copies
        // succeed, C3=CLI exits normally; E1=reply delivered, E2=exit code kept.
        // Decision-table rule B1=(T,T,T)->E1+E2. Failure rules B2-B4 are the
        // focused tests below. FMECA: a dropped final reply is observable here.
        let (bridge, addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        // A fake stdio "CLI": read one line from stdin, reply on stdout, exit — the
        // shape of an ACP CLI's newline-framed turn, without needing a real agent.
        let argv = echo_cli_command();
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

    #[tokio::test]
    async fn accept_timeout_fails_closed_and_reaps_the_cli() {
        // Cause/effect graph: C1=no host connection, C2=deadline expires;
        // E1=AcceptTimeout, E2=spawned CLI killed/reaped. Decision-table rule
        // B2=(C1=T,C2=T)->E1+E2. FMECA mitigation: bounded startup prevents an
        // orphaned warm Pod/process; `run` returning proves cleanup completed.
        let (mut bridge, _addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        bridge.accept_timeout = Duration::from_millis(20);
        let argv = sleeping_cli_command();

        let error = bridge.run(&argv).await.expect_err("must time out");
        assert!(matches!(error, BridgeError::AcceptTimeout));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_before_connect_is_never_reported_as_success() {
        // Cause/effect graph: C1=CLI terminates before dial, C2=no numeric exit code;
        // E1=SignalTermination (never zero). Decision-table rule B3=(T,T)->E1.
        // FMECA mitigation: orchestration cannot recycle a signal-killed sandbox as
        // a successful completed session.
        let (bridge, _addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        let argv = vec!["sh".into(), "-c".into(), "kill -TERM $$".into()];

        let error = bridge.run(&argv).await.expect_err("signal must fail");
        assert!(matches!(error, BridgeError::SignalTermination));
    }

    #[tokio::test]
    async fn normal_exit_before_connect_is_a_startup_failure() {
        // Cause/effect graph: C1=CLI exits before dial, C2=numeric status exists;
        // E1=ExitedBeforeConnect(status). Decision-table rule B4=(T,T)->E1.
        // FMECA mitigation: a bad image/argv cannot look like a usable bridge.
        let (bridge, _addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        let argv = immediate_exit_cli_command();

        let error = bridge.run(&argv).await.expect_err("early exit must fail");
        assert!(matches!(error, BridgeError::ExitedBeforeConnect(0)));
    }

    #[tokio::test]
    async fn spawn_and_copy_task_failures_preserve_their_terminal_class() {
        // Failure-class graph: C1=argv cannot spawn; C2=copy returns I/O error;
        // C3=copy task panics. Effects E1/E2/E3 are distinct Spawn/Copy/CopyTask
        // terminals. Decision rules BF1=C1=>E1, BF2=C2=>E2, BF3=C3=>E3.
        // FMECA: none may collapse to exit zero or an ambiguous EOF.
        let (bridge, _addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        let missing = vec![format!(
            "awaken-definitely-missing-bridge-cli-{}",
            std::process::id()
        )];
        assert!(matches!(
            bridge.run(&missing).await,
            Err(BridgeError::Spawn(_))
        ));

        let io_failure = copy_result(
            Ok(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "peer vanished",
            ))),
            "test-copy",
        );
        assert!(matches!(io_failure, Err(BridgeError::Copy { .. })), "BF2");

        let panicked = tokio::spawn(async move {
            panic!("injected copy task panic");
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        })
        .await;
        assert!(
            matches!(
                copy_result(panicked, "test-copy"),
                Err(BridgeError::CopyTask { .. })
            ),
            "BF3"
        );
    }

    #[tokio::test]
    async fn disconnected_client_cannot_leave_a_non_exiting_cli_orphaned() {
        // Lifetime graph: C1=host connects then closes; C2=CLI ignores stdin EOF;
        // E1=bounded ExitTimeout; E2=CLI is killed/reaped before return. Decision
        // rule BF4=C1+C2=>E1+E2. FMECA: a wedged image cannot consume one Pod
        // forever after the owning session channel disappears.
        let (bridge, addr) = AcpBridge::bind("127.0.0.1:0").await.expect("bind");
        let argv = sleeping_cli_command();
        let server = tokio::spawn(async move { bridge.run(&argv).await });
        let socket = TcpStream::connect(addr).await.expect("dial");
        drop(socket);

        let error = server.await.expect("join").expect_err("must be bounded");
        assert!(matches!(error, BridgeError::ExitTimeout), "BF4/E1");
    }

    #[tokio::test]
    async fn a_stalled_final_copy_has_a_bounded_drain() {
        // Drain graph: C1=CLI/output task is terminally stalled; C2=drain bound
        // expires; E1=CopyDrainTimeout and task cancellation. Rule BF5=C1+C2=>E1.
        // This helper-level injection deterministically covers TCP backpressure
        // that cannot be made portable with socket buffer timing.
        let mut task =
            tokio::spawn(async move { std::future::pending::<Result<(), std::io::Error>>().await });
        let error = await_copy_bounded(&mut task, "test-drain")
            .await
            .expect_err("drain must be bounded");
        assert!(matches!(error, BridgeError::CopyDrainTimeout("test-drain")));
        assert!(task.await.expect_err("BF5 task was aborted").is_cancelled());
    }

    #[cfg(windows)]
    fn echo_cli_command() -> Vec<String> {
        vec![
            "cmd.exe".into(),
            "/D".into(),
            "/V:ON".into(),
            "/C".into(),
            "set /p line= & echo reply:!line!".into(),
        ]
    }

    #[cfg(not(windows))]
    fn echo_cli_command() -> Vec<String> {
        vec![
            "sh".into(),
            "-c".into(),
            "read line; printf 'reply:%s\\n' \"$line\"".into(),
        ]
    }

    #[cfg(windows)]
    fn sleeping_cli_command() -> Vec<String> {
        vec![
            "cmd.exe".into(),
            "/C".into(),
            "ping -n 30 127.0.0.1 >NUL".into(),
        ]
    }

    #[cfg(not(windows))]
    fn sleeping_cli_command() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "sleep 30".into()]
    }

    #[cfg(windows)]
    fn immediate_exit_cli_command() -> Vec<String> {
        vec!["cmd.exe".into(), "/C".into(), "exit 0".into()]
    }

    #[cfg(not(windows))]
    fn immediate_exit_cli_command() -> Vec<String> {
        vec!["sh".into(), "-c".into(), "exit 0".into()]
    }
}
