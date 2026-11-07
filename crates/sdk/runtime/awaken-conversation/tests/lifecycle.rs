use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_conversation::ConversationRuns;
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime::{ExecutableAgentSnapshot, ModelBinding, Runtime, RuntimeRunContext};
use awaken_runtime_contract::resolved::CatalogFingerprint;
use awaken_runtime_contract::resume::{ResumeCommand, ResumeResult};
use awaken_runtime_contract::runs::Runs;
use awaken_runtime_contract::snapshot::ExecutableAgentSnapshotId;
use awaken_store_inmem::MemoryCommitCoordinator;
use tokio::sync::Semaphore;

const DEADLINE: Duration = Duration::from_secs(10);

struct GatedModel {
    calls: AtomicUsize,
    entered: Semaphore,
    release: Semaphore,
}

#[async_trait::async_trait]
impl LlmExecutor for GatedModel {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        let text = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .unwrap()
            .text_content();
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("reply to {text}")),
            usage: None,
            stop_reason: None,
        })
    }
}

fn fixture() -> (
    Arc<ConversationRuns>,
    Arc<MemoryCommitCoordinator>,
    Arc<GatedModel>,
    Arc<Runtime>,
) {
    let clock = awaken_time::system_clock();
    let store = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let model = Arc::new(GatedModel {
        calls: AtomicUsize::new(0),
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let runtime = Arc::new(
        Runtime::new()
            .with_clock(clock.clone())
            .with_llm(model.clone()),
    );
    let snapshot = ExecutableAgentSnapshot::builder("selected")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .build();
    let runs = ConversationRuns::new(
        runtime.clone(),
        snapshot,
        RuntimeRunContext::new().with_store(store.clone()),
        clock,
    )
    .unwrap();
    (Arc::new(runs), store, model, runtime)
}

fn input(text: &str) -> Vec<Message> {
    vec![Message::text(Id(format!("input-{text}")), Role::User, text)]
}

async fn entered(model: &GatedModel, count: u32) {
    tokio::time::timeout(DEADLINE, model.entered.acquire_many(count))
        .await
        .expect("model was not entered before the deadline")
        .unwrap()
        .forget();
}

#[tokio::test]
async fn concurrent_deliveries_share_one_committed_run_without_blocking_other_threads() {
    let (runs, store, model, _) = fixture();
    let first = tokio::spawn({
        let runs = runs.clone();
        async move { runs.run("same-key", "first", None, input("first")).await }
    });
    entered(&model, 1).await;

    // Poll the duplicate while the first request holds its Thread. A replay
    // must wait for the committed result; an independent Thread can run now.
    let duplicate = runs.run("same-key", "first", None, input("first"));
    tokio::pin!(duplicate);
    tokio::select! {
        biased;
        result = &mut duplicate => panic!("duplicate completed before the original: {result:?}"),
        () = std::future::ready(()) => {},
    }
    let other = tokio::spawn({
        let runs = runs.clone();
        async move { runs.run("same-key", "other", None, input("other")).await }
    });
    entered(&model, 1).await;
    model.release.add_permits(2);

    let (first, replay, other) =
        tokio::time::timeout(DEADLINE, async { tokio::join!(first, duplicate, other) })
            .await
            .expect("both Threads and the duplicate must settle");
    let first = first.unwrap().unwrap();
    let replay = replay.unwrap();
    let other = other.unwrap().unwrap();
    assert_eq!(first.run_id(), replay.run_id());
    assert_eq!(first.new_messages, replay.new_messages);
    assert_ne!(first.run_id(), other.run_id());
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    for (thread, outcome) in [("first", first), ("other", other)] {
        let record = store.latest_run(&ThreadId(thread.into())).unwrap();
        assert_eq!(Some(&record.id), outcome.run_id());
        assert_eq!(record.state, RunState::Ended(EndCause::NaturalEnd));
        let history = runs.history(thread).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].text_content(), thread);
        assert_eq!(history[1].text_content(), format!("reply to {thread}"));
    }
}

#[tokio::test]
async fn stop_targets_the_active_run_and_replay_retains_its_cancelled_outcome() {
    let (runs, store, model, runtime) = fixture();
    let running = tokio::spawn({
        let runs = runs.clone();
        async move { runs.run("start", "stopped", None, input("stopped")).await }
    });
    entered(&model, 1).await;
    let thread = ThreadId("stopped".into());
    let admitted = store.latest_run(&thread).unwrap();
    assert_eq!(admitted.state, RunState::Running);
    // An invalid resume cannot replace the live registration using its Run id
    // and a different Thread. The original attempt must still receive Stop.
    let unrelated_resume = ResumeCommand {
        correlation_id: "not-a-ticket".into(),
        run_id: admitted.id.clone(),
        thread_id: ThreadId("unrelated".into()),
        snapshot_id: ExecutableAgentSnapshotId("not-a-snapshot".into()),
        catalog_fingerprint: CatalogFingerprint("not-a-fingerprint".into()),
        result: ResumeResult::Continue,
        operation_id: None,
        context_messages: Vec::new(),
        now_ms: 0,
    };
    let error = runtime
        .resume(
            unrelated_resume,
            store.as_ref(),
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code(), "run_not_awaiting");
    // The model never receives a release permit. Only explicit cancellation
    // can settle this Run; Stop must not wait for the Thread admission lock.
    tokio::time::timeout(DEADLINE, runs.stop("stopped"))
        .await
        .expect("Stop blocked behind active execution")
        .unwrap();
    let stopped = tokio::time::timeout(DEADLINE, running)
        .await
        .expect("cancelled Run did not settle")
        .unwrap()
        .unwrap();
    assert_eq!(stopped.run_id(), Some(&admitted.id));
    assert_eq!(stopped.state(), &RunState::Ended(EndCause::Cancelled));
    assert_eq!(store.run(&admitted.id).unwrap().state, *stopped.state());

    let replay = runs
        .run("start", "stopped", None, input("stopped"))
        .await
        .unwrap();
    assert_eq!(replay.run_id(), stopped.run_id());
    assert_eq!(replay.state(), stopped.state());
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(runs.pending("stopped").await.unwrap().is_none());
    assert!(
        runs.history("stopped")
            .await
            .unwrap()
            .iter()
            .all(|message| message.role != Role::Assistant)
    );

    // The fixture supplied no physical termination receipt. A logical Stop
    // cannot release that Thread's fence, or cancel a different Thread.
    let blocked = runs
        .run("next", "stopped", None, input("next"))
        .await
        .expect_err("a stopped model without termination evidence still fences its Thread");
    assert!(
        blocked.message.contains("no termination evidence"),
        "{blocked}"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);

    let next = tokio::spawn({
        let runs = runs.clone();
        async move { runs.run("next", "unrelated", None, input("next")).await }
    });
    entered(&model, 1).await;
    model.release.add_permits(1);
    let next = tokio::time::timeout(DEADLINE, next)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(next.run_id(), stopped.run_id());
    assert_eq!(next.state(), &RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
}
