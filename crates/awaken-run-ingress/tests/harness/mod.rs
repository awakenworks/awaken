//! Shared runtime harness for the durable-ingress integration tests.
//!
//! It builds a real `Runtime` with deterministic model providers — a plain text
//! model whose runs end naturally, and a tool-then-text model that parks on a
//! gate so the resume path can be driven. The commit coordinator and dispatch
//! store are supplied by each test (in-memory or Postgres), so this harness is
//! storage-agnostic and shared by every suite.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::{PersistenceMode, RunActivation, RunOptions};
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, ChatRole, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, PermissionContext, ToolGateHook};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::resume::ResumeResult;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

pub const FP: &str = "catalog-a";
pub const SNAP: &str = "snapshot-1";
pub const THREAD: &str = "thread-1";
pub const TICKET: &str = "ticket-1";

/// Always answers with fixed text — a fresh run ends naturally in one step.
struct TextLlm(&'static str);
#[async_trait::async_trait]
impl LlmExecutor for TextLlm {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        Ok(ChatResponse {
            output: AssistantOutput::text(self.0.to_string()),
            usage: None,
        })
    }
}

/// Calls `echo` once, then ends with text — drives the park/resume path.
struct ToolThenText {
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl LlmExecutor for ToolThenText {
    async fn infer(&self, _r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "call-1".to_string(),
                tool_id: "echo".to_string(),
                arguments: serde_json::json!({"text": "ping"}),
            }])
        } else {
            AssistantOutput::text("all done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
        })
    }
}

struct EchoTool {
    ran: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl RawTool for EchoTool {
    fn id(&self) -> &str {
        "echo"
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, "echoed: ping"))
    }
}

struct SuspendGate;
#[async_trait::async_trait]
impl ToolGateHook for SuspendGate {
    async fn gate(&self, _c: &PermissionContext) -> GateOutcome {
        GateOutcome::Suspend {
            ticket_id: TICKET.to_string(),
        }
    }
}

/// Defers the tool call as a ScheduledAction instead of running it inline.
struct ScheduleGate;
#[async_trait::async_trait]
impl ToolGateHook for ScheduleGate {
    async fn gate(&self, _c: &PermissionContext) -> GateOutcome {
        GateOutcome::Schedule {
            correlation_id: TICKET.to_string(),
        }
    }
}

pub fn snapshot() -> ExecutableAgentSnapshot {
    let fp = CatalogFingerprint(FP.to_string());
    ExecutableAgentSnapshot {
        id: ExecutableAgentSnapshotId(SNAP.to_string()),
        root_agent_id: AgentId("agent-1".to_string()),
        resolved_spec: ResolvedSpec {
            catalog_fingerprint: fp.clone(),
            instructions: String::new(),
            max_steps: 16,
            model_binding: ModelBinding {
                provider_instance_ref: "p".to_string(),
                model_ref: "m".to_string(),
                backend_ref: "b".to_string(),
            },
            tool_descriptors: vec![ToolDescriptor::pinned(
                "test",
                "echo",
                "Echo",
                serde_json::json!({"type": "object"}),
            )],
            plugin_ids: Vec::new(),
        },
        fingerprint: fp,
    }
}

fn install(runtime: &Runtime) {
    let fp = CatalogFingerprint(FP.to_string());
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "pub-1".to_string(),
            fingerprint: fp.clone(),
            source_revisions: vec!["rev-1".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fp,
                runtime_version: "test".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("installs");
    runtime.register_snapshot(snapshot());
}

/// A runtime with a plain text model — fresh runs end naturally.
pub fn text_runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(TextLlm("done"))));
    install(&runtime);
    runtime
}

/// Echoes the run's user-message text back as the assistant reply, so a test can
/// observe exactly which input reached the model.
struct EchoInputLlm;
#[async_trait::async_trait]
impl LlmExecutor for EchoInputLlm {
    async fn infer(&self, r: ChatRequest) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let echoed = r
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
            output: AssistantOutput::text(echoed),
            usage: None,
        })
    }
}

/// A runtime whose model echoes the user input it received — a probe for which
/// messages actually reached the run.
pub fn input_echo_runtime() -> Arc<Runtime> {
    let runtime = Arc::new(Runtime::new().with_llm(Arc::new(EchoInputLlm)));
    install(&runtime);
    runtime
}

/// A runtime that parks on a tool gate, exposing the tool-run counter so a test
/// can assert the pending tool runs exactly once on resume.
pub fn tool_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(SuspendGate)),
    );
    install(&runtime);
    (runtime, ran)
}

/// A runtime whose gate defers the tool call as a ScheduledAction (ADR-0020),
/// exposing the tool-run counter so a test can assert the deferred action runs.
pub fn schedule_runtime() -> (Arc<Runtime>, Arc<AtomicUsize>) {
    let ran = Arc::new(AtomicUsize::new(0));
    let runtime = Arc::new(
        Runtime::new()
            .with_llm(Arc::new(ToolThenText {
                calls: AtomicUsize::new(0),
            }))
            .with_tool(Arc::new(EchoTool { ran: ran.clone() }))
            .with_gate(Arc::new(ScheduleGate)),
    );
    install(&runtime);
    (runtime, ran)
}

/// Build a pending input for the test thread. The one place the `PendingInput`
/// shape lives, so each suite's convenience builder delegates here.
pub fn pending(
    message_id: &str,
    run: &str,
    correlation: &str,
    result: ResumeResult,
) -> awaken_run_ingress::PendingInput {
    awaken_run_ingress::PendingInput {
        message_id: message_id.to_string(),
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        correlation_id: correlation.to_string(),
        available_at_ms: None,
        result,
    }
}

pub fn activation(run: &str) -> RunActivation {
    RunActivation {
        run_id: RunId(run.to_string()),
        thread_id: ThreadId(THREAD.to_string()),
        snapshot: snapshot(),
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        options: RunOptions {
            persistence: PersistenceMode::ReadWrite,
        },
        trace: Default::default(),
    }
}

// --- Live Postgres helpers, shared by the live suites -----------------------

use sqlx::postgres::PgPool;

/// The test database URL: `AWAKEN_TEST_DATABASE_URL`, or the local dev container.
pub fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://oversight:oversight@127.0.0.1:32771/awaken_store_test".to_string()
    })
}

/// Connect, or return `None` with a skip notice when no Postgres is reachable so
/// the suite still passes on a machine without a database.
pub async fn pool() -> Option<PgPool> {
    match PgPool::connect(&database_url()).await {
        Ok(pool) => Some(pool),
        Err(err) => {
            println!("[skip] no Postgres reachable: {err}");
            None
        }
    }
}

/// Shared spec for revision-guarded pending edit/retract (M3a): every backend
/// must match this behaviour, so the test body lives here once.
pub async fn assert_pending_revision_cas<S: awaken_run_ingress::PendingInbox>(store: &S) {
    use awaken_run_ingress::{CasOutcome, PendingInput};
    let thread = ThreadId(THREAD.to_string());
    let input = |result| PendingInput {
        message_id: "m1".to_string(),
        run_id: RunId("run-1".to_string()),
        thread_id: thread.clone(),
        correlation_id: TICKET.to_string(),
        available_at_ms: None,
        result,
    };
    store
        .append(input(ResumeResult::Input("a".to_string())))
        .await
        .unwrap();

    let records = store.list(&thread).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].revision, 1);

    // A stale-revision edit is rejected; the correct revision applies and bumps.
    assert_eq!(
        store
            .edit("m1", 99, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(
        store
            .edit("m1", 1, ResumeResult::Input("b".to_string()))
            .await
            .unwrap(),
        CasOutcome::Applied
    );
    let records = store.list(&thread).await.unwrap();
    assert_eq!(records[0].revision, 2);
    assert_eq!(
        records[0].input.result,
        ResumeResult::Input("b".to_string())
    );

    // Retract is likewise guarded; a stale revision fails, the current one wins.
    assert_eq!(
        store.retract("m1", 1).await.unwrap(),
        CasOutcome::RevisionMismatch
    );
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::Applied);
    assert_eq!(store.retract("m1", 2).await.unwrap(), CasOutcome::NotFound);
    assert!(store.list(&thread).await.unwrap().is_empty());
}

/// Shared spec for the cross-thread outbox + relay (M3b): every backend must
/// match. A staged delivery is not visible as pending until relayed; relay is
/// idempotent and moves it to the *target* thread's pending input.
pub async fn assert_cross_thread_outbox<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::PendingInput;
    let target = ThreadId("thread-2".to_string());
    let input = PendingInput {
        message_id: "x1".to_string(),
        run_id: RunId("run-2".to_string()),
        thread_id: target.clone(),
        correlation_id: "c2".to_string(),
        available_at_ms: None,
        result: ResumeResult::Input("hi".to_string()),
    };

    // Staging is idempotent and does not yet appear as pending on the target.
    assert!(store.stage(input.clone()).await.unwrap());
    assert!(!store.stage(input.clone()).await.unwrap());
    assert!(store.list(&target).await.unwrap().is_empty());

    // Relay moves it to the target thread's pending input, and is then drained.
    assert_eq!(store.relay().await.unwrap(), 1);
    let records = store.list(&target).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].input.message_id, "x1");
    assert_eq!(store.relay().await.unwrap(), 0, "the outbox was drained");
}

/// Shared spec for scheduled delivery (M4): a future-dated pending input is not
/// claimable until its time has come; every backend must gate the wake the same.
pub async fn assert_scheduled_due<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, PendingInput, RunExecutionRequest};
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    // Claim the fresh run, then park it so it can be woken by a delivery.
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    store
        .settle(&run, DispatchOutcome::Parked, &[])
        .await
        .unwrap();

    // Schedule a delivery for t=1000.
    store
        .append(PendingInput {
            message_id: "sched".to_string(),
            run_id: run.clone(),
            thread_id: ThreadId(THREAD.to_string()),
            correlation_id: TICKET.to_string(),
            available_at_ms: Some(1_000),
            result: ResumeResult::Decision {
                allow: true,
                note: None,
            },
        })
        .await
        .unwrap();

    // Before its time, the run is not claimable; at its time, it wakes with the
    // now-due input in hand.
    assert!(
        store.claim("w", 1_000, 500).await.unwrap().is_none(),
        "a future delivery is not yet claimable"
    );
    let claimed = store
        .claim("w", 1_000, 1_000)
        .await
        .unwrap()
        .expect("a due delivery is claimable");
    assert_eq!(claimed.pending.len(), 1);
    assert_eq!(claimed.pending[0].message_id, "sched");
}

/// Shared spec for the crash-retry budget and dead-letter (M5): a run reclaimed
/// past its budget is dead-lettered and no longer claimed, and `requeue` brings
/// it back. Every backend must match.
pub async fn assert_dead_letter<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();

    // A fresh claim does not spend the budget; each later recovery (expired
    // lease) does. With max_attempts = 2, two recoveries exhaust it.
    assert!(store.claim("w", 100, 0).await.unwrap().is_some());
    assert_eq!(store.reap(2, 200).await.unwrap(), 0, "still within budget");
    assert!(store.claim("w", 100, 200).await.unwrap().is_some());
    assert_eq!(store.reap(2, 400).await.unwrap(), 0);
    assert!(store.claim("w", 100, 400).await.unwrap().is_some());

    // Budget exhausted: reap dead-letters it; it is no longer claimable.
    assert_eq!(store.reap(2, 600).await.unwrap(), 1, "dead-lettered");
    assert!(
        store.claim("w", 100, 700).await.unwrap().is_none(),
        "a dead-lettered run is not claimed"
    );
    assert_eq!(store.dead_letters().await.unwrap(), vec![run.clone()]);

    // Requeue restores it to a fresh budget.
    assert!(store.requeue(&run).await.unwrap());
    assert!(store.dead_letters().await.unwrap().is_empty());
    assert!(
        store.claim("w", 100, 800).await.unwrap().is_some(),
        "a requeued run is claimable again"
    );
}

/// Shared spec for durable cancel: a pending or parked dispatch is cancellable
/// (returns its thread id and is removed); a running one is not. Every backend
/// must match.
pub async fn assert_cancel<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let thread = Some(ThreadId(THREAD.to_string()));

    // A pending run is cancellable and then gone.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        thread
    );
    assert!(store.claim("w", 100, 0).await.unwrap().is_none());
    // Cancelling an unknown run is a no-op.
    assert_eq!(
        store.cancel(&RunId("run-1".to_string())).await.unwrap(),
        None
    );

    // A running run is not durably cancellable (use live control instead).
    store
        .enqueue(RunExecutionRequest::new(activation("run-2")))
        .await
        .unwrap();
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    assert_eq!(
        store.cancel(&RunId("run-2".to_string())).await.unwrap(),
        None,
        "a running run is not durably cancelled"
    );

    // A parked run on a thread is resolvable by thread (send_message addressing).
    let thread_id = ThreadId(THREAD.to_string());
    assert!(store.parked_run(&thread_id).await.unwrap().is_none());
    store
        .enqueue(RunExecutionRequest::new(activation("run-3")))
        .await
        .unwrap();
    assert!(store.claim("w", 1_000, 0).await.unwrap().is_some());
    store
        .settle(
            &RunId("run-3".to_string()),
            awaken_run_ingress::DispatchOutcome::Parked,
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        store.parked_run(&thread_id).await.unwrap(),
        Some(RunId("run-3".to_string()))
    );
}

/// Shared spec for priority, dedupe, and dead-letter GC. Every backend matches.
pub async fn assert_priority_dedupe_gc<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest, SubmitOptions};
    let req = |id: &str| RunExecutionRequest::new(activation(id));

    // Priority: the higher-priority fresh run is claimed first.
    store
        .enqueue_with(req("low"), SubmitOptions::default())
        .await
        .unwrap();
    store
        .enqueue_with(
            req("high"),
            SubmitOptions {
                priority: 10,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "high"
    );
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "low"
    );
    store
        .settle(&RunId("high".to_string()), DispatchOutcome::Done, &[])
        .await
        .unwrap();
    store
        .settle(&RunId("low".to_string()), DispatchOutcome::Done, &[])
        .await
        .unwrap();

    // Dedupe: a second enqueue carrying a live dedupe key is a no-op.
    let key = SubmitOptions {
        dedupe_key: Some("k".to_string()),
        ..Default::default()
    };
    store.enqueue_with(req("d1"), key.clone()).await.unwrap();
    store.enqueue_with(req("d2"), key).await.unwrap();
    assert_eq!(
        store
            .claim("w", 1_000, 0)
            .await
            .unwrap()
            .unwrap()
            .request
            .run_id()
            .0,
        "d1"
    );
    assert!(
        store.claim("w", 1_000, 0).await.unwrap().is_none(),
        "the duplicate was not enqueued"
    );
    store
        .settle(&RunId("d1".to_string()), DispatchOutcome::Done, &[])
        .await
        .unwrap();

    // GC: a dead-lettered run is purged.
    store
        .enqueue_with(req("poison"), SubmitOptions::default())
        .await
        .unwrap();
    assert!(store.claim("w", 1, 0).await.unwrap().is_some());
    assert_eq!(store.reap(0, 100).await.unwrap(), 1);
    assert_eq!(
        store.dead_letters().await.unwrap(),
        vec![RunId("poison".to_string())]
    );
    assert_eq!(store.purge_dead_letters().await.unwrap(), 1);
    assert!(store.dead_letters().await.unwrap().is_empty());
}

/// Shared spec for the idle-thread inbox (ADR-0021): unbound input is listed for
/// its thread, and a Done settle that consumed it removes it. Every backend matches.
pub async fn assert_idle_thread_inbox<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::{DispatchOutcome, RunExecutionRequest};
    use awaken_runtime_contract::resume::ResumeResult;

    // Unbound idle-thread input (empty run/correlation) is listed for the thread.
    let unbound = pending("u1", "", "", ResumeResult::Input("hi".to_string()));
    assert!(store.append(unbound).await.unwrap());
    let listed = store.list(&ThreadId(THREAD.to_string())).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|r| r.input.message_id == "u1" && r.input.run_id.0.is_empty()),
        "the unbound input is listed for its thread"
    );

    // A fresh run drains it: a Done settle that consumed it removes it.
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    store.claim("w", 1_000, 0).await.unwrap();
    store
        .settle(
            &RunId("run-1".to_string()),
            DispatchOutcome::Done,
            &["u1".to_string()],
        )
        .await
        .unwrap();
    assert!(
        store
            .list(&ThreadId(THREAD.to_string()))
            .await
            .unwrap()
            .iter()
            .all(|r| r.input.message_id != "u1"),
        "the consumed unbound input is removed on Done"
    );
}

/// Shared spec for lease renewal (the multi-node liveness knob). A run's owner
/// extends its lease so another node's recovery cannot steal it; a non-owner
/// cannot renew; an un-renewed lease still expires. Every backend matches.
pub async fn assert_lease_renewal<S: awaken_run_ingress::DispatchStore>(store: &S) {
    use awaken_run_ingress::RunExecutionRequest;
    let run = RunId("run-1".to_string());
    store
        .enqueue(RunExecutionRequest::new(activation("run-1")))
        .await
        .unwrap();
    assert!(store.claim("owner-a", 100, 0).await.unwrap().is_some());

    // owner-a renews at t=50 (extends to 150); a recovery claim at t=120 cannot
    // steal it because the lease has not expired.
    assert!(store.renew_lease(&run, "owner-a", 100, 50).await.unwrap());
    assert!(
        store.claim("owner-b", 100, 120).await.unwrap().is_none(),
        "a renewed lease is not yet expired"
    );
    // A non-owner cannot renew.
    assert!(!store.renew_lease(&run, "owner-b", 100, 130).await.unwrap());

    // Once the renewed lease expires, recovery reclaims for the new owner.
    assert_eq!(
        store
            .claim("owner-b", 100, 200)
            .await
            .unwrap()
            .map(|c| c.lease.owner),
        Some("owner-b".to_string())
    );
}

/// Drop every commit- and dispatch-schema table for a prefix (plus the shared
/// migration ledger) so each test starts from a clean, isolated schema.
pub async fn reset(pool: &PgPool, prefix: &str) {
    for table in [
        "commit",
        "message",
        "state_command",
        "event",
        "run_record",
        "waiting",
        "dispatch",
        "pending",
        "outbox",
        "schema_migrations",
    ] {
        let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {prefix}_{table} CASCADE"))
            .execute(pool)
            .await;
    }
}
