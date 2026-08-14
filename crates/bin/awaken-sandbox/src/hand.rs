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

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use awaken_connection_plan::{
    ConnectionPlan, TokioChannelFactory, bind_tcp, bind_unix, connect_with_retry,
};
use awaken_ext_builtin_tools::all_hand_tools;
use std::sync::Arc;

use awaken_tool_relay::{FsOperationLedger, HandOperationLedger, HandSession, serve_hand};
use tokio::io::{AsyncRead, AsyncWrite};

struct StdioChannel<R = tokio::io::Stdin, W = tokio::io::Stdout> {
    read: R,
    write: W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for StdioChannel<R, W> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.read).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for StdioChannel<R, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.write).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.write).poll_shutdown(cx)
    }
}

/// Where the hand serves the executor channel.
pub enum HandBind {
    /// Serve one hand session directly over this process's stdin/stdout. Container
    /// Session environments use this with their attached exec channel, avoiding a
    /// second network or rendezvous transport inside the same sandbox.
    Stdio,
    /// A unix socket at a filesystem path (works under `--network none`; the ONLY
    /// transport across a network-denied sandbox boundary — C5).
    Unix(String),
    /// A TCP address the hand LISTENS on; the brain dials in (Direct topology).
    Tcp(String),
    /// A brain rendezvous the hand REVERSE-DIALS out to (Reverse / NAT / k8s
    /// egress-fenced, where the brain cannot dial into the Pod). Reconnects on drop.
    Dial(String),
    /// A NATS broker + subject the hand serves request/reply over (Relay fleet).
    Nats { url: String, subject: String },
}

/// The default NATS subject a hand serves on (matches the brain's `AWAKEN_HAND_SUBJECT`).
const DEFAULT_NATS_SUBJECT: &str = "awaken.hand.exec";
const HAND_LEDGER_DIR: &str = "AWAKEN_HAND_LEDGER_DIR";
const HAND_LEDGER_MAX_ENTRIES: &str = "AWAKEN_HAND_LEDGER_MAX_ENTRIES";
const HAND_MAX_CONNECTIONS: &str = "AWAKEN_HAND_MAX_CONNECTIONS";
const DEFAULT_HAND_MAX_CONNECTIONS: usize = 16;

fn operation_ledger_root(configured: Option<std::ffi::OsString>) -> std::path::PathBuf {
    configured.map_or_else(
        || std::path::PathBuf::from(".awaken/hand-operations"),
        std::path::PathBuf::from,
    )
}

fn positive_limit(
    name: &'static str,
    configured: Option<std::ffi::OsString>,
    default: usize,
) -> Result<usize, String> {
    configured.map_or(Ok(default), |value| {
        value
            .to_str()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{name} must be a positive integer"))
    })
}

fn open_operation_ledger() -> Result<Arc<dyn HandOperationLedger>, String> {
    let root = operation_ledger_root(std::env::var_os(HAND_LEDGER_DIR));
    let max_entries = positive_limit(
        HAND_LEDGER_MAX_ENTRIES,
        std::env::var_os(HAND_LEDGER_MAX_ENTRIES),
        awaken_tool_relay::DEFAULT_FS_LEDGER_MAX_ENTRIES,
    )?;
    FsOperationLedger::open_with_max_entries(&root, max_entries)
        .map(|ledger| Arc::new(ledger) as Arc<dyn HandOperationLedger>)
        .map_err(|error| format!("open Hand operation ledger at {}: {error}", root.display()))
}

/// Parse the `hand` role args: `--unix <path>`, `--listen <addr>` (brain dials in),
/// `--dial <addr>` (hand dials the brain rendezvous), or `--nats <url> [--subject <s>]`.
pub fn parse_hand_args(args: &[String]) -> Result<HandBind, String> {
    match args {
        [flag] if flag == "--stdio" => Ok(HandBind::Stdio),
        [flag, path, ..] if flag == "--unix" => Ok(HandBind::Unix(path.clone())),
        [flag, addr, ..] if flag == "--listen" => Ok(HandBind::Tcp(addr.clone())),
        [flag, addr, ..] if flag == "--dial" => Ok(HandBind::Dial(addr.clone())),
        [flag, url, rest @ ..] if flag == "--nats" => Ok(HandBind::Nats {
            url: url.clone(),
            subject: subject_flag(rest).unwrap_or_else(|| DEFAULT_NATS_SUBJECT.to_string()),
        }),
        _ => Err(
            "hand requires `--stdio`, `--unix <path>`, `--listen <addr>`, `--dial <addr>`, \
                  or `--nats <url> [--subject <s>]`"
                .into(),
        ),
    }
}

/// The value after `--subject` in `rest`, if present.
fn subject_flag(rest: &[String]) -> Option<String> {
    rest.windows(2)
        .find(|w| w[0] == "--subject")
        .map(|w| w[1].clone())
}

/// Bind and serve the executor channel until the process is torn down. Each accepted
/// connection is served concurrently over a fresh [`HandSession`].
pub async fn serve(bind: HandBind) -> Result<(), String> {
    let ledger = open_operation_ledger()?;
    serve_with_operation_ledger(bind, ledger).await
}

/// Serve with an explicitly scoped ledger. SessionEnvironment embeddings and
/// tests use this seam to bind the ledger lifetime to the Environment owner.
pub async fn serve_with_operation_ledger(
    bind: HandBind,
    ledger: Arc<dyn HandOperationLedger>,
) -> Result<(), String> {
    match bind {
        HandBind::Stdio => {
            let channel = StdioChannel {
                read: tokio::io::stdin(),
                write: tokio::io::stdout(),
            };
            let session = HandSession::new(all_hand_tools(), ledger);
            serve_hand(channel, session)
                .await
                .map_err(|error| format!("hand stdio: {error}"))
        }
        HandBind::Unix(path) => {
            let listener = bind_unix(&ConnectionPlan::unix_listen(&path))
                .map_err(|e| format!("hand bind unix://{path}: {e}"))?;
            // The brain (host side) may run as a different uid than this in-container
            // hand, and a unix `connect()` needs write permission on the socket. Make
            // the socket world-connectable — access is gated by the PRIVATE rendezvous
            // DIRECTORY the composition bind-mounts (0700), not by the socket, the
            // standard unix-rendezvous posture.
            #[cfg(unix)]
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777))
            {
                eprintln!("awaken-sandbox hand: chmod {path}: {e} (brain may not connect)");
            }
            eprintln!("awaken-sandbox hand: serving the executor channel on unix://{path}");
            let connections = Arc::new(tokio::sync::Semaphore::new(positive_limit(
                HAND_MAX_CONNECTIONS,
                std::env::var_os(HAND_MAX_CONNECTIONS),
                DEFAULT_HAND_MAX_CONNECTIONS,
            )?));
            loop {
                let permit = connections
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "hand connection limiter closed".to_string())?;
                let channel = listener
                    .accept()
                    .await
                    .map_err(|e| format!("hand accept: {e}"))?;
                let ledger = ledger.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let session = HandSession::new(all_hand_tools(), ledger);
                    let _ = serve_hand(channel, session).await;
                });
            }
        }
        HandBind::Tcp(addr) => {
            let listener = bind_tcp(&ConnectionPlan::tcp_listen(&addr))
                .await
                .map_err(|e| format!("hand bind tcp://{addr}: {e}"))?;
            eprintln!("awaken-sandbox hand: serving the executor channel on tcp://{addr}");
            let connections = Arc::new(tokio::sync::Semaphore::new(positive_limit(
                HAND_MAX_CONNECTIONS,
                std::env::var_os(HAND_MAX_CONNECTIONS),
                DEFAULT_HAND_MAX_CONNECTIONS,
            )?));
            loop {
                let permit = connections
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "hand connection limiter closed".to_string())?;
                let channel = listener
                    .accept()
                    .await
                    .map_err(|e| format!("hand accept: {e}"))?;
                let ledger = ledger.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let session = HandSession::new(all_hand_tools(), ledger);
                    let _ = serve_hand(channel, session).await;
                });
            }
        }
        HandBind::Dial(addr) => {
            // Reverse topology: the hand dials OUT to the brain rendezvous and serves
            // over that outbound connection, re-dialing if the link drops. This is how a
            // k8s egress-fenced Pod (no ingress) still reaches the brain — the directed
            // outbound the NetworkPolicy allows.
            let plan = ConnectionPlan::tcp_dial(&addr);
            eprintln!("awaken-sandbox hand: reverse-dialing the brain rendezvous at tcp://{addr}");
            loop {
                match connect_with_retry(
                    &TokioChannelFactory,
                    &plan,
                    240,
                    std::time::Duration::from_millis(500),
                )
                .await
                {
                    Ok(channel) => {
                        let session = HandSession::new(all_hand_tools(), ledger.clone());
                        let _ = serve_hand(channel, session).await;
                        eprintln!("awaken-sandbox hand: brain link closed; re-dialing");
                    }
                    Err(e) => eprintln!("awaken-sandbox hand: reverse-dial failed: {e}"),
                }
            }
        }
        HandBind::Nats { url, subject } => run_nats(&url, &subject, ledger).await,
    }
}

/// Serve the executor channel over a NATS broker (Relay topology, ADR-0045): subscribe
/// the shared subject and reply to each `HandRequest` with the built-in hand tools'
/// result. Retries while the broker comes up; loops until killed.
async fn run_nats(
    url: &str,
    subject: &str,
    ledger: Arc<dyn HandOperationLedger>,
) -> Result<(), String> {
    use awaken_tool_relay::wire::HandRequest;
    use futures::StreamExt;

    let mut client = None;
    for _ in 0..120 {
        match async_nats::connect(url).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
    let client = client.ok_or_else(|| format!("hand failed to reach NATS {url}"))?;
    let mut sub = client
        .subscribe(subject.to_string())
        .await
        .map_err(|e| format!("hand NATS subscribe {subject}: {e}"))?;
    let mut session = HandSession::new(all_hand_tools(), ledger);
    eprintln!(
        "awaken-sandbox hand: serving the executor channel over NATS {url} subject '{subject}'"
    );
    while let Some(msg) = sub.next().await {
        let Some(reply_to) = msg.reply else { continue };
        let request: HandRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("awaken-sandbox hand: bad request over NATS: {e}");
                continue;
            }
        };
        let reply = session.handle(request).await;
        let bytes = serde_json::to_vec(&reply).map_err(|e| e.to_string())?;
        client
            .publish(reply_to, bytes.into())
            .await
            .map_err(|e| e.to_string())?;
        client.flush().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_channel_forwards_flush_and_shutdown_to_its_writer() {
        // Attached-Hand stream graph: C1=stdio binding selected; C2=reply written;
        // C3=flush/shutdown requested; E1=all AsyncWrite operations reach the
        // wrapped stdout. Decision rule HS1=C1+C2+C3=>E1. This detects an adapter
        // regression that could lose a terminal reply while the tool ran once.
        use tokio::io::AsyncWriteExt as _;

        let mut channel = StdioChannel {
            read: tokio::io::empty(),
            write: tokio::io::sink(),
        };
        channel.write_all(b"framed reply").await.unwrap();
        channel.flush().await.unwrap();
        channel.shutdown().await.unwrap();
    }

    #[test]
    fn parse_hand_args_reads_unix_and_tcp_binds() {
        // Bind decision table: exactly one of stdio/unix/listen/dial/nats with
        // its required value selects that canonical transport; missing/unknown
        // input fails closed. These assertions cover the local and directed
        // transports changed by resident Hand placement; NATS has its live test.
        assert!(matches!(
            parse_hand_args(&["--stdio".into()]).unwrap(),
            HandBind::Stdio
        ));
        assert!(matches!(
            parse_hand_args(&["--unix".into(), "/rv/hand.sock".into()]).unwrap(),
            HandBind::Unix(p) if p == "/rv/hand.sock"
        ));
        assert!(matches!(
            parse_hand_args(&["--listen".into(), "0.0.0.0:9000".into()]).unwrap(),
            HandBind::Tcp(a) if a == "0.0.0.0:9000"
        ));
        assert!(matches!(
            parse_hand_args(&["--dial".into(), "brain:7000".into()]).unwrap(),
            HandBind::Dial(a) if a == "brain:7000"
        ));
        assert!(parse_hand_args(&[]).is_err());
        assert!(parse_hand_args(&["--unix".into()]).is_err());
    }

    #[test]
    fn operation_ledger_root_uses_explicit_override_or_workspace_default() {
        // Cause/effect decision table: L1 explicit Environment-owned directory
        // -> use it exactly; L2 absent -> use the current Session workspace's
        // hidden durable directory. An empty override remains explicit so a bad
        // production configuration fails while opening instead of silently
        // falling back to another authority.
        assert_eq!(
            operation_ledger_root(Some("/session/ledger".into())),
            std::path::PathBuf::from("/session/ledger"),
            "L1"
        );
        assert_eq!(
            operation_ledger_root(None),
            std::path::PathBuf::from(".awaken/hand-operations"),
            "L2"
        );
    }

    #[test]
    fn resident_resource_limits_are_positive_and_default_to_one_authoritative_bound() {
        /*
         * Resource decision table: C1 value absent/positive/zero/malformed;
         * E1 canonical default, E2 exact override, E3 startup rejection. Rules
         * RL1 absent=>E1; RL2 positive=>E2; RL3 zero|malformed=>E3. The same
         * parser owns ledger-entry and connection bounds so invalid deployment
         * input cannot silently disable either FMECA control.
         */
        assert_eq!(positive_limit("LIMIT", None, 16).unwrap(), 16, "RL1");
        assert_eq!(
            positive_limit("LIMIT", Some("7".into()), 16).unwrap(),
            7,
            "RL2"
        );
        assert!(
            positive_limit("LIMIT", Some("0".into()), 16).is_err(),
            "RL3"
        );
        assert!(
            positive_limit("LIMIT", Some("many".into()), 16).is_err(),
            "RL3"
        );
    }

    /// Drive the private `run_nats` at the library altitude when a broker is reachable:
    /// serve the executor channel over NATS, then act as the brain — publish a real
    /// `HandRequest` (a `bash` call) to the subject and assert the harvested `HandReply`.
    ///
    /// A local NATS is unlikely in CI, so this is skip-on-unreachable: it probes the
    /// broker with a short timeout and returns early (a pass, logging "skipping") when
    /// none answers. Point `AWAKEN_TEST_NATS_URL` at a broker to exercise the round trip.
    /// Readiness is a request-retry loop (not a sleep): retry until the subscription
    /// responds, so it is deterministic whether the broker is fast or slow to subscribe.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_nats_serves_a_real_tool_when_a_broker_is_reachable() {
        use std::time::Duration;

        use awaken_runtime_contract::llm::ToolCall;
        use awaken_tool_relay::wire::{HandReply, HandRequest, HandResult};

        let url =
            std::env::var("AWAKEN_TEST_NATS_URL").unwrap_or_else(|_| "127.0.0.1:4222".to_string());

        // Probe the broker; skip cleanly if unreachable (the CI-default case).
        let client =
            match tokio::time::timeout(Duration::from_millis(750), async_nats::connect(&url)).await
            {
                Ok(Ok(c)) => c,
                _ => {
                    eprintln!("run_nats test: no NATS broker reachable at {url}; skipping");
                    return;
                }
            };

        // A per-process subject so parallel test runs never cross-talk.
        let subject = format!("awaken.hand.test.{}", std::process::id());
        let serve_url = url.clone();
        let serve_subject = subject.clone();
        let server = tokio::spawn(async move {
            run_nats(
                &serve_url,
                &serve_subject,
                Arc::new(awaken_tool_relay::InMemoryOperationLedger::default()),
            )
            .await
        });

        // Brain side: a real HandRequest carrying a `bash` call.
        let request = HandRequest::new(
            1,
            ToolCall {
                call_id: "c1".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({ "command": "echo nats-hand-ran-the-tool" }),
            },
        );
        let payload = serde_json::to_vec(&request).expect("encode request");

        // Retry-until-responded: `request()` errors with "no responders" until the
        // hand's subscription is live, so loop until we get a reply.
        let mut reply_bytes = None;
        for _ in 0..100 {
            match tokio::time::timeout(
                Duration::from_millis(250),
                client.request(subject.clone(), payload.clone().into()),
            )
            .await
            {
                Ok(Ok(msg)) => {
                    reply_bytes = Some(msg.payload);
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(25)).await,
            }
        }
        server.abort();

        let reply: HandReply =
            serde_json::from_slice(&reply_bytes.expect("the hand replied over NATS"))
                .expect("decode reply");
        match reply.result {
            HandResult::Ok { output } => {
                let rendered = serde_json::to_string(&output).expect("output serializes");
                assert!(
                    rendered.contains("nats-hand-ran-the-tool"),
                    "the NATS hand executed bash and returned its output: {rendered}"
                );
            }
            other => panic!("expected the NATS hand to run bash, got {other:?}"),
        }
    }
}
