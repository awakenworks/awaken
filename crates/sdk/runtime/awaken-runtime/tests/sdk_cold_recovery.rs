//! T02-C02 (重建与恢复) and T03-C02 (审批超时后恢复): cold recovery across a
//! real process death over a durable SQLite commit store.
//!
//! An owned child process builds the SDK `Runtime`, runs a gated tool call to
//! its approval wait (T02) or lets the approval time out (T03), records the
//! Run identity and is then killed by the parent. The parent opens the same
//! database file with a NEW store object and builds a NEW `Runtime` and
//! `Conversation`, then replies to the original ticket.
//!
//! | Row | Child | Parent (rebuilt) | Expected |
//! |---|---|---|---|
//! | R1 | run -> Awaiting, killed | resume the original id: allow | same Run ends NaturalEnd; tool runs once; input once |
//! | R2 | approval timeout -> Awaiting, killed | resume: deny | same Run ends; tool never runs; ticket consumed |
//! | R3 | R1/R2 then a second reply | replay | the same terminal, no callback, model or tool effect |
//! | NC | R1, but recovery starts a NEW Run | - | the same checker reports the new Run and the lost ticket |
//!
//! Oracles: T02-C02 O0 the original wait point and Run are restored; O1 no new
//! Run or repeated committed step. T03-C02 O0 the defined wait/terminal
//! semantics are kept; O1 a valid resume is admitted at most once and yields
//! the original Run.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use awaken_agent_contract::agent::message::Role;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::tools::{Tool, ToolError, ToolRegistration};
use awaken_runtime::{ApprovalOptions, Runtime};
use awaken_runtime_contract::llm::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor};
use awaken_runtime_contract::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::{ExecutableAgentSnapshot, RuntimeRunContext};
use awaken_store_sqlite::SqliteCommitCoordinator;
use awaken_tool_contract::tool::ToolCall;
use schemars::JsonSchema;
use serde::Deserialize;

const CHILD: &str = "cold_recovery_child";
const INPUT: &str = "cold hello";

#[derive(Deserialize, JsonSchema)]
struct EchoArgs {
    text: String,
}

struct Echo(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Tool for Echo {
    type Args = EchoArgs;
    type Output = String;
    const ID: &'static str = "app.echo";
    const DESCRIPTION: &'static str = "Echo text";
    async fn call(&self, args: EchoArgs) -> Result<String, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("echoed: {}", args.text))
    }
}

/// One tool call, then a final answer once a tool result is in the window.
#[derive(Default)]
struct Model {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for Model {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if request.messages.iter().any(|m| m.role == Role::Tool) {
            AssistantOutput::text("complete")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "echo-1".into(),
                tool_id: Echo::ID.into(),
                arguments: serde_json::json!({"text": "hello"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct ApprovalGate;

#[async_trait::async_trait]
impl ToolGateHook for ApprovalGate {
    async fn gate(
        &self,
        _: &ToolCall,
        _: &awaken_agent_contract::agent::state::Store,
    ) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "approve-echo".into(),
        })
    }
}

/// A freshly built SDK owner: its own Runtime, model, tool counter and Snapshot.
struct Owner {
    runtime: Runtime,
    snapshot: ExecutableAgentSnapshot,
    model: Arc<Model>,
    hits: Arc<AtomicUsize>,
}

fn build_owner() -> Owner {
    let model = Arc::new(Model::default());
    let hits = Arc::new(AtomicUsize::new(0));
    let registration = ToolRegistration::new("app", Echo(hits.clone())).unwrap();
    let runtime = registration
        .install(
            Runtime::new()
                .with_clock(awaken_time::system_clock())
                .with_llm(model.clone()),
        )
        .unwrap()
        .with_gate(Arc::new(ApprovalGate));
    let snapshot = ExecutableAgentSnapshot::builder("sdk")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(registration.descriptor().clone())
        .max_steps(8)
        .build();
    Owner {
        runtime,
        snapshot,
        model,
        hits,
    }
}

fn thread(row: &str) -> ThreadId {
    ThreadId(format!("t02-t03-{row}"))
}

/// The recovery checker over the reopened store after recovery.
fn recovery_violations(
    store: &SqliteCommitCoordinator,
    row: &str,
    original: &RunId,
    expected_tool_runs: usize,
    observed_tool_runs: usize,
) -> Vec<String> {
    let mut violations = Vec::new();
    match store.latest_run(&thread(row)) {
        Some(record) if &record.id == original => {
            if !matches!(record.state, RunState::Ended(_)) {
                violations.push(format!(
                    "{row}/O0 the original Run did not settle: {:?}",
                    record.state
                ));
            }
        }
        Some(record) => violations.push(format!(
            "{row}/O1 recovery created a new Run {:?} instead of resuming {:?}",
            record.id, original
        )),
        None => violations.push(format!("{row}/O0 the original Run is gone")),
    }
    if store.resume_ticket(original).is_some() {
        violations.push(format!("{row}/O1 the original ticket was not consumed"));
    }
    let messages = store.committed_messages(&thread(row));
    let inputs = messages
        .iter()
        .filter(|m| m.text_content() == INPUT)
        .count();
    if inputs != 1 {
        violations.push(format!(
            "{row}/O1 the original input is committed {inputs} times"
        ));
    }
    if observed_tool_runs != expected_tool_runs {
        violations.push(format!(
            "{row}/O1 the tool ran {observed_tool_runs} times, expected {expected_tool_runs}"
        ));
    }
    violations
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Run the child to its recorded wait point, kill it, and return the Run id.
fn crash_child(database: &Path, row: &str, root: &Path) -> RunId {
    let marker = root.join(format!("{row}.run"));
    let diagnostics = root.join(format!("{row}.stderr"));
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env("AWAKEN_T02_DATABASE", database)
            .env("AWAKEN_T02_ROW", row)
            .env("AWAKEN_T02_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&diagnostics).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    // awaken-allow(wait): bounded native child readiness at its recorded wait point.
    let run = loop {
        if let Ok(run) = std::fs::read_to_string(&marker)
            && !run.is_empty()
        {
            break run;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "{row}: child exited early: {}",
            std::fs::read_to_string(&diagnostics).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "{row}: child never reached its wait point"
        );
        // awaken-allow(wait): poll interval of the bounded child-readiness loop above.
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_ne!(
        child.0.id(),
        std::process::id(),
        "NC: a separate OS process owned the Run"
    );
    child.0.kill().unwrap();
    assert!(
        !child.0.wait().unwrap().success(),
        "{row}: the child was killed, not exited"
    );
    RunId(run)
}

async fn reopen(database: &Path) -> Arc<SqliteCommitCoordinator> {
    Arc::new(
        SqliteCommitCoordinator::open(database.to_str().unwrap())
            .await
            .expect("reopen the durable commit store"),
    )
}

/// T02-C02 O0 / T03-C02 O0 before the reply: the original wait point.
fn assert_waiting(store: &SqliteCommitCoordinator, row: &str, original: &RunId) {
    let record = store
        .latest_run(&thread(row))
        .expect("the original Run is durable");
    assert_eq!(&record.id, original, "{row}/O0 the original Run identity");
    assert_eq!(
        record.state,
        RunState::Awaiting,
        "{row}/O0 the original wait point"
    );
    assert!(
        store.resume_ticket(original).is_some(),
        "{row}/O0 the ticket survived the process death"
    );
}

#[tokio::test]
async fn a_rebuilt_runtime_resumes_the_original_run_after_process_death() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("runs.db");
    let original = crash_child(&database, "r1", root.path());
    let store = reopen(&database).await;
    assert_waiting(&store, "r1", &original);

    let owner = build_owner();
    let conversation = owner
        .runtime
        .conversation(
            &owner.snapshot,
            &thread("r1").0,
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap();
    let resumed = conversation
        .resume_with_id_async(original.clone(), INPUT, |_| async {
            Ok(ResumeResult::allow())
        })
        .await
        .unwrap();
    assert_eq!(
        resumed.run_id(),
        &original,
        "T02 O0 the original Run resumes"
    );
    assert_eq!(resumed.end_cause(), Some(&EndCause::NaturalEnd));
    assert_eq!(
        recovery_violations(
            &store,
            "r1",
            &original,
            1,
            owner.hits.load(Ordering::SeqCst)
        ),
        Vec::<String>::new()
    );

    // R3: a second reply is the committed terminal, with no new effect.
    let (commits, calls) = (
        store.commit_count(),
        owner.model.calls.load(Ordering::SeqCst),
    );
    let replay = conversation
        .resume_with_id_async(original.clone(), "ignored", |_| async {
            panic!("a terminal replay never asks for approval")
        })
        .await
        .unwrap();
    assert_eq!(
        replay, resumed,
        "T02 O1 replay returns the original terminal"
    );
    assert_eq!(store.commit_count(), commits, "T02 O1 no new commit");
    assert_eq!(
        owner.model.calls.load(Ordering::SeqCst),
        calls,
        "T02 O1 no model call"
    );
    assert_eq!(
        owner.hits.load(Ordering::SeqCst),
        1,
        "T02 O1 the tool ran once"
    );
}

#[tokio::test]
async fn a_timed_out_approval_survives_process_death_and_is_answered_once() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("runs.db");
    let original = crash_child(&database, "r2", root.path());
    let store = reopen(&database).await;
    assert_waiting(&store, "r2", &original);

    let owner = build_owner();
    let conversation = owner
        .runtime
        .conversation(
            &owner.snapshot,
            &thread("r2").0,
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap();
    let denied = conversation
        .resume_with_id_async(original.clone(), INPUT, |_| async {
            Ok(ResumeResult::deny(Some(
                "declined after the timeout".into(),
            )))
        })
        .await
        .unwrap();
    assert_eq!(
        denied.run_id(),
        &original,
        "T03 O1 the original Run is answered"
    );
    assert!(
        denied.state().is_terminal(),
        "T03 O0 the denial reaches a terminal"
    );
    assert_eq!(
        recovery_violations(
            &store,
            "r2",
            &original,
            0,
            owner.hits.load(Ordering::SeqCst)
        ),
        Vec::<String>::new()
    );
    let commits = store.commit_count();
    let replay = conversation
        .resume_with_id_async(original.clone(), "ignored", |_| async {
            panic!("an answered ticket is never asked again")
        })
        .await
        .unwrap();
    assert_eq!(replay, denied, "T03 O1 at most one admitted reply");
    assert_eq!(store.commit_count(), commits, "T03 O1 no second admission");
    assert_eq!(
        owner.hits.load(Ordering::SeqCst),
        0,
        "T03 O0 a denied tool never runs"
    );
}

#[tokio::test]
async fn a_recovery_that_starts_a_new_run_is_detected() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("runs.db");
    let original = crash_child(&database, "nc", root.path());
    let store = reopen(&database).await;
    assert_waiting(&store, "nc", &original);

    // The divergent recovery: the rebuilt owner re-runs the input as a new Run.
    let owner = build_owner();
    let fresh = owner
        .runtime
        .conversation(
            &owner.snapshot,
            &thread("nc").0,
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run_to_completion_async(INPUT, |_| async { Ok(ResumeResult::allow()) })
        .await;
    let violations = recovery_violations(
        &store,
        "nc",
        &original,
        1,
        owner.hits.load(Ordering::SeqCst),
    );
    assert!(
        fresh.is_err()
            || violations
                .iter()
                .any(|v| v.contains("new Run") || v.contains("ticket")),
        "NC: a new Run in place of the original resume is rejected or reported: {violations:#?}"
    );
    // The same checker, told to expect a Run other than the store's, reports it.
    let foreign = RunId("run_t02_not_the_resumed_one".into());
    assert!(
        recovery_violations(&store, "nc", &foreign, 0, 0)
            .iter()
            .any(|v| v.starts_with("nc/O1 recovery created a new Run")),
        "NC: the checker distinguishes the resumed Run from any other"
    );
}

/// The owned child: drive the row to its wait point, record the Run id, park.
#[tokio::test]
#[ignore = "owned child invoked and killed by the cold-recovery tests"]
async fn cold_recovery_child() {
    let database = std::env::var("AWAKEN_T02_DATABASE").unwrap();
    let row = std::env::var("AWAKEN_T02_ROW").unwrap();
    let marker = std::env::var_os("AWAKEN_T02_MARKER").unwrap();
    let store = Arc::new(SqliteCommitCoordinator::open(&database).await.unwrap());
    let owner = build_owner();
    let conversation = owner
        .runtime
        .conversation(
            &owner.snapshot,
            &thread(&row).0,
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap();
    if row == "r2" {
        let error = conversation
            .run_to_completion_async_with_options(
                INPUT,
                ApprovalOptions::default().with_timeout(Duration::ZERO),
                |_| std::future::pending(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), "approval_timeout");
    } else {
        let pending = conversation.run(INPUT).await.unwrap();
        assert_eq!(pending.state(), &RunState::Awaiting);
    }
    let record = store
        .latest_run(&thread(&row))
        .expect("the Run is committed");
    assert_eq!(
        record.state,
        RunState::Awaiting,
        "the child stops at the wait point"
    );
    assert_eq!(
        owner.hits.load(Ordering::SeqCst),
        0,
        "no tool effect before approval"
    );
    std::fs::write(&marker, record.id.0.as_bytes()).unwrap();
    // awaken-allow(wait): the child is parked at its recorded wait point; the parent owns and kills it.
    std::future::pending::<()>().await;
}
