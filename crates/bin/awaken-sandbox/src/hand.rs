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

use awaken_connection_plan::{
    ConnectionPlan, TokioChannelFactory, bind_tcp, bind_unix, connect_with_retry,
};
use awaken_ext_builtin_tools::executable_hand_tools;
use awaken_tool_relay::{HandSession, serve_hand};

/// Where the hand serves the executor channel.
pub enum HandBind {
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

/// Parse the `hand` role args: `--unix <path>`, `--listen <addr>` (brain dials in),
/// `--dial <addr>` (hand dials the brain rendezvous), or `--nats <url> [--subject <s>]`.
pub fn parse_hand_args(args: &[String]) -> Result<HandBind, String> {
    match args {
        [flag, path, ..] if flag == "--unix" => Ok(HandBind::Unix(path.clone())),
        [flag, addr, ..] if flag == "--listen" => Ok(HandBind::Tcp(addr.clone())),
        [flag, addr, ..] if flag == "--dial" => Ok(HandBind::Dial(addr.clone())),
        [flag, url, rest @ ..] if flag == "--nats" => Ok(HandBind::Nats {
            url: url.clone(),
            subject: subject_flag(rest).unwrap_or_else(|| DEFAULT_NATS_SUBJECT.to_string()),
        }),
        _ => Err(
            "hand requires `--unix <path>`, `--listen <addr>`, `--dial <addr>`, \
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
                        let session = HandSession::new(executable_hand_tools());
                        let _ = serve_hand(channel, session).await;
                        eprintln!("awaken-sandbox hand: brain link closed; re-dialing");
                    }
                    Err(e) => eprintln!("awaken-sandbox hand: reverse-dial failed: {e}"),
                }
            }
        }
        HandBind::Nats { url, subject } => run_nats(&url, &subject).await,
    }
}

/// Serve the executor channel over a NATS broker (Relay topology, ADR-0045): subscribe
/// the shared subject and reply to each `HandRequest` with the built-in hand tools'
/// result. Retries while the broker comes up; loops until killed.
async fn run_nats(url: &str, subject: &str) -> Result<(), String> {
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
    let mut session = HandSession::new(executable_hand_tools());
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
        assert!(matches!(
            parse_hand_args(&["--dial".into(), "brain:7000".into()]).unwrap(),
            HandBind::Dial(a) if a == "brain:7000"
        ));
        assert!(parse_hand_args(&[]).is_err());
        assert!(parse_hand_args(&["--unix".into()]).is_err());
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
        let server = tokio::spawn(async move { run_nats(&serve_url, &serve_subject).await });

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
