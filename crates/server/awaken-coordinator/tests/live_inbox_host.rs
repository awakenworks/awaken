//! Host-level live-inbox round trip: a message queued while a Run is in
//! flight is folded into that Run before it ends; a message the cancelled
//! Run never consumed carries over into the Thread's next attempt.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_coordinator::SharedHost;
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use tokio::sync::{Mutex, mpsc};
use tokio::time::timeout;

/// Echoes the user inputs it was shown, but only after the test grants a
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
            .filter(|m| matches!(m.role, Role::User))
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

/// Cause/effect design: C1 a native Run is gated mid-inference with its inbox
/// open; C2 a follow-up is offered before the natural-end boundary; C3 both
/// inference Steps are released. Effects: E1 the same Run folds the follow-up
/// into its final reply; E2 the inbox closes; E3 the injection commits under a
/// Run-scoped inbox id. Decision rule L1=C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn a_message_queued_mid_run_reaches_the_same_run() {
    // Causes: the fixtures below establish `a message queued mid run` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
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
        .expect("an in-flight native Run opens a live inbox");
    let _ = inbox.offer(user("q1", "Queued follow-up."));

    // Let the Run proceed: Step 1 answers, the natural-end boundary drains
    // the queue, and Step 2 (also permitted) sees the injected message.
    let _ = h.permits.send(());
    h.wait_step_started().await;
    let _ = h.permits.send(());

    let result = timeout(Duration::from_secs(5), run)
        .await
        .expect("Run finishes")
        .expect("task join")
        .expect("Run ok");
    let reply = result
        .new_messages
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .expect("assistant reply");
    assert_eq!(
        reply.text_content(),
        "First.|Queued follow-up.",
        "the queued message was folded into the same Run"
    );

    // The attempt closed its inbox on the way out.
    assert!(h.host.live_inbox(thread).await.is_none());

    // The injected message is committed under the engine's Run-scoped inbox id.
    let committed = h
        .host
        .committed_messages(thread)
        .await
        .expect("committed history remains readable");
    assert!(
        committed
            .iter()
            .any(|m| m.id.0.ends_with("-inbox-0") && m.text_content() == "Queued follow-up."),
        "committed transcript carries the re-identified injection"
    );
}

/// Cause/effect design: C1 a follow-up is queued while the first Run is gated;
/// C2 that Run is interrupted before a drain boundary; C3 a second Run starts on
/// the same Thread and reaches its first natural boundary. Effects: E1 the first
/// inbox closes without consuming the message; E2 the survivor appears in the
/// second Run's reply. Decision rule C1+C2+C3=>E1+E2; consumption before cancel
/// is covered by the preceding test.
#[tokio::test]
async fn a_message_the_cancelled_run_never_consumed_carries_over() {
    // Causes: the fixtures below establish `a message the cancelled run` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let h = harness();
    let thread = "live-2";

    let run = {
        let host = h.host.clone();
        tokio::spawn(async move { host.run(None, thread, vec![user("u1", "First.")]).await })
    };

    // Queue while Step 1 is gated, then cancel the Run instead of permitting
    // it: the run ends cancelled without ever reaching a drain boundary.
    h.wait_step_started().await;
    let inbox = h.host.live_inbox(thread).await.expect("inbox open");
    let _ = inbox.offer(user("q1", "Survivor."));
    h.host
        .interrupt(thread)
        .await
        .expect("interrupt the gated Run");
    let _ = timeout(Duration::from_secs(5), run)
        .await
        .expect("cancelled Run returns");

    assert!(
        h.host.live_inbox(thread).await.is_none(),
        "the cancelled attempt closed its inbox"
    );

    // Next Run on the same Thread: the leftover seeds the fresh inbox and is
    // consumed at the first natural-end boundary.
    let run = {
        let host = h.host.clone();
        tokio::spawn(async move { host.run(None, thread, vec![user("u2", "Second.")]).await })
    };
    // Step 1 of Run 2, then Step 2 after the drain.
    h.wait_step_started().await;
    let _ = h.permits.send(());
    h.wait_step_started().await;
    let _ = h.permits.send(());

    let result = timeout(Duration::from_secs(5), run)
        .await
        .expect("Run finishes")
        .expect("task join")
        .expect("Run ok");
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
