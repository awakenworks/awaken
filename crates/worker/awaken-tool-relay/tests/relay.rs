//! ADR-0044 first vertical slice: prove the brain–hand split end to end over an
//! in-process duplex, plus the Indeterminate and idempotent-re-drive guarantees.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_runtime_contract::tool::{RawTool, ToolCall, ToolError, ToolExecutor, ToolOutput};
use awaken_tool_relay::wire::{HandError, HandErrorKind, HandReply, HandRequest, HandResult};
use awaken_tool_relay::{
    FsOperationLedger, HandOperationLedger, HandSession, LedgerAdmission, RemoteToolExecutor,
    serve_hand,
};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn create() -> Self {
        let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "awaken-tool-relay-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).expect("create ledger test directory");
        Self(path)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct TestOperationLedger {
    _directory: TestDirectory,
    inner: FsOperationLedger,
}

#[async_trait]
impl HandOperationLedger for TestOperationLedger {
    async fn begin(
        &self,
        operation_id: &str,
    ) -> Result<awaken_tool_relay::LedgerAdmission, String> {
        self.inner.begin(operation_id).await
    }

    async fn wait(&self, operation_id: &str) -> Result<Option<HandResult>, String> {
        self.inner.wait(operation_id).await
    }

    async fn complete(&self, operation_id: &str, result: &HandResult) -> Result<(), String> {
        self.inner.complete(operation_id, result).await
    }
}

struct BlockingEcho {
    runs: Arc<AtomicU32>,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl RawTool for BlockingEcho {
    fn id(&self) -> &str {
        "blocking_echo"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::ok(call.call_id, "joined"))
    }
}

#[tokio::test]
async fn a_reconnected_session_joins_the_live_operation_instead_of_replaying() {
    // Cause/effect graph for ADR-0073 resident-Hand rule H2:
    // C1 one resident Hand process shares one FsOperationLedger; C2 request A
    // has durably claimed operation O and is still executing; C3 a replacement
    // Worker connection sends O with a different transport correlation id.
    // E1 request B waits for A; E2 both receive the same result; E3 the tool
    // side effect count is exactly one. Constraint: the join exists only while
    // the same Hand process owns the in-flight map. FMECA control: this detects
    // the severity-5 duplicate-effect mode after Worker/channel loss; a claim
    // from a prior Hand process is covered separately by H4 and remains
    // Indeterminate.
    let directory = TestDirectory::create();
    let ledger: Arc<dyn HandOperationLedger> =
        Arc::new(FsOperationLedger::open(directory.path()).expect("open shared ledger"));
    let runs = Arc::new(AtomicU32::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(BlockingEcho {
        runs: runs.clone(),
        started: started.clone(),
        release: release.clone(),
    }) as Arc<dyn RawTool>;
    let request = HandRequest::new(1, call("call-live", "blocking_echo", "x"));
    let mut first = HandSession::new([tool.clone()], ledger.clone());
    let first_request = request.clone();
    let first_task = tokio::spawn(async move { first.handle(first_request).await });
    started.notified().await;

    let mut second = HandSession::new([tool], ledger);
    let mut retry = request;
    retry.correlation_id = 2;
    let second_task = tokio::spawn(async move { second.handle(retry).await });
    tokio::task::yield_now().await;
    assert!(!second_task.is_finished(), "H2/E1 joins the live execution");

    release.notify_one();
    let first_reply = first_task.await.expect("first session task");
    let second_reply = second_task.await.expect("replacement session task");
    assert_eq!(first_reply.result, second_reply.result, "H2/E2");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "H2/E3");
}

fn test_session(tools: impl IntoIterator<Item = Arc<dyn RawTool>>) -> HandSession {
    let directory = TestDirectory::create();
    let inner = FsOperationLedger::open(directory.path()).expect("open test operation ledger");
    HandSession::new(
        tools,
        Arc::new(TestOperationLedger {
            _directory: directory,
            inner,
        }),
    )
}

/// A tool that echoes its `text` argument and counts how many times it ran, so a
/// test can prove an effect ran exactly once.
struct CountingEcho {
    id: String,
    runs: Arc<AtomicU32>,
}

#[async_trait]
impl RawTool for CountingEcho {
    fn id(&self) -> &str {
        &self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        let text = call
            .arguments
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(ToolOutput::ok(&call.call_id, text))
    }
}

fn call(call_id: &str, tool_id: &str, text: &str) -> ToolCall {
    ToolCall {
        call_id: call_id.into(),
        tool_id: tool_id.into(),
        arguments: serde_json::json!({ "text": text }),
    }
}

#[tokio::test]
async fn remote_tool_runs_out_of_process_and_returns_output() {
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let session = test_session([tool as Arc<dyn RawTool>]);

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let output = executor
        .invoke(&call("c1", "echo", "hello from the hand"))
        .await
        .expect("remote invoke");

    assert_eq!(output.text(), "hello from the hand");
    assert!(!output.is_error);
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the effect ran exactly once"
    );

    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn concurrent_calls_serialize_and_never_cross_their_correlation() {
    // The executor guards its framed channel with a Mutex, so overlapping `invoke`s
    // from many tasks must each still receive THEIR OWN reply (request/reply pairing
    // never crosses under contention).
    let runs = Arc::new(AtomicU32::new(0));
    let session = test_session([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    }) as Arc<dyn RawTool>]);

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = Arc::new(RemoteToolExecutor::new(brain_end));
    let mut handles = Vec::new();
    for i in 0..8 {
        let ex = executor.clone();
        handles.push(tokio::spawn(async move {
            let text = format!("msg-{i}");
            let out = ex
                .invoke(&call(&format!("c{i}"), "echo", &text))
                .await
                .expect("concurrent invoke");
            (text, out.text())
        }));
    }
    for h in handles {
        let (sent, got) = h.await.unwrap();
        assert_eq!(
            got, sent,
            "each concurrent call received its own reply, not a crossed one"
        );
    }
    assert_eq!(
        runs.load(Ordering::SeqCst),
        8,
        "each effect ran exactly once"
    );

    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn unknown_tool_reads_identically_to_the_local_path() {
    let session = test_session(std::iter::empty::<Arc<dyn RawTool>>());
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let err = executor
        .invoke(&call("c1", "nope", "x"))
        .await
        .expect_err("unknown tool is an error");

    // Same display as the in-process `ToolError::Unknown`.
    assert_eq!(err.to_string(), "unknown tool: nope");
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn an_oversized_frame_fails_closed_at_encode_never_indeterminate() {
    // A call whose encoded frame exceeds the length-delimited codec's max is rejected
    // locally at encode — before it can reach the hand — so it is a DEFINITE error, not
    // an Indeterminate. The effect provably never ran (nothing was dispatched).
    let (brain_end, _hand_end) = tokio::io::duplex(64 * 1024);
    let executor = RemoteToolExecutor::new(brain_end);
    let huge = "x".repeat(9 * 1024 * 1024); // > the 8 MiB default max frame length
    let result = executor.call_hand(&call("c1", "echo", &huge)).await;
    assert!(
        matches!(result, HandResult::Err { .. }),
        "an oversized frame is a definite encode error, not Indeterminate: {result:?}"
    );
}

#[tokio::test]
async fn dropped_channel_after_dispatch_is_indeterminate() {
    // A hand that reads the request then vanishes without replying: the brain
    // cannot know whether the effect ran, so the outcome is Indeterminate.
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        use tokio_util::codec::{Framed, LengthDelimitedCodec};
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        let _ = framed.next().await; // consume the request, then drop the channel
    });

    let executor = RemoteToolExecutor::new(brain_end);
    let result = executor.call_hand(&call("c1", "echo", "x")).await;
    assert_eq!(result, HandResult::Indeterminate);
}

#[tokio::test]
async fn re_drive_with_same_correlation_id_runs_the_effect_at_most_once() {
    // Idempotency ledger (ADR-0044 D4): the second identical request returns the
    // recorded result without re-running the tool.
    let runs = Arc::new(AtomicU32::new(0));
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    });
    let mut session = test_session([tool as Arc<dyn RawTool>]);

    let request = HandRequest::new(42, call("c1", "echo", "once"));
    let first = session.handle(request.clone()).await;
    let second = session.handle(request).await;

    assert_eq!(first, second, "a re-drive returns the recorded reply");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the effect ran at most once"
    );
    assert!(matches!(first.result, HandResult::Ok { .. }));
}

#[tokio::test]
async fn different_transport_ids_with_one_operation_id_run_the_effect_once() {
    let runs = Arc::new(AtomicU32::new(0));
    let mut session = test_session([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    }) as Arc<dyn RawTool>]);

    let first = HandRequest::new(1, call("c1", "echo", "once"));
    let mut retry = first.clone();
    retry.correlation_id = 2;
    let first_reply = session.handle(first).await;
    let retry_reply = session.handle(retry).await;

    assert_eq!(first_reply.correlation_id, 1);
    assert_eq!(retry_reply.correlation_id, 2);
    assert_eq!(first_reply.result, retry_reply.result);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn filesystem_ledger_fences_replay_but_never_persists_tool_output() {
    // Cause/effect/FMECA graph for ADR-0073 rules H4 and H9: C1 a Hand claims
    // and completes operation O with secret-bearing output; C2 the same live
    // Hand receives O again; C3 that Hand dies and a replacement opens the
    // Environment-owned ledger. Effects: E1 C2 returns the process-local cached
    // result without replay; E2 durable files contain neither result nor secret;
    // E3 C3 is Indeterminate and never executes O. Decision rules:
    // D1=C1+C2+live-process -> E1; D2=C1+C3+prior-process -> E2+E3. This controls
    // both severity-5 duplicate effects and severity-5 credential/output
    // disclosure to another same-uid sandbox process.
    let directory = TestDirectory::create();
    let runs = Arc::new(AtomicU32::new(0));
    let tool = || {
        Arc::new(CountingEcho {
            id: "echo".into(),
            runs: runs.clone(),
        }) as Arc<dyn RawTool>
    };

    let mut first = HandSession::new(
        [tool()],
        Arc::new(FsOperationLedger::open(directory.path()).expect("open first ledger")),
    );
    let secret = "credential-secret-must-not-reach-disk"; // awaken-allow: secret
    let request = HandRequest::new(1, call("c1", "echo", secret));
    let first_reply = first.handle(request.clone()).await;
    let mut same_process_retry = request.clone();
    same_process_retry.correlation_id = 2;
    let same_process_reply = first.handle(same_process_retry).await;
    assert_eq!(first_reply.result, same_process_reply.result, "H9/E1");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "H9/E1 no replay");

    for entry in std::fs::read_dir(directory.path()).expect("read ledger directory") {
        let bytes = std::fs::read(entry.expect("ledger entry").path()).expect("read ledger file");
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(secret), "H9/E2 no secret-bearing output");
        assert!(!text.contains("ToolOutput"), "H9/E2 no serialized result");
    }
    drop(first);

    let mut restarted = HandSession::new(
        [tool()],
        Arc::new(FsOperationLedger::open(directory.path()).expect("reopen ledger")),
    );
    let mut retry = request;
    retry.correlation_id = 99;
    let retry_reply = restarted.handle(retry).await;

    assert_eq!(retry_reply.result, HandResult::Indeterminate, "H4/E3");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "H4/E3 restart did not re-run effect"
    );
}

#[tokio::test]
async fn a_prior_hand_claim_without_a_result_remains_indeterminate() {
    // Cause/effect graph for ADR-0073 rule H4: C1 the first Hand durably claims
    // operation O; C2 that Hand process disappears before a result is durable;
    // C3 another Hand opens the same ledger. E1 the replacement reports
    // Indeterminate and E2 it does not acquire Execute. Constraint: only a
    // process-local in-flight owner may produce H2/InFlight. FMECA mitigation:
    // this is the fail-closed control for Hand/Pod loss and prevents an
    // ambiguous severity-5 side effect from being replayed as Worker recovery.
    let directory = TestDirectory::create();
    let first = FsOperationLedger::open(directory.path()).expect("open first Hand ledger");
    assert_eq!(
        first.begin("operation-with-lost-hand").await.unwrap(),
        LedgerAdmission::Execute,
        "H4/C1"
    );
    drop(first);

    let replacement =
        FsOperationLedger::open(directory.path()).expect("open replacement Hand ledger");
    assert_eq!(
        replacement.begin("operation-with-lost-hand").await.unwrap(),
        LedgerAdmission::Indeterminate,
        "H4/E1+E2"
    );
}

#[tokio::test]
async fn failed_claim_persistence_never_leaves_a_phantom_in_flight_owner() {
    /*
     * Ledger rule H7 / FMECA persistence fault. Causes: C1 admission has an
     * otherwise new operation; C2 its ledger directory disappears before the
     * claim-file open; C3 the same live process retries. Effects: E1 both calls
     * return a definite pre-dispatch ledger error; E2 neither returns InFlight
     * or waits. Constraint: every open/write/sync failure follows the same
     * cleanup outcome, so the observable open failure proves the local join
     * reservation is not leaked.
     */
    let directory = TestDirectory::create();
    let ledger = FsOperationLedger::open(directory.path()).expect("open ledger");
    std::fs::remove_dir_all(directory.path()).expect("inject missing ledger root");

    let first = ledger.begin("claim-open-fault").await;
    let retry = ledger.begin("claim-open-fault").await;
    assert!(first.is_err(), "H7/E1 first admission");
    assert!(
        retry.is_err(),
        "H7/E1+E2 retry must not join a phantom owner"
    );
}

#[tokio::test]
async fn append_only_capacity_fails_new_operations_but_keeps_existing_results() {
    /*
     * Capacity rule H7. Causes: C1 append-only capacity is one; C2 operation A
     * has a durable claim/result; C3 new operation B arrives; C4 A is retried.
     * Effects: E1 B fails before dispatch; E2 A remains cached. Constraint: no
     * live claim/result is garbage-collected to manufacture capacity. This is
     * the inode-exhaustion FMECA control and preserves at-most-once semantics.
     */
    let directory = TestDirectory::create();
    let ledger =
        FsOperationLedger::open_with_max_entries(directory.path(), 1).expect("open bounded ledger");
    let result = HandResult::ok(ToolOutput::ok("a", "done"));
    assert_eq!(ledger.begin("a").await.unwrap(), LedgerAdmission::Execute);
    ledger.complete("a", &result).await.unwrap();

    let error = ledger.begin("b").await.expect_err("H7/E1 capacity");
    assert!(error.contains("capacity 1 is exhausted"));
    assert_eq!(
        ledger.begin("a").await.unwrap(),
        LedgerAdmission::Cached(result),
        "H7/E2"
    );
}

#[tokio::test]
async fn an_interrupted_hashed_claim_is_typed_indeterminate_not_ledger_unavailable() {
    /*
     * Interrupted-write rule H4. Causes: C1 a long identity uses a hashed stem;
     * C2 its durable claim exists but contains no complete identity after a
     * simulated interrupted write; C3 a replacement Hand admits the same id.
     * Effects: E1 typed Indeterminate; E2 never Execute. A non-empty mismatched
     * identity remains a collision error, so interruption cannot weaken digest
     * collision detection.
     */
    let directory = TestDirectory::create();
    let operation_id = "nested-operation:".repeat(20);
    let first = FsOperationLedger::open(directory.path()).unwrap();
    assert_eq!(
        first.begin(&operation_id).await.unwrap(),
        LedgerAdmission::Execute
    );
    drop(first);
    let claim = std::fs::read_dir(directory.path())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "claim")
        })
        .expect("hashed claim")
        .path();
    std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(claim)
        .unwrap();

    let replacement = FsOperationLedger::open(directory.path()).unwrap();
    assert_eq!(
        replacement.begin(&operation_id).await.unwrap(),
        LedgerAdmission::Indeterminate,
        "H4/E1+E2"
    );
}

#[tokio::test]
async fn an_in_flight_join_deadline_returns_indeterminate_without_replay() {
    /*
     * Join deadline rule H8. Causes: C1 operation A is live and blocked; C2 a
     * reconnect uses the same operation id; C3 its safety wait expires. Effects:
     * E1 reconnect returns Indeterminate; E2 side-effect invocation count stays
     * one; E3 the original owner can still complete. The waiter never cancels or
     * replays the effect.
     */
    let directory = TestDirectory::create();
    let ledger: Arc<dyn HandOperationLedger> =
        Arc::new(FsOperationLedger::open(directory.path()).unwrap());
    let runs = Arc::new(AtomicU32::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let tool = Arc::new(BlockingEcho {
        runs: runs.clone(),
        started: started.clone(),
        release: release.clone(),
    }) as Arc<dyn RawTool>;
    let request = HandRequest::new(1, call("deadline", "blocking_echo", "x"));
    let mut owner = HandSession::new([tool.clone()], ledger.clone());
    let owner_request = request.clone();
    let owner_task = tokio::spawn(async move { owner.handle(owner_request).await });
    started.notified().await;

    let mut waiter = HandSession::new([tool], ledger)
        .with_in_flight_wait_timeout(std::time::Duration::from_millis(10));
    let reply = waiter.handle(request).await;
    assert_eq!(reply.result, HandResult::Indeterminate, "H8/E1");
    assert_eq!(runs.load(Ordering::SeqCst), 1, "H8/E2");
    release.notify_one();
    assert!(
        matches!(owner_task.await.unwrap().result, HandResult::Ok { .. }),
        "H8/E3"
    );
}

#[tokio::test]
async fn filesystem_ledger_accepts_and_fences_a_long_nested_workflow_operation_identity() {
    let directory = TestDirectory::create();
    let ledger = FsOperationLedger::open(directory.path()).expect("open ledger");
    let operation_id = format!(
        "workflow-execution:state-entry:issue_{}:deliver:3:coding:tool-call-17",
        "nested".repeat(40)
    );
    let result = HandResult::Ok {
        output: awaken_runtime_contract::tool::ToolOutput::ok(
            "nested-workflow-call",
            "nested-hand-ok",
        ),
    };

    assert_eq!(
        ledger
            .begin(&operation_id)
            .await
            .expect("long identity is admitted"),
        LedgerAdmission::Execute
    );
    ledger
        .complete(&operation_id, &result)
        .await
        .expect("long identity completes durably");

    let restarted = FsOperationLedger::open(directory.path()).expect("reopen ledger");
    assert_eq!(
        ledger.begin(&operation_id).await.unwrap(),
        LedgerAdmission::Cached(result.clone()),
        "the live Hand retains the completed result"
    );

    assert_eq!(
        restarted
            .begin(&operation_id)
            .await
            .expect("long identity remains fenced after restart"),
        LedgerAdmission::Indeterminate
    );

    for entry in std::fs::read_dir(directory.path()).expect("read ledger directory") {
        let name = entry
            .expect("ledger entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(name.len() <= 255, "ledger filename must remain bounded");
    }
}

/// A tool whose own `invoke` returns an error — the hand reports it as an
/// execution `HandError`, and the brain maps it back to a `ToolError`.
struct FailingTool;

#[async_trait]
impl RawTool for FailingTool {
    fn id(&self) -> &str {
        "fail"
    }
    async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Execution("boom".to_string()))
    }
}

#[tokio::test]
async fn a_tool_execution_error_round_trips_as_a_tool_error() {
    let session = test_session([Arc::new(FailingTool) as Arc<dyn RawTool>]);
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end);
    let err = executor
        .invoke(&call("c1", "fail", "x"))
        .await
        .expect_err("execution error surfaces");
    assert!(err.to_string().contains("boom"), "got: {err}");
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn catalog_fingerprint_mismatch_fails_closed() {
    let tool = Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    });
    let mut session = test_session([tool as Arc<dyn RawTool>]).with_catalog_fingerprint("hand-v1");

    let mut request = HandRequest::new(1, call("c1", "echo", "x"));
    request.catalog_fingerprint = Some("run-v2".into());

    let reply = session.handle(request).await;
    match reply.result {
        HandResult::Err { error } => {
            assert_eq!(error.kind, HandErrorKind::FingerprintMismatch)
        }
        other => panic!("expected fingerprint mismatch, got {other:?}"),
    }
}

#[tokio::test]
async fn a_write_that_fails_before_dispatch_is_a_definite_error_not_indeterminate() {
    // The hand's read half is already gone, so the very first send fails: the call
    // never left, so it is a definite Execution error, never Indeterminate.
    let (brain_end, hand_end) = tokio::io::duplex(64);
    drop(hand_end);
    let executor = RemoteToolExecutor::new(brain_end);
    match executor.call_hand(&call("c1", "echo", "x")).await {
        HandResult::Err { error } => {
            assert_eq!(error.kind, HandErrorKind::Execution);
            assert!(
                error.message.contains("closed before dispatch"),
                "got: {}",
                error.message
            );
        }
        other => panic!("expected a definite pre-dispatch error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_reply_with_a_mismatched_correlation_id_is_indeterminate() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        if let Some(Ok(frame)) = framed.next().await {
            let req: HandRequest = serde_json::from_slice(&frame).unwrap();
            // A well-formed reply, but for the wrong correlation id → the brain
            // cannot match it and must treat the call as indeterminate.
            let reply = HandReply {
                correlation_id: req.correlation_id.wrapping_add(999),
                result: HandResult::ok(ToolOutput::ok("c1", "stale")),
            };
            let _ = framed
                .send(serde_json::to_vec(&reply).unwrap().into())
                .await;
        }
    });
    let executor = RemoteToolExecutor::new(brain_end);
    assert_eq!(
        executor.call_hand(&call("c1", "echo", "x")).await,
        HandResult::Indeterminate
    );
}

#[tokio::test]
async fn a_reply_frame_that_is_not_a_hand_reply_is_indeterminate() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        if framed.next().await.is_some() {
            // Garbage that does not decode to a HandReply → indeterminate.
            let _ = framed.send(b"not-a-hand-reply".to_vec().into()).await;
        }
    });
    let executor = RemoteToolExecutor::new(brain_end);
    assert_eq!(
        executor.call_hand(&call("c1", "echo", "x")).await,
        HandResult::Indeterminate
    );
}

#[tokio::test]
async fn the_brain_stamps_its_catalog_fingerprint_and_a_matching_hand_accepts() {
    let session = test_session([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    }) as Arc<dyn RawTool>])
    .with_catalog_fingerprint("v1");
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end).with_catalog_fingerprint("v1");
    let out = executor
        .invoke(&call("c1", "echo", "ok"))
        .await
        .expect("a matching fingerprint is accepted");
    assert_eq!(out.text(), "ok");
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn a_brain_fingerprint_drift_is_rejected_end_to_end() {
    let session = test_session([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: Arc::new(AtomicU32::new(0)),
    }) as Arc<dyn RawTool>])
    .with_catalog_fingerprint("hand-v1");
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    let executor = RemoteToolExecutor::new(brain_end).with_catalog_fingerprint("run-v2");
    let err = executor
        .invoke(&call("c1", "echo", "x"))
        .await
        .expect_err("a drifted fingerprint is rejected");
    assert!(
        err.to_string().contains("fingerprint mismatch"),
        "got: {err}"
    );
    drop(executor);
    let _ = hand.await;
}

#[tokio::test]
async fn invoke_maps_an_indeterminate_outcome_to_a_named_execution_error() {
    // `call_hand` returning Indeterminate is exercised elsewhere; this covers the
    // `ToolExecutor::invoke` *mapping* row: Indeterminate → ToolError::Execution
    // whose message names the tool and says the connection was lost. The kernel
    // loop sees a definite error string, never a silent success.
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        use tokio_util::codec::{Framed, LengthDelimitedCodec};
        let mut framed = Framed::new(hand_end, LengthDelimitedCodec::new());
        let _ = framed.next().await; // read the request, then drop without replying
    });
    let executor = RemoteToolExecutor::new(brain_end);
    let err = executor
        .invoke(&call("c1", "echo", "x"))
        .await
        .expect_err("an indeterminate outcome is surfaced as an error");
    let msg = err.to_string();
    assert!(msg.contains("indeterminate"), "got: {msg}");
    assert!(msg.contains("echo"), "the message names the tool: {msg}");
}

#[tokio::test]
async fn invoke_classifies_a_closed_pre_dispatch_channel_as_safe_to_reacquire() {
    /*
     * Cause/effect rule RPD1: C1 the peer closes before the framed request can
     * be written; E1 no tool effect ran and invoke returns the one typed
     * pre-dispatch-unavailable error. This is deliberately distinct from RPD2,
     * covered above, where the peer reads the request and drops the reply and
     * the result remains an ordinary indeterminate Execution error.
     */
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    drop(hand_end);
    let error = RemoteToolExecutor::new(brain_end)
        .invoke(&call("c1", "echo", "x"))
        .await
        .expect_err("a closed channel cannot accept the request");
    assert!(matches!(
        error,
        ToolError::UnavailableBeforeDispatch(ref message)
            if message == "hand channel closed before dispatch"
    ));
}

#[tokio::test]
async fn a_hand_fingerprint_with_an_unstamped_request_runs_permissively() {
    // The hand fails closed only when BOTH sides carry a fingerprint and they
    // differ (`if let (Some, Some)`). A request that omits its fingerprint runs —
    // this locks that documented permissive row so a future tightening is a
    // deliberate, test-visible change.
    let runs = Arc::new(AtomicU32::new(0));
    let mut session = test_session([Arc::new(CountingEcho {
        id: "echo".into(),
        runs: runs.clone(),
    }) as Arc<dyn RawTool>])
    .with_catalog_fingerprint("hand-v1");

    // HandRequest::new leaves catalog_fingerprint = None.
    let reply = session
        .handle(HandRequest::new(7, call("c1", "echo", "ok")))
        .await;
    match reply.result {
        HandResult::Ok { output } => assert_eq!(output.text(), "ok"),
        other => panic!("an unstamped request should run, got {other:?}"),
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_re_drive_of_a_failed_execution_returns_the_cached_error_without_re_running() {
    // The idempotency ledger caches *errors* too: a second identical request to a
    // failing tool returns the recorded HandError without invoking the tool again.
    struct CountingFail {
        runs: Arc<AtomicU32>,
    }
    #[async_trait]
    impl RawTool for CountingFail {
        fn id(&self) -> &str {
            "fail"
        }
        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Err(ToolError::Execution("boom".to_string()))
        }
    }
    let runs = Arc::new(AtomicU32::new(0));
    let mut session =
        test_session([Arc::new(CountingFail { runs: runs.clone() }) as Arc<dyn RawTool>]);

    let request = HandRequest::new(99, call("c1", "fail", "x"));
    let first = session.handle(request.clone()).await;
    let second = session.handle(request).await;

    assert_eq!(first, second, "a re-drive returns the recorded error reply");
    assert!(matches!(first.result, HandResult::Err { .. }));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the failing effect ran at most once"
    );
}

#[test]
fn hand_result_serializes_with_an_internally_tagged_snake_case_status() {
    // Cause/effect graph: C1=Ok with one text ContentBlock; C2=typed Hand error;
    // C3=indeterminate transport outcome. Effects: E1=snake_case status plus the
    // lossless ContentBlock array; E2=typed error payload; E3=tag only. Decision
    // rows R1/R2/R3 below lock the canonical ToolOutput wire rather than reviving
    // the retired scalar-content compatibility shape.
    let expected_ok = HandResult::ok(ToolOutput::ok("c1", "hi"));
    let ok = serde_json::to_value(&expected_ok).unwrap();
    assert_eq!(ok["status"], "ok", "R1 status");
    assert_eq!(
        ok["output"]["content"],
        serde_json::json!([{"type": "text", "text": "hi"}]),
        "R1 content"
    );
    assert_eq!(
        serde_json::from_value::<HandResult>(ok).unwrap(),
        expected_ok,
        "the brain-hand wire must round-trip structured tool output"
    );

    let err = serde_json::to_value(HandResult::err(HandError::new(
        HandErrorKind::FingerprintMismatch,
        "drift",
    )))
    .unwrap();
    assert_eq!(err["status"], "err", "R2 status");
    assert_eq!(err["error"]["kind"], "fingerprint_mismatch", "R2 kind");
    assert_eq!(err["error"]["message"], "drift", "R2 message");

    let ind = serde_json::to_value(HandResult::Indeterminate).unwrap();
    assert_eq!(ind["status"], "indeterminate", "R3 status");
    // A unit-like variant carries no payload key beyond the tag.
    assert_eq!(ind.as_object().unwrap().len(), 1, "R3 payload");
}

#[test]
fn hand_request_omits_absent_optionals_and_defaults_them_on_decode() {
    // On the wire, an absent fingerprint/deadline are dropped (skip_serializing_if)
    // and default back to None on decode (#[serde(default)]) — so a minimal
    // producer and this struct interoperate.
    let req = HandRequest::new(1, call("c1", "echo", "x"));
    let json = serde_json::to_value(&req).unwrap();
    assert!(
        !json
            .as_object()
            .unwrap()
            .contains_key("catalog_fingerprint"),
        "an absent fingerprint is not serialized"
    );
    assert!(!json.as_object().unwrap().contains_key("deadline_unix_ms"));

    // A minimal frame with only the required fields round-trips to None optionals.
    let minimal = serde_json::json!({
        "correlation_id": 5,
        "call": { "call_id": "c1", "tool_id": "echo", "arguments": { "text": "x" } }
    });
    let decoded: HandRequest = serde_json::from_value(minimal).unwrap();
    assert_eq!(decoded.correlation_id, 5);
    assert!(decoded.operation_id.is_empty());
    assert_eq!(decoded.catalog_fingerprint, None);
    assert_eq!(decoded.deadline_unix_ms, None);
    assert_eq!(decoded.call.tool_id, "echo");
}

#[tokio::test]
async fn serve_hand_fails_closed_on_a_frame_that_is_not_a_request() {
    use futures_util::SinkExt;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let session = test_session(std::iter::empty::<Arc<dyn RawTool>>());
    let (brain_end, hand_end) = tokio::io::duplex(64 * 1024);
    let hand = tokio::spawn(serve_hand(hand_end, session));

    // A well-framed byte blob that does not decode to a HandRequest.
    let mut framed = Framed::new(brain_end, LengthDelimitedCodec::new());
    framed.send(b"garbage-frame".to_vec().into()).await.unwrap();

    match hand.await.unwrap() {
        Err(e) => assert!(e.to_string().contains("not a HandRequest"), "got: {e}"),
        Ok(()) => panic!("expected the hand to fail closed on a non-request frame"),
    }
}
