//! Hand-server entry points (ADR-0044/0045): serve the neutral executor
//! channel to a brain over a TCP channel (Direct/Reverse) or a NATS relay.
//! Extracted from `lib.rs` to hold it under the file-length limit; the public
//! `run_hand_server*` fns are re-exported at the crate root, so callers are
//! unchanged.

/// Run this binary as a HAND over NATS (ADR-0045 Relay): connect to the broker,
/// subscribe the shared subject, and reply to each `HandRequest` with the built-in
/// hand tools' result (G33). Loops until killed.
pub async fn run_hand_server_nats(
    url: &str,
    subject: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use awaken_tool_relay::{HandSession, wire::HandRequest};
    use futures::StreamExt;

    // Retry while the broker pod comes up.
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
    let mut sub = client.subscribe(subject.to_string()).await?;
    let mut session = HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
    eprintln!("awaken hand: serving the executor channel over NATS {url} subject '{subject}'");
    while let Some(msg) = sub.next().await {
        let Some(reply_to) = msg.reply else { continue };
        let request: HandRequest = match serde_json::from_slice(&msg.payload) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("awaken hand: bad request over NATS: {e}");
                continue;
            }
        };
        let reply = session.handle(request).await;
        let bytes = serde_json::to_vec(&reply)?;
        client.publish(reply_to, bytes.into()).await?;
        client.flush().await?;
    }
    Ok(())
}

/// Run this binary as a HAND (ADR-0044/0045): serve the neutral executor channel —
/// the built-in hand tools, and nothing else (G33) — to a brain. Two topologies:
///   - listen (`AWAKEN_HAND_LISTEN`): bind and accept brains that dial in (Direct).
///   - dial   (`AWAKEN_HAND_DIAL`):   dial the brain's rendezvous and serve over
///     that outbound connection (Reverse / NAT). Reconnects if the link drops.
///
/// Loops until killed.
pub async fn run_hand_server(addr: &str, dial: bool) -> Result<(), Box<dyn std::error::Error>> {
    use awaken_tool_relay::{HandSession, serve_hand};

    if dial {
        let factory = awaken_connection_plan::TokioChannelFactory;
        let plan = awaken_connection_plan::ConnectionPlan::tcp_dial(addr);
        eprintln!("awaken hand: reverse-dialing the brain rendezvous at tcp://{addr}");
        loop {
            match awaken_connection_plan::connect_with_retry(
                &factory,
                &plan,
                240,
                std::time::Duration::from_millis(500),
            )
            .await
            {
                Ok(channel) => {
                    let session =
                        HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
                    // Serve this brain until the link drops, then re-dial.
                    let _ = serve_hand(channel, session).await;
                    eprintln!("awaken hand: brain link closed; re-dialing");
                }
                Err(e) => eprintln!("awaken hand: reverse-dial failed: {e}"),
            }
        }
    }

    let plan = awaken_connection_plan::ConnectionPlan::tcp_listen(addr);
    let listener = awaken_connection_plan::bind_tcp(&plan)
        .await
        .map_err(|e| format!("hand failed to bind {addr}: {e}"))?;
    eprintln!("awaken hand: serving the executor channel on tcp://{addr}");
    loop {
        match listener.accept().await {
            Ok(channel) => {
                let session = HandSession::new(awaken_ext_builtin_tools::executable_hand_tools());
                tokio::spawn(async move {
                    let _ = serve_hand(channel, session).await;
                });
            }
            Err(e) => eprintln!("awaken hand: accept error: {e}"),
        }
    }
}
