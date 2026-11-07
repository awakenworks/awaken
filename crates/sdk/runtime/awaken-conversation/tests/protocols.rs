//! Real protocol routers over an SDK Runtime and its committed store.
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_conversation::ConversationRuns;
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime::{ExecutableAgentSnapshot, ModelBinding, Runtime, RuntimeRunContext};
use awaken_runtime_contract::runs::{RunErrorKind, Runs};
use awaken_store_inmem::MemoryCommitCoordinator;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

#[derive(Default)]
struct TextModel(AtomicUsize);
#[async_trait::async_trait]
impl LlmExecutor for TextModel {
    async fn infer(&self, _: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text("sdk reply"),
            usage: Some(awaken_agent_contract::audit::model_request::TokenUsage {
                prompt_tokens: 7,
                completion_tokens: 3,
                ..Default::default()
            }),
            stop_reason: None,
        })
    }
}
fn fixture() -> (
    Arc<ConversationRuns>,
    Arc<MemoryCommitCoordinator>,
    Arc<TextModel>,
) {
    let clock = awaken_time::system_clock();
    let store = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let model = Arc::new(TextModel::default());
    let snapshot = ExecutableAgentSnapshot::builder("selected")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .build();
    let runtime = Arc::new(
        Runtime::new()
            .with_clock(clock.clone())
            .with_llm(model.clone()),
    );
    let runs = ConversationRuns::new(
        runtime,
        snapshot,
        RuntimeRunContext::new().with_store(store.clone()),
        clock,
    )
    .unwrap();
    (Arc::new(runs), store, model)
}
#[tokio::test]
async fn sdk_replay_reads_the_committed_run_and_changed_input_is_refused() {
    // R1 replay retains one original Run; R2 changed input under that key is
    // refused; R3 an unselected Agent cannot silently use the selected one.
    let (runs, store, model) = fixture();
    let input = vec![Message::text(Id("input".into()), Role::User, "hello")];
    let first = runs
        .run("operation", "thread", None, input.clone())
        .await
        .unwrap();
    let replay = runs.run("operation", "thread", None, input).await.unwrap();
    assert_eq!(first.run_id(), replay.run_id());
    assert_eq!(
        store.run(first.run_id().unwrap()).unwrap().state,
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert_eq!(replay.new_messages, first.new_messages);
    let changed = vec![Message::text(Id("input".into()), Role::User, "different")];
    assert_eq!(
        runs.run("operation", "thread", None, changed)
            .await
            .unwrap_err()
            .kind,
        RunErrorKind::Conflict
    );
    assert!(
        runs.run("another", "thread", Some("unselected".into()), vec![])
            .await
            .is_err()
    );
    assert_eq!(model.0.load(Ordering::SeqCst), 1);
    assert_eq!(runs.usage("thread").await.unwrap(), (7, 3));
    assert_eq!(
        runs.history("thread")
            .await
            .unwrap()
            .iter()
            .map(Message::text_content)
            .collect::<Vec<_>>(),
        ["hello", "sdk reply"]
    );
}
#[tokio::test]
async fn ag_ui_and_ai_sdk_drive_sdk_runs_over_the_same_committed_thread() {
    // R1/R2 both real HTTP bodies commit a Run; R3 the second adapter sees
    // the first adapter's history. A response-only mock cannot satisfy R3.
    let (runs, store, model) = fixture();
    let cases = [
        (
            awaken_protocol_ag_ui::router(runs.clone(), "local".into()),
            "/v1/ag-ui",
            json!({"threadId":"shared", "runId":"ag-run", "messages":[{"id":"u1", "role":"user", "content":"first"}]}),
            "RUN_FINISHED",
        ),
        (
            awaken_protocol_ai_sdk::router(runs.clone(), "local".into()),
            "/v1/ai-sdk/chat",
            json!({"threadId":"shared", "messages":[{"id":"u2", "role":"user", "parts":[{"type":"text", "text":"second"}]}]}),
            "finish",
        ),
    ];
    for (router, path, input, terminal) in cases {
        let response = router
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(input.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("sdk reply"), "{body}");
        assert!(body.contains(terminal), "{body}");
    }
    assert_eq!(model.0.load(Ordering::SeqCst), 2);
    assert_eq!(runs.usage("shared").await.unwrap(), (14, 6));
    let latest = store
        .latest_run(&awaken_agent_contract::agent::thread::Id("shared".into()))
        .unwrap();
    assert_eq!(latest.state, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        runs.history("shared")
            .await
            .unwrap()
            .iter()
            .map(Message::text_content)
            .collect::<Vec<_>>(),
        ["first", "sdk reply", "second", "sdk reply"]
    );
}

struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl awaken_runtime::tools::Tool for Echo {
    type Args = serde_json::Value;
    type Output = String;
    const ID: &'static str = "sdk.echo";
    const DESCRIPTION: &'static str = "Echo after approval";
    async fn call(&self, _: Self::Args) -> Result<String, awaken_runtime::tools::ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("echoed".into())
    }
}
struct ToolModel;
#[async_trait::async_trait]
impl LlmExecutor for ToolModel {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        let output = if request
            .messages
            .iter()
            .any(|message| message.text_content().contains("echoed"))
        {
            AssistantOutput::text("complete")
        } else {
            AssistantOutput::from_tool_calls(vec![awaken_runtime::model::ToolCall {
                call_id: "echo-call".into(),
                tool_id: "sdk.echo".into(),
                arguments: json!({}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}
struct Confirm;
#[async_trait::async_trait]
impl awaken_runtime::permission::ToolGateHook for Confirm {
    async fn gate(
        &self,
        _: &awaken_runtime::model::ToolCall,
        _: &awaken_runtime::permission::Store,
    ) -> awaken_runtime::permission::GateDecision {
        awaken_runtime::permission::GateDecision::plugin_gate(
            awaken_runtime::permission::GateOutcome::RequireConfirmation {
                correlation_id: "sdk-confirm".into(),
            },
        )
    }
}
fn approval_fixture() -> (
    ConversationRuns,
    Arc<MemoryCommitCoordinator>,
    Arc<AtomicUsize>,
) {
    approval_fixture_with_model(Arc::new(ToolModel))
}

fn approval_fixture_with_model(
    model: Arc<dyn LlmExecutor>,
) -> (
    ConversationRuns,
    Arc<MemoryCommitCoordinator>,
    Arc<AtomicUsize>,
) {
    let hits = Arc::new(AtomicUsize::new(0));
    let registration =
        awaken_runtime::tools::ToolRegistration::new("sdk", Echo(hits.clone())).unwrap();
    let snapshot = ExecutableAgentSnapshot::builder("selected")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(registration.descriptor().clone())
        .max_steps(8)
        .build();
    let clock = awaken_time::system_clock();
    let runtime = registration
        .install(Runtime::new().with_clock(clock.clone()).with_llm(model))
        .unwrap()
        .with_gate(Arc::new(Confirm));
    let store = Arc::new(MemoryCommitCoordinator::new(clock.clone()));
    let runs = ConversationRuns::new(
        Arc::new(runtime),
        snapshot,
        RuntimeRunContext::new().with_store(store.clone()),
        clock,
    )
    .unwrap();
    (runs, store, hits)
}
#[tokio::test]
async fn sdk_resume_requires_the_committed_call_and_replays_without_another_effect() {
    // R1 Awaiting commits without invoking the tool; R2 a different call is
    // rejected with the original ticket intact; R3 approval consumes that
    // ticket once; R4 replay reads the same terminal Run and one tool effect.
    // R5 the original submission includes the resumed answer on replay, while
    // a later Run cannot leak its messages into that original outcome.
    use awaken_agent_contract::agent::awaiting::PermissionDecision;
    use awaken_runtime_contract::runs::RunResume;
    let (runs, store, hits) = approval_fixture();
    let input = vec![Message::text(Id("question".into()), Role::User, "echo")];
    let first = runs
        .run("start", "approval", None, input.clone())
        .await
        .unwrap();
    assert_eq!(first.state(), &RunState::Awaiting);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let pending = runs.pending("approval").await.unwrap().unwrap();
    let answer = RunResume::Permission(PermissionDecision::Allow { note: None });
    assert!(
        runs.resume("wrong", "approval", "another-call", answer.clone())
            .await
            .is_err()
    );
    assert!(store.resume_ticket(first.run_id().unwrap()).is_some());
    let resumed = runs
        .resume("answer", "approval", &pending.tool_use_id, answer.clone())
        .await
        .unwrap();
    let replay = runs
        .resume("answer", "approval", &pending.tool_use_id, answer)
        .await
        .unwrap();
    assert_eq!(resumed.run_id(), first.run_id());
    assert_eq!(replay.run_id(), first.run_id());
    assert_eq!(replay.state(), &RunState::Ended(EndCause::NaturalEnd));
    assert!(store.resume_ticket(first.run_id().unwrap()).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let submitted = runs
        .run("start", "approval", None, input.clone())
        .await
        .unwrap();
    assert_eq!(submitted.run_id(), first.run_id());
    assert!(
        submitted
            .new_messages
            .iter()
            .any(|message| message.text_content() == "complete")
    );
    let later = runs
        .run(
            "later",
            "approval",
            None,
            vec![Message::text(
                Id("later-question".into()),
                Role::User,
                "later-turn-marker",
            )],
        )
        .await
        .unwrap();
    assert_ne!(later.run_id(), first.run_id());
    let replayed = runs.run("start", "approval", None, input).await.unwrap();
    assert_eq!(replayed.new_messages, submitted.new_messages);
    assert!(
        !replayed
            .new_messages
            .iter()
            .any(|message| message.text_content().contains("later-turn-marker"))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn sdk_stop_commits_cancellation_and_removes_the_open_wait_without_a_tool_effect() {
    // R1 explicit Stop answers the committed Awaiting Run, clears its ticket,
    // and leaves the unapproved tool unexecuted. R2 a second Stop is harmless.
    let (runs, store, hits) = approval_fixture();
    let first = runs
        .run(
            "start",
            "stop",
            None,
            vec![Message::text(Id("question".into()), Role::User, "echo")],
        )
        .await
        .unwrap();
    assert_eq!(first.state(), &RunState::Awaiting);
    runs.stop("stop").await.unwrap();
    runs.stop("stop").await.unwrap();
    assert_eq!(
        store.run(first.run_id().unwrap()).unwrap().state,
        RunState::Ended(EndCause::Cancelled)
    );
    assert!(runs.pending("stop").await.unwrap().is_none());
    assert!(store.resume_ticket(first.run_id().unwrap()).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn rejected_resume_never_becomes_successful_replay_or_reserves_the_operation() {
    use awaken_agent_contract::agent::awaiting::PermissionDecision;
    use awaken_runtime_contract::runs::RunResume;

    let (runs, store, hits) = approval_fixture();
    let first = runs
        .run(
            "start",
            "rejected-answer",
            None,
            vec![Message::text(Id("question".into()), Role::User, "echo")],
        )
        .await
        .unwrap();
    let ticket = store.resume_ticket(first.run_id().unwrap()).unwrap();
    let pending = runs.pending("rejected-answer").await.unwrap().unwrap();
    // A client result cannot answer a permission wait. Repeating the rejected
    // command must not turn its unchanged Awaiting Run into an accepted reply.
    for _ in 0..2 {
        let error = runs
            .resume(
                "answer",
                "rejected-answer",
                &pending.tool_use_id,
                RunResume::ClientResult {
                    content: Vec::new(),
                    is_error: false,
                },
            )
            .await
            .expect_err("invalid resume must be refused on every delivery");
        assert_eq!(error.kind, RunErrorKind::BadRequest);
        assert_eq!(error.code, "resume_result_kind_mismatch");
        assert_eq!(
            store.resume_ticket(first.run_id().unwrap()),
            Some(ticket.clone())
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
    // Rejection precedes admission: the corrected command may use that key,
    // consumes the original ticket and executes the approved effect once.
    let resumed = runs
        .resume(
            "answer",
            "rejected-answer",
            &pending.tool_use_id,
            RunResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .unwrap();
    assert_eq!(resumed.run_id(), first.run_id());
    assert_eq!(resumed.state(), &RunState::Ended(EndCause::NaturalEnd));
    assert!(store.resume_ticket(first.run_id().unwrap()).is_none());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

struct GatedAfterApproval(tokio::sync::Notify);

#[async_trait::async_trait]
impl LlmExecutor for GatedAfterApproval {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        if request
            .messages
            .iter()
            .any(|message| message.text_content() == "echoed")
        {
            self.0.notify_one();
            std::future::pending().await
        } else {
            ToolModel.infer(request).await
        }
    }
}

#[tokio::test]
async fn stop_reaches_an_approved_run_after_its_ticket_has_been_consumed() {
    use awaken_agent_contract::agent::awaiting::PermissionDecision;
    use awaken_runtime_contract::runs::RunResume;
    use std::time::Duration;

    let deadline = Duration::from_secs(10);
    let model = Arc::new(GatedAfterApproval(tokio::sync::Notify::new()));
    let (runs, store, hits) = approval_fixture_with_model(model.clone());
    let runs = Arc::new(runs);
    let awaiting = runs
        .run(
            "start",
            "resumed",
            None,
            vec![Message::text(Id("question".into()), Role::User, "echo")],
        )
        .await
        .unwrap();
    let pending = runs.pending("resumed").await.unwrap().unwrap();
    let resumed = tokio::spawn({
        let runs = runs.clone();
        async move {
            runs.resume(
                "answer",
                "resumed",
                &pending.tool_use_id,
                RunResume::Permission(PermissionDecision::Allow { note: None }),
            )
            .await
        }
    });
    tokio::time::timeout(deadline, model.0.notified())
        .await
        .expect("the resumed model must enter after the approved effect");
    let run_id = awaiting.run_id().unwrap();
    assert!(store.resume_ticket(run_id).is_none());
    assert_eq!(store.run(run_id).unwrap().state, RunState::Running);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // There is no waiting ticket for Stop to clear. Only the active resume
    // registration can deliver cancellation to the still-running model.
    tokio::time::timeout(deadline, runs.stop("resumed"))
        .await
        .unwrap()
        .unwrap();
    let stopped = tokio::time::timeout(deadline, resumed)
        .await
        .expect("Stop must reach the resumed physical attempt")
        .unwrap()
        .unwrap();
    assert_eq!(stopped.run_id(), Some(run_id));
    assert_eq!(
        store.run(run_id).unwrap().state,
        RunState::Ended(EndCause::Cancelled)
    );
    assert_eq!(stopped.state(), &RunState::Ended(EndCause::Cancelled));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
