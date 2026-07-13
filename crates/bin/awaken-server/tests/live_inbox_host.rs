//! Host-level live-inbox round trip: a message queued while a turn is in
//! flight is folded into that turn before it ends; a message the cancelled
//! turn never consumed carries over into the thread's next attempt.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor,
};
use awaken_server::SharedHost;
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

/// Echoes the user turns it was shown, but only after the test grants a
/// permit — so the test controls exactly when each inference step happens,
/// and can queue live-inbox messages while a step is provably in flight.
struct GatedEcho {
    started: mpsc::UnboundedSender<()>,
    permits: Mutex<mpsc::UnboundedReceiver<()>>,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedEcho {
    async fn infer(&self, r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let _ = self.started.send(());
        self.permits.lock().await.recv().await;
        let seen = r
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::User))
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|");
        Ok(ChatResponse {
            output: AssistantOutput::text(seen),
            usage: None,
            stop_reason: None,
        })
    }
}

struct Harness {
    host: Arc<SharedHost>,
    started: Mutex<mpsc::UnboundedReceiver<()>>,
    permits: mpsc::UnboundedSender<()>,
}

fn harness() -> Harness {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (permit_tx, permit_rx) = mpsc::unbounded_channel();
    let llm = GatedEcho {
        started: started_tx,
        permits: Mutex::new(permit_rx),
    };
    Harness {
        host: Arc::new(SharedHost::new(Arc::new(llm), "gated")),
        started: Mutex::new(started_rx),
        permits: permit_tx,
    }
}

impl Harness {
    async fn wait_step_started(&self) {
        timeout(Duration::from_secs(5), self.started.lock().await.recv())
            .await
            .expect("an inference step should start")
            .expect("started channel open");
    }
}

fn user(id: &str, text: &str) -> Message {
    Message::text(MessageId(id.to_string()), Role::User, text)
}

#[tokio::test]
async fn a_message_queued_mid_turn_reaches_the_same_turn() {
    let h = harness();
    let thread = "live-1";

    let run = {
        let host = h.host.clone();
        tokio::spawn(async move { host.run(None, thread, vec![user("u1", "First.")]).await })
    };

    // Step 1 is in flight (started, not yet permitted): the inbox is open.
    h.wait_step_started().await;
    let inbox = h
        .host
        .live_inbox(thread)
        .await
        .expect("an in-flight native turn opens a live inbox");
    let _ = inbox.offer(user("q1", "Queued follow-up."));

    // Let the turn proceed: step 1 answers, the natural-end boundary drains
    // the queue, and step 2 (also permitted) sees the injected turn.
    let _ = h.permits.send(());
    h.wait_step_started().await;
    let _ = h.permits.send(());

    let result = timeout(Duration::from_secs(5), run)
        .await
        .expect("turn finishes")
        .expect("task join")
        .expect("turn ok");
    let reply = result
        .new_messages
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(
        reply.text_content(),
        "First.|Queued follow-up.",
        "the queued message was folded into the same turn"
    );

    // The attempt closed its inbox on the way out.
    assert!(h.host.live_inbox(thread).await.is_none());

    // The injected turn is committed under the engine's run-scoped inbox id.
    let committed = h.host.committed_messages(thread).await;
    assert!(
        committed
            .iter()
            .any(|m| m.id.0.ends_with("-inbox-0") && m.text_content() == "Queued follow-up."),
        "committed transcript carries the re-identified injection"
    );
}

#[tokio::test]
async fn a_message_the_cancelled_turn_never_consumed_carries_over() {
    let h = harness();
    let thread = "live-2";

    let run = {
        let host = h.host.clone();
        tokio::spawn(async move { host.run(None, thread, vec![user("u1", "First.")]).await })
    };

    // Queue while step 1 is gated, then cancel the turn instead of permitting
    // it: the run ends cancelled without ever reaching a drain boundary.
    h.wait_step_started().await;
    let inbox = h.host.live_inbox(thread).await.expect("inbox open");
    let _ = inbox.offer(user("q1", "Survivor."));
    h.host
        .interrupt(thread)
        .await
        .expect("interrupt the gated turn");
    let _ = timeout(Duration::from_secs(5), run)
        .await
        .expect("cancelled turn returns");

    assert!(
        h.host.live_inbox(thread).await.is_none(),
        "the cancelled attempt closed its inbox"
    );

    // Next turn on the same thread: the leftover seeds the fresh inbox and is
    // consumed at the first natural-end boundary.
    let run = {
        let host = h.host.clone();
        tokio::spawn(async move { host.run(None, thread, vec![user("u2", "Second.")]).await })
    };
    // Step 1 of turn 2, then step 2 after the drain.
    h.wait_step_started().await;
    let _ = h.permits.send(());
    h.wait_step_started().await;
    let _ = h.permits.send(());

    let result = timeout(Duration::from_secs(5), run)
        .await
        .expect("turn finishes")
        .expect("task join")
        .expect("turn ok");
    let reply = result
        .new_messages
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert!(
        reply.text_content().contains("Survivor."),
        "the carried-over message reached the next attempt (reply: {})",
        reply.text_content()
    );
}
