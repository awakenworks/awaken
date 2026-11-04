//! Host-level live-inbox round trip: a message queued while a Run is in
//! flight is folded into that Run before it ends; a best-effort message the
//! cancelled Run never consumed is discarded with that physical attempt.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_session_executor::ExecutorComponents;
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
    host: Arc<ExecutorComponents>,
    started: Mutex<mpsc::UnboundedReceiver<()>>,
    permits: mpsc::UnboundedSender<()>,
}

async fn harness() -> Harness {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (permit_tx, permit_rx) = mpsc::unbounded_channel();
    let llm = GatedEcho {
        started: started_tx,
        permits: Mutex::new(permit_rx),
    };
    let host = (awaken_scenario_host::build_unmounted_dependencies(Arc::new(llm), "gated", false)
        .await)
        .build()
        .expect("validated Host dependencies");
    Harness {
        host,
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

/// Cause/effect design: C0 the Host installs the canonical dispatch Session
/// Runtime and canonical Resources (including empty artifact capture); C1 a native Run is gated mid-inference with its inbox
/// open; C2 a follow-up is offered before the natural-end boundary; C3 both
/// inference Steps are released. Effects: E1 the same Run folds the follow-up
/// into its final reply; E2 the inbox closes; E3 the injection commits under a
/// Run-scoped inbox id. Decision rule L1=C0+C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn a_message_queued_mid_run_reaches_the_same_run() {
    // Causes: the fixtures below establish `a message queued mid run` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let h = harness().await;
    let thread = "live-1";

    let run = {
        let host = h.host.clone();
        tokio::spawn(async move {
            host.session_runs()
                .run(None, thread, vec![user("u1", "First.")])
                .await
        })
    };

    // Step 1 is in flight (started, not yet permitted): the inbox is open.
    h.wait_step_started().await;
    let inbox = h
        .host
        .live_inboxes()
        .live_inbox(thread)
        .await
        .expect("healthy registry")
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
    assert!(
        h.host
            .live_inboxes()
            .live_inbox(thread)
            .await
            .expect("healthy registry")
            .is_none()
    );

    // The injected message is committed under the engine's Run-scoped inbox id.
    let committed = h
        .host
        .thread_reads()
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

/// Cause/effect design: C0 the Host installs the canonical dispatch Session
/// Runtime; C1 a follow-up is queued while the first Run is gated; C2 that Run
/// is interrupted before a drain boundary with unknown model termination; C3 a
/// second Run is requested on the same Thread. Effects: E1 the first inbox
/// closes and rejects later offers; E2 its uncommitted entry is discarded;
/// E3 the physical fence rejects reentry without a second inference.
/// Decision rule L2=C0+C1+C2+C3=>E1+E2+E3.
/// Constraint: reliable cross-attempt delivery uses durable Session ingress,
/// never a parallel process-local carry-over queue. L1 covers consumption before
/// close; the active-generation test covers replacement by another live attempt.
#[tokio::test]
async fn a_message_the_cancelled_run_never_consumed_is_discarded() {
    // Causes: the fixtures below establish `a message the cancelled run` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let h = harness().await;
    let thread = "live-2";

    let run = {
        let host = h.host.clone();
        tokio::spawn(async move {
            host.session_runs()
                .run(None, thread, vec![user("u1", "First.")])
                .await
        })
    };

    // Queue while Step 1 is gated, then cancel the Run instead of permitting
    // it: the run ends cancelled without ever reaching a drain boundary.
    h.wait_step_started().await;
    let inbox = h
        .host
        .live_inboxes()
        .live_inbox(thread)
        .await
        .expect("healthy registry")
        .expect("inbox open");
    let _ = inbox.offer(user("q1", "Survivor."));
    h.host
        .session_runs()
        .interrupt(thread)
        .await
        .expect("interrupt the gated Run");
    let interrupted = timeout(Duration::from_secs(5), run)
        .await
        .expect("cancelled Run returns")
        .expect("cancelled Run task joins")
        .expect("cancelled Run has a committed terminal receipt");
    assert!(
        matches!(
            interrupted.state,
            awaken_agent_contract::agent::run::RunState::Ended(
                awaken_agent_contract::agent::run::EndCause::Cancelled
            )
        ),
        "direct Run commits logical cancellation: {:?}",
        interrupted.state
    );

    assert!(
        h.host
            .live_inboxes()
            .live_inbox(thread)
            .await
            .expect("healthy registry")
            .is_none(),
        "the cancelled attempt closed its inbox"
    );
    assert!(
        inbox.list().is_empty(),
        "L2/E2 close removed the uncommitted attempt-local entry"
    );
    assert!(
        matches!(
            inbox.offer(user("late", "Too late.")),
            awaken_runtime_contract::live_inbox::Offer::Closed
        ),
        "L2/E1 the retired inbox cannot accept another ownerless entry"
    );

    // Reentry cannot erase the unknown physical attempt. Assert the actual
    // rejection and absence of another inference, instead of waiting for an
    // unsafe new attempt or fabricating termination evidence in the fixture.
    let result = timeout(
        Duration::from_secs(5),
        h.host
            .session_runs()
            .run(None, thread, vec![user("u2", "Second.")]),
    )
    .await
    .expect("reentry reaches a terminal admission decision");
    let error = result
        .err()
        .expect("unknown physical attempt rejects reentry");
    assert!(
        error.to_string().contains("no termination evidence"),
        "rejection must come from the physical fence: {error}"
    );
    assert!(
        h.started.lock().await.try_recv().is_err(),
        "L2/E3 no second inference entered"
    );
    let committed = h
        .host
        .thread_reads()
        .committed_messages(thread)
        .await
        .expect("committed history remains readable");
    assert!(
        committed
            .iter()
            .all(|message| message.text_content() != "Survivor."),
        "L2/E2 an unconsumed best-effort entry never became durable truth"
    );
}
