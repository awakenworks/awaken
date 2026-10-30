//! Session-acceptance scenarios on the independent consumer (public deps only).
//!
//! | Scenario | Rows |
//! |---|---|
//! | T02-C01 | single- and multi-turn Runs commit and are re-read; the normal dependency closure holds only `product = "sdk"` crates and `awaken-runtime` declares no `[features]` (minimal == default); NC: a copy that adds a server Session crate fails the same closure check |
//! | T03-C01 | first Awaiting return: original Run, unconsumed ticket, zero tool hits, bounded return, stable re-read; NC: auto-allow gate and a gate that blocks to the terminal |
//! | T03-C03 | cancel before admission, cancel while awaiting, failing callback then resume of the same Run, commit N failure; NC: an `is_err`-only oracle accepts a mutant that still invokes the model |
//!
//! Must not be merged into `tasks.rs`: `scripts/ci/check_sdk_consumer.py`
//! pins that binary's exact test names.
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator as CommitCoordinator, Error as CommitError,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_runtime::model::{AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall};
use awaken_runtime::permission::{GateDecision, GateOutcome, ToolGateHook};
use awaken_runtime::tools::{Tool, ToolError, ToolRegistration};
use awaken_runtime::{
    ApprovalError, CancellationToken, EndCause, ExecutableAgentSnapshot, ModelBinding,
    ResumeResult, RunOutcome, RunState, Runtime, RuntimeRunContext,
};
use awaken_store_inmem::MemoryCommitCoordinator;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

fn memory() -> Arc<MemoryCommitCoordinator> {
    Arc::new(MemoryCommitCoordinator::new(awaken_time::system_clock()))
}

/// Text-only model that counts every logical request and its history length.
#[derive(Default)]
struct CountingText {
    calls: AtomicUsize,
    longest_history: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for CountingText {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.longest_history
            .fetch_max(request.messages.len(), Ordering::SeqCst);
        Ok(ChatResponse {
            output: AssistantOutput::text(format!("reply {call}")),
            usage: None,
            stop_reason: None,
        })
    }
}

fn text_agent() -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder("acceptance")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .build()
}

// ── T02-C01: real Runs, read back from the committed store ──────────────────

#[tokio::test]
async fn t02_c01_single_turn_commits_a_run_that_is_read_back_from_the_store() {
    let store = memory();
    let model = Arc::new(CountingText::default());
    let runtime = Runtime::new().with_llm(model.clone());
    let outcome = runtime
        .run_with_outcome(
            &text_agent(),
            "single",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .await
        .unwrap();
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "O1 the model ran once"
    );
    assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    assert!(store.commit_count() > 0, "O1 the Run was committed");
    // O1 the result is read from the committed record, not a local buffer.
    let reread = RunOutcome::read(store.as_ref(), outcome.run_id(), outcome.thread_id()).unwrap();
    assert_eq!(reread, outcome);
    assert_eq!(
        store.latest_run(outcome.thread_id()).unwrap().id,
        *outcome.run_id()
    );
    let transcript = outcome.transcript(store.as_ref()).unwrap();
    assert_eq!(transcript.len(), 2);
    assert_eq!(transcript[0].text_content(), "single");
    assert_eq!(transcript[1].text_content(), "reply 1");
}

#[tokio::test]
async fn t02_c01_multi_turn_runs_share_the_thread_and_each_is_read_back() {
    let store = memory();
    let model = Arc::new(CountingText::default());
    let runtime = Runtime::new().with_llm(model.clone());
    let agent = text_agent();
    let context = RuntimeRunContext::new().with_store(store.clone());
    let conversation = runtime
        .conversation(&agent, "acceptance-multi", context)
        .unwrap();
    let first = conversation.run("first").await.unwrap();
    let second = conversation.run("second").await.unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    assert!(
        model.longest_history.load(Ordering::SeqCst) >= 3,
        "O1 the second turn saw the committed first turn"
    );
    assert_eq!(first.thread_id(), second.thread_id());
    assert_ne!(first.run_id(), second.run_id());
    for outcome in [&first, &second] {
        assert_eq!(conversation.outcome(outcome.run_id()).unwrap(), *outcome);
        assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    }
    let texts: Vec<String> = second
        .transcript(store.as_ref())
        .unwrap()
        .iter()
        .map(|message| message.text_content())
        .collect();
    assert_eq!(texts, ["first", "reply 1", "second", "reply 2"]);
}

// ── T02-C01: dependency closure ─────────────────────────────────────────────

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `<repository>/crates` as a TOML-safe absolute path (no `..` segments).
fn repository_crates() -> String {
    let repository = manifest_dir()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    repository
        .join("crates")
        .to_string_lossy()
        .replace('\\', "/")
}

fn cargo() -> Command {
    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
}

fn metadata(manifest: &Path, extra: &[&str]) -> Value {
    let output = cargo()
        .args(["metadata", "--format-version", "1"])
        .args(extra)
        .arg("--manifest-path")
        .arg(manifest)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo metadata {extra:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The T02-C01 oracle over full `cargo metadata` (with the resolve graph):
/// every package reachable from the consumer through NORMAL dependency edges
/// that is a repository crate (path source) or an `awaken*` crate must declare
/// `[package.metadata.awaken] product = "sdk"`; `awaken-runtime` must be in the
/// closure and declare no `[features]`, so minimal and default selections are
/// one graph (a future default feature makes this fail and forces review).
fn closure_violations(metadata: &Value) -> (Vec<String>, Vec<String>) {
    let packages: std::collections::HashMap<&str, &Value> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| (package["id"].as_str().unwrap(), package))
        .collect();
    let nodes: std::collections::HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();
    let root = metadata["resolve"]["root"].as_str().unwrap();
    let mut closure = std::collections::BTreeSet::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if !closure.insert(id) {
            continue;
        }
        for dependency in nodes[id]["deps"].as_array().unwrap() {
            let normal = dependency["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind["kind"].is_null());
            if normal {
                pending.push(dependency["pkg"].as_str().unwrap());
            }
        }
    }
    let mut violations = Vec::new();
    let mut repository_crates = Vec::new();
    for id in closure.iter().filter(|id| **id != root) {
        let package = packages[id];
        let name = package["name"].as_str().unwrap();
        if !(package["source"].is_null() || name.starts_with("awaken")) {
            continue;
        }
        repository_crates.push(name.to_string());
        let awaken = &package["metadata"]["awaken"];
        if awaken["product"] != "sdk" {
            violations.push(format!(
                "{name}: product={} layer={} bounded_context={}",
                awaken["product"], awaken["layer"], awaken["bounded_context"]
            ));
        }
    }
    match closure
        .iter()
        .map(|id| packages[id])
        .find(|package| package["name"] == "awaken-runtime")
    {
        None => violations.push("awaken-runtime is not in the normal closure".into()),
        Some(runtime) if runtime["features"] != serde_json::json!({}) => violations.push(format!(
            "awaken-runtime declares [features] {}: minimal and default diverge",
            runtime["features"]
        )),
        Some(_) => {}
    }
    repository_crates.sort();
    (violations, repository_crates)
}

#[test]
fn t02_c01_normal_dependency_closure_holds_only_sdk_crates() {
    let metadata = metadata(&manifest_dir().join("Cargo.toml"), &["--locked"]);
    let (violations, crates) = closure_violations(&metadata);
    assert_eq!(
        violations,
        Vec::<String>::new(),
        "O0/O2 closure: {crates:?}"
    );
    // Non-vacuous: the SDK crates the consumer links are what was judged.
    for expected in [
        "awaken-agent-contract",
        "awaken-runtime",
        "awaken-runtime-contract",
        "awaken-store-inmem",
        "awaken-time",
        "awaken-tool-contract",
    ] {
        assert!(
            crates.iter().any(|name| name == expected),
            "{expected} in {crates:?}"
        );
    }
    assert!(!crates.iter().any(|name| name.starts_with("awaken-session")));
}

/// Writes a copy of this consumer whose path dependencies point back at the
/// repository, optionally adding one more dependency line.
fn consumer_copy(directory: &Path, extra_dependency: Option<&str>) -> PathBuf {
    let repository = manifest_dir()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let crates = repository_crates();
    let mut manifest = std::fs::read_to_string(manifest_dir().join("Cargo.toml"))
        .unwrap()
        .replace("\"../../crates/", &format!("\"{crates}/"));
    assert!(
        !manifest.contains("../../"),
        "every path dependency was rewritten"
    );
    if let Some(line) = extra_dependency {
        manifest = manifest.replacen("[dependencies]", &format!("[dependencies]\n{line}"), 1);
        assert!(manifest.contains(line));
    }
    std::fs::create_dir_all(directory.join("src")).unwrap();
    for file in ["lib.rs", "main.rs"] {
        std::fs::copy(
            manifest_dir().join("src").join(file),
            directory.join("src").join(file),
        )
        .unwrap();
    }
    // The repository lock pins every version the server crates need, so the
    // copy resolves offline without touching this consumer's own Cargo.lock.
    std::fs::copy(repository.join("Cargo.lock"), directory.join("Cargo.lock")).unwrap();
    std::fs::write(directory.join("Cargo.toml"), manifest).unwrap();
    directory.join("Cargo.toml")
}

#[test]
fn t02_c01_nc_a_copy_with_a_server_session_dependency_fails_the_closure_check() {
    let control = tempfile::tempdir().unwrap();
    let control = metadata(&consumer_copy(control.path(), None), &["--offline"]);
    assert_eq!(
        closure_violations(&control).0,
        Vec::<String>::new(),
        "unchanged copy passes"
    );

    let mutant = tempfile::tempdir().unwrap();
    let crates = repository_crates();
    let line = format!(
        "awaken-session-application = {{ path = \"{crates}/server/awaken-session-application\" }}"
    );
    let mutant = metadata(&consumer_copy(mutant.path(), Some(&line)), &["--offline"]);
    let (violations, _) = closure_violations(&mutant);
    assert!(
        violations
            .iter()
            .any(|violation| violation
                .starts_with("awaken-session-application: product=\"platform\"")),
        "NC server Session authority detected: {violations:?}"
    );
}

// ── shared tool fixture for T03 ─────────────────────────────────────────────

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}
struct Echo(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Tool for Echo {
    type Args = EchoArgs;
    type Output = String;
    const ID: &'static str = "acceptance.echo";
    const DESCRIPTION: &'static str = "Echo text";
    async fn call(&self, args: EchoArgs) -> Result<String, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("echoed: {}", args.text))
    }
}

#[derive(Default)]
struct CountingToolModel {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for CountingToolModel {
    async fn infer(&self, request: ChatRequest) -> awaken_runtime::model::Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if request
            .messages
            .iter()
            .any(|message| message.text_content().contains("echoed: hello"))
        {
            AssistantOutput::text("complete")
        } else {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "echo-1".into(),
                tool_id: Echo::ID.into(),
                arguments: serde_json::json!({"text":"hello"}),
            }])
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

struct ConfirmGate;
#[async_trait::async_trait]
impl ToolGateHook for ConfirmGate {
    async fn gate(&self, _: &ToolCall, _: &awaken_runtime::permission::Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::RequireConfirmation {
            correlation_id: "acceptance-approval".into(),
        })
    }
}

/// NC mutant for T03-C01: approves on its own instead of awaiting the caller.
struct AutoAllowGate;
#[async_trait::async_trait]
impl ToolGateHook for AutoAllowGate {
    async fn gate(&self, _: &ToolCall, _: &awaken_runtime::permission::Store) -> GateDecision {
        GateDecision::plugin_gate(GateOutcome::Allow)
    }
}

/// NC mutant for T03-C01: holds the caller until a decision that never comes.
struct BlockingGate;
#[async_trait::async_trait]
impl ToolGateHook for BlockingGate {
    async fn gate(&self, _: &ToolCall, _: &awaken_runtime::permission::Store) -> GateDecision {
        std::future::pending().await
    }
}

struct Fixture {
    runtime: Runtime,
    agent: ExecutableAgentSnapshot,
    model: Arc<CountingToolModel>,
    hits: Arc<AtomicUsize>,
}

fn fixture(gate: Option<Arc<dyn ToolGateHook>>) -> Fixture {
    let model = Arc::new(CountingToolModel::default());
    let hits = Arc::new(AtomicUsize::new(0));
    let registration = ToolRegistration::new("acceptance", Echo(hits.clone())).unwrap();
    let mut runtime = registration
        .install(
            Runtime::new()
                .with_clock(awaken_time::system_clock())
                .with_llm(model.clone()),
        )
        .unwrap();
    if let Some(gate) = gate {
        runtime = runtime.with_gate(gate);
    }
    let agent = ExecutableAgentSnapshot::builder("acceptance-tools")
        .model(ModelBinding::parse("demo", "stub", "stub").unwrap())
        .tool(registration.descriptor().clone())
        .max_steps(8)
        .build();
    Fixture {
        runtime,
        agent,
        model,
        hits,
    }
}

// ── T03-C01: first Awaiting return ──────────────────────────────────────────

const FIRST_RETURN_BUDGET: Duration = Duration::from_secs(5);

struct FirstReturn {
    /// `None` when the call did not return within the budget.
    outcome: Option<RunOutcome>,
    ticket: Option<ResumeTicket>,
    reread: Option<(RunOutcome, Option<ResumeTicket>)>,
    latest_run: Option<String>,
    hits: usize,
    model_calls: usize,
}

async fn first_return(gate: Arc<dyn ToolGateHook>, budget: Duration) -> FirstReturn {
    let fixture = fixture(Some(gate));
    let store = memory();
    let conversation = fixture
        .runtime
        .conversation(
            &fixture.agent,
            "first-wait",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap();
    let outcome = tokio::time::timeout(budget, conversation.run("echo"))
        .await
        .ok()
        .map(Result::unwrap);
    let ticket = outcome
        .as_ref()
        .and_then(|outcome| outcome.resume_ticket(store.as_ref()).unwrap());
    // The caller never replies; time passes, then the Run is read again.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reread = outcome.as_ref().map(|outcome| {
        let again = conversation.outcome(outcome.run_id()).unwrap();
        let ticket = again.resume_ticket(store.as_ref()).unwrap();
        (again, ticket)
    });
    FirstReturn {
        outcome,
        ticket,
        reread,
        latest_run: store
            .latest_run(conversation.thread_id())
            .map(|record| record.id.0),
        hits: fixture.hits.load(Ordering::SeqCst),
        model_calls: fixture.model.calls.load(Ordering::SeqCst),
    }
}

/// T03-C01 oracle O0 (original Run + unconsumed ticket) and O1 (no automatic
/// approval or tool progress), plus the NC's "blocks to a terminal" clause.
fn first_return_violations(observed: &FirstReturn) -> Vec<&'static str> {
    let Some(outcome) = &observed.outcome else {
        return vec!["blocked past the first wait"];
    };
    let mut violations = Vec::new();
    if outcome.state() != &RunState::Awaiting {
        violations.push("not Awaiting");
    }
    match &observed.ticket {
        None => violations.push("ticket missing"),
        Some(ticket) if ticket.run_id != *outcome.run_id() => {
            violations.push("ticket bound to another Run")
        }
        Some(_) => {}
    }
    if observed.latest_run.as_deref() != Some(outcome.run_id().0.as_str()) {
        violations.push("committed Run identity differs");
    }
    if observed.hits != 0 {
        violations.push("tool advanced");
    }
    if observed.model_calls != 1 {
        violations.push("model advanced past the gated call");
    }
    match &observed.reread {
        Some((again, ticket)) if again == outcome && *ticket == observed.ticket => {}
        _ => violations.push("re-read changed the wait"),
    }
    violations
}

#[tokio::test]
async fn t03_c01_first_awaiting_return_keeps_the_original_run_and_an_unconsumed_ticket() {
    let observed = first_return(Arc::new(ConfirmGate), FIRST_RETURN_BUDGET).await;
    assert_eq!(first_return_violations(&observed), Vec::<&str>::new());
    let outcome = observed.outcome.as_ref().unwrap();
    assert_eq!(outcome.state(), &RunState::Awaiting, "O0");
    assert_eq!(outcome.end_cause(), None, "O0 not a terminal");
    let ticket = observed.ticket.as_ref().unwrap();
    assert_eq!(ticket.run_id, *outcome.run_id(), "O0 original Run");
    assert_eq!(ticket.correlation_id, "acceptance-approval");
    assert_eq!(observed.hits, 0, "O1 the tool did not run");
    let (again, ticket_again) = observed.reread.as_ref().unwrap();
    assert_eq!(again.state(), &RunState::Awaiting, "O0 still Awaiting");
    assert_eq!(
        ticket_again.as_ref(),
        Some(ticket),
        "O0 the same ticket, unconsumed"
    );
}

#[tokio::test]
async fn t03_c01_nc_an_auto_allowing_gate_fails_the_first_return_oracle() {
    let observed = first_return(Arc::new(AutoAllowGate), FIRST_RETURN_BUDGET).await;
    assert_eq!(observed.hits, 1, "the mutant ran the tool");
    let violations = first_return_violations(&observed);
    assert!(violations.contains(&"tool advanced"), "{violations:?}");
    assert!(violations.contains(&"ticket missing"), "{violations:?}");
    assert!(violations.contains(&"not Awaiting"), "{violations:?}");
}

#[tokio::test]
async fn t03_c01_nc_a_call_that_blocks_for_the_decision_fails_the_first_return_oracle() {
    let observed = first_return(Arc::new(BlockingGate), Duration::from_millis(300)).await;
    assert_eq!(
        first_return_violations(&observed),
        vec!["blocked past the first wait"]
    );
    assert_eq!(observed.hits, 0);
}

// ── T03-C03: cancellation and callback/commit failure ───────────────────────

#[tokio::test]
async fn t03_c03_cancel_before_admission_persists_cancelled_with_zero_model_calls() {
    let fixture = fixture(Some(Arc::new(ConfirmGate)));
    let store = memory();
    let token = CancellationToken::new();
    token.cancel();
    let outcome = fixture
        .runtime
        .run_with_outcome(
            &fixture.agent,
            "cancelled",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_cancellation(token),
        )
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::Cancelled));
    let record = store
        .run(outcome.run_id())
        .expect("O0 the cancellation is committed");
    assert_eq!(record.id, *outcome.run_id(), "O0 original identity");
    assert_eq!(
        record.state,
        RunState::Ended(EndCause::Cancelled),
        "O0 persisted"
    );
    assert!(store.resume_ticket(outcome.run_id()).is_none());
    assert_eq!(
        fixture.model.calls.load(Ordering::SeqCst),
        0,
        "O1 no model call"
    );
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn t03_c03_cancel_while_awaiting_persists_cancelled_without_running_the_tool() {
    let fixture = fixture(Some(Arc::new(ConfirmGate)));
    let store = memory();
    let token = CancellationToken::new();
    let callback_token = token.clone();
    let outcome = fixture
        .runtime
        .conversation(
            &fixture.agent,
            "cancel-awaiting",
            RuntimeRunContext::new()
                .with_store(store.clone())
                .with_cancellation(token),
        )
        .unwrap()
        .run_to_completion_async("echo", move |_| {
            let token = callback_token.clone();
            async move {
                token.cancel();
                std::future::pending().await
            }
        })
        .await
        .unwrap();
    let record = store.run(outcome.run_id()).unwrap();
    assert_eq!(
        record.state,
        RunState::Ended(EndCause::Cancelled),
        "O0 persisted"
    );
    assert!(store.resume_ticket(outcome.run_id()).is_none());
    assert_eq!(
        fixture.model.calls.load(Ordering::SeqCst),
        1,
        "O1 only the gated call"
    );
    assert_eq!(
        fixture.hits.load(Ordering::SeqCst),
        0,
        "O1 the tool never ran"
    );
}

#[tokio::test]
async fn t03_c03_failed_callback_keeps_the_ticket_and_the_same_run_resumes_once() {
    let fixture = fixture(Some(Arc::new(ConfirmGate)));
    let store = memory();
    let conversation = fixture
        .runtime
        .conversation(
            &fixture.agent,
            "callback-failure",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap();
    let error = conversation
        .run_to_completion_async("echo", |_| async {
            Err(ApprovalError::failed(std::io::Error::other(
                "approval service offline",
            )))
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), "approval_failed");
    // O1 the failure is not a business result: persisted Awaiting, ticket kept.
    let record = store.latest_run(conversation.thread_id()).unwrap();
    assert_eq!(record.state, RunState::Awaiting);
    let ticket = store
        .resume_ticket(&record.id)
        .expect("ticket not consumed");
    assert_eq!(ticket.run_id, record.id);
    assert_eq!(fixture.hits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 1);

    // O0 the original identity is recoverable: resume the same Run once.
    let resumed = conversation
        .resume_with_id_async(record.id.clone(), "echo", |seen| {
            assert_eq!(seen, ticket);
            std::future::ready(Ok(ResumeResult::allow()))
        })
        .await
        .unwrap();
    assert_eq!(*resumed.run_id(), record.id, "O0 same run_id");
    assert_eq!(resumed.end_cause(), Some(&EndCause::NaturalEnd));
    assert_eq!(
        fixture.hits.load(Ordering::SeqCst),
        1,
        "exactly one tool hit"
    );
    assert!(store.resume_ticket(&record.id).is_none(), "ticket consumed");
    assert_eq!(fixture.model.calls.load(Ordering::SeqCst), 2);
}

/// Fails exactly the `fail_at`-th commit (1-based) and delegates the rest.
struct FailNthCommit {
    inner: Arc<MemoryCommitCoordinator>,
    fail_at: u64,
    seen: AtomicU64,
}
#[async_trait::async_trait]
impl CommitCoordinator for FailNthCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, CommitError> {
        if self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.fail_at {
            return Err(CommitError::Rejected(format!(
                "injected failure of commit {}",
                self.fail_at
            )));
        }
        self.inner.commit(commit).await
    }
}

struct FailureObservation {
    is_err: bool,
    persisted: Option<RunState>,
    durable_commits: u64,
    model_calls: usize,
    hits: usize,
}

/// Production entrypoint over a store whose `fail_at`-th commit fails.
async fn run_with_failing_commit(fail_at: u64) -> FailureObservation {
    let fixture = fixture(None);
    let inner = memory();
    let commit = Arc::new(FailNthCommit {
        inner: inner.clone(),
        fail_at,
        seen: AtomicU64::new(0),
    });
    let result = fixture
        .runtime
        .conversation(
            &fixture.agent,
            "commit-failure",
            RuntimeRunContext::new()
                .with_commit(commit)
                .with_reader(inner.clone()),
        )
        .unwrap()
        .run("echo")
        .await;
    FailureObservation {
        is_err: result.is_err(),
        persisted: inner
            .latest_run(&awaken_agent_contract::agent::thread::Id(
                "commit-failure".into(),
            ))
            .map(|record| record.state),
        durable_commits: inner.commit_count(),
        model_calls: fixture.model.calls.load(Ordering::SeqCst),
        hits: fixture.hits.load(Ordering::SeqCst),
    }
}

/// NC mutant: reports a connection failure for commit 1 as an error, but only
/// after running the whole Run on a healthy store.
async fn mutant_reporting_failure_after_the_effect() -> FailureObservation {
    let fixture = fixture(None);
    let store = memory();
    let outcome = fixture
        .runtime
        .conversation(
            &fixture.agent,
            "commit-failure",
            RuntimeRunContext::new().with_store(store.clone()),
        )
        .unwrap()
        .run("echo")
        .await
        .unwrap();
    assert_eq!(outcome.end_cause(), Some(&EndCause::NaturalEnd));
    let result: Result<RunOutcome, &str> = Err("connection lost");
    FailureObservation {
        is_err: result.is_err(),
        persisted: store
            .latest_run(&awaken_agent_contract::agent::thread::Id(
                "commit-failure".into(),
            ))
            .map(|record| record.state),
        durable_commits: store.commit_count(),
        model_calls: fixture.model.calls.load(Ordering::SeqCst),
        hits: fixture.hits.load(Ordering::SeqCst),
    }
}

/// The oracle the NC calls insufficient.
fn is_err_only(observed: &FailureObservation) -> bool {
    observed.is_err
}

/// T03-C03 O0/O1 for commit N failure: an error, nothing past commit N-1 is
/// durable, the persisted state is never a natural end, and no effect beyond
/// what N-1 commits admit (commit 1 failing admits no model call at all).
fn commit_failure_violations(observed: &FailureObservation, fail_at: u64) -> Vec<String> {
    let mut violations = Vec::new();
    if !observed.is_err {
        violations.push("reported success".to_string());
    }
    if observed.persisted == Some(RunState::Ended(EndCause::NaturalEnd)) {
        violations.push("natural end persisted".to_string());
    }
    if observed.durable_commits != fail_at - 1 {
        violations.push(format!("{} durable commits", observed.durable_commits));
    }
    if fail_at == 1 && observed.model_calls != 0 {
        violations.push(format!(
            "{} model calls before admission",
            observed.model_calls
        ));
    }
    violations
}

#[tokio::test]
async fn t03_c03_commit_n_failure_is_an_error_and_never_a_natural_end() {
    // The healthy Run's commit count bounds N.
    let healthy = run_with_failing_commit(u64::MAX).await;
    assert!(!healthy.is_err);
    assert_eq!(
        healthy.persisted,
        Some(RunState::Ended(EndCause::NaturalEnd))
    );
    assert_eq!(healthy.hits, 1);
    let total = healthy.durable_commits;
    assert!(
        total >= 2,
        "a tool Run commits at admission and at the end: {total}"
    );
    for fail_at in 1..=total {
        let observed = run_with_failing_commit(fail_at).await;
        assert_eq!(
            commit_failure_violations(&observed, fail_at),
            Vec::<String>::new(),
            "commit {fail_at} of {total}: persisted={:?} calls={} hits={}",
            observed.persisted,
            observed.model_calls,
            observed.hits
        );
    }
    let first = run_with_failing_commit(1).await;
    assert_eq!(
        (first.persisted, first.model_calls, first.hits),
        (None, 0, 0)
    );
}

#[tokio::test]
async fn t03_c03_nc_is_err_only_accepts_a_mutant_that_still_invokes_the_model() {
    let production = run_with_failing_commit(1).await;
    let mutant = mutant_reporting_failure_after_the_effect().await;
    assert!(
        is_err_only(&production) && is_err_only(&mutant),
        "is_err cannot tell them apart"
    );
    assert_eq!(
        commit_failure_violations(&production, 1),
        Vec::<String>::new()
    );
    let violations = commit_failure_violations(&mutant, 1);
    assert!(
        violations.contains(&"natural end persisted".to_string()),
        "{violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.ends_with("model calls before admission")),
        "{violations:?}"
    );
    assert_eq!(mutant.hits, 1);
}
