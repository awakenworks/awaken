//! Tool-produced state side-effects across *multiple* tool calls in one Step.
//!
//! Ported (adapted) from the reference `tool_side_effects.rs`. The reference's
//! `ToolOutput::with_command` + `StateKey`/merge-strategy model maps here onto
//! `ToolOutput::with_state(Vec<StateCommand>)` + `MergePolicy`. Behaviors covered:
//!   * a tool that stages no state commits nothing extra (empty command);
//!   * two tool calls in one Step each staging a `Commutative` write to the same
//!     key both commit and shallow-merge;
//!   * two tool calls each staging an `Exclusive` write to the same key conflict
//!     and fail closed (no partial commit) — the batch validated across calls.
//!
//! Current's `state.rs` already covers the single-tool commit/replay and the
//! single-tool exclusive conflict; these exercise the cross-tool-call path.

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command as StateCommand, Key, MergePolicy, Scope};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::permission::{GateOutcome, ToolGateHook};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, IdBound, Plugin, PluginManifest, ToolConcurrencyConstraint,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{
    RawTool, ToolConcurrency, ToolError, ToolOutput, ToolResource, ToolResourceAccess,
};
use awaken_store_inmem::{MemoryCommitCoordinator, replay_state};
use tokio::sync::Barrier;

/// Emits a single Step with the given tool calls, then ends with text.
struct CallsThenEnd {
    calls: std::sync::Mutex<Option<Vec<ToolCall>>>,
    seen: AtomicUsize,
}

impl CallsThenEnd {
    fn new(calls: Vec<ToolCall>) -> Self {
        Self {
            calls: std::sync::Mutex::new(Some(calls)),
            seen: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for CallsThenEnd {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            let calls = self.calls.lock().unwrap().take().unwrap_or_default();
            AssistantOutput::from_tool_calls(calls)
        } else {
            AssistantOutput::text("done".to_string())
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

/// A tool with a fixed id that stages the state commands it was built with.
struct FixedTool {
    id: &'static str,
    state: Vec<StateCommand>,
    concurrency: ToolConcurrency,
}

/// Shared observation point used to prove whether two executor futures overlap.
/// A delay, rather than a barrier, keeps the serial expectation from deadlocking.
#[derive(Default)]
struct ConcurrencyProbe {
    active: AtomicUsize,
    maximum_active: AtomicUsize,
}

struct ObservedTool {
    id: &'static str,
    probe: Arc<ConcurrencyProbe>,
    concurrency: Option<ToolConcurrency>,
    delay: Duration,
    barrier: Option<Arc<Barrier>>,
}

#[async_trait::async_trait]
impl RawTool for FixedTool {
    fn id(&self) -> &str {
        self.id
    }
    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        self.concurrency.clone()
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, "noted").with_state(self.state.clone()))
    }
}

#[async_trait::async_trait]
impl RawTool for ObservedTool {
    fn id(&self) -> &str {
        self.id
    }

    fn concurrency(&self, _arguments: &serde_json::Value) -> ToolConcurrency {
        self.concurrency.clone().unwrap_or_default()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.probe
            .maximum_active
            .fetch_max(active, Ordering::SeqCst);
        if let Some(barrier) = &self.barrier {
            barrier.wait().await;
        }
        tokio::time::sleep(self.delay).await;
        self.probe.active.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, self.id))
    }
}

struct AllowGate;

#[derive(Clone, Copy)]
enum ImmediateDecision {
    Block,
    SetResult,
}

struct ImmediateGate {
    tool_id: &'static str,
    decision: ImmediateDecision,
}

struct CountedTool {
    id: &'static str,
    calls: Arc<AtomicUsize>,
}

struct ArgumentResourceConstraint;

impl ToolConcurrencyConstraint for ArgumentResourceConstraint {
    fn id(&self) -> &str {
        "argument-resource"
    }

    fn constrain(&self, call: &ToolCall) -> ToolConcurrency {
        call.arguments
            .get("resource")
            .and_then(serde_json::Value::as_str)
            .map_or(ToolConcurrency::Parallel, |key| {
                ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource {
                    namespace: "plugin-test".to_string(),
                    key: key.to_string(),
                })])
            })
    }
}

struct ConstraintPlugin;

impl Plugin for ConstraintPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "constraint-plugin".to_string(),
            bound: CapabilityBound {
                tool_constraints: IdBound::Exact(vec!["argument-resource".to_string()]),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("constraint-plugin");
        contributions.register_tool_constraint(Arc::new(ArgumentResourceConstraint));
        contributions
    }
}

#[async_trait::async_trait]
impl ToolGateHook for AllowGate {
    async fn gate(
        &self,
        _ctx: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        GateOutcome::Allow
    }
}

#[async_trait::async_trait]
impl ToolGateHook for ImmediateGate {
    async fn gate(
        &self,
        call: &ToolCall,
        _state: &awaken_agent_contract::agent::state::Store,
    ) -> GateOutcome {
        if call.tool_id != self.tool_id {
            return GateOutcome::Allow;
        }
        match self.decision {
            ImmediateDecision::Block => GateOutcome::Block {
                reason: "policy denied the call".to_string(),
            },
            ImmediateDecision::SetResult => {
                GateOutcome::SetResult(ToolOutput::ok(&call.call_id, "provided by gate"))
            }
        }
    }
}

#[async_trait::async_trait]
impl RawTool for CountedTool {
    fn id(&self) -> &str {
        self.id
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(call.call_id, self.id))
    }
}

fn activation(tool_ids: &[&str]) -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
    let tool_descriptors = tool_ids
        .iter()
        .map(|id| {
            ToolDescriptor::pinned(
                "test",
                *id,
                "stage state",
                serde_json::json!({"type": "object"}),
            )
        })
        .collect();
    RunActivation {
        run_id: RunId("run-1".to_string()),
        thread_id: ThreadId("thread-1".to_string()),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-1".to_string()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-1".to_string()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: fingerprint.clone(),
                instructions: String::new(),
                max_steps: 16,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "p".to_string(),
                        model_ref: "m".to_string(),
                        backend_ref: "b".to_string(),
                    },
                ),
                tool_descriptors,
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("m1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

/// A tool that stages nothing must not add any state to the commit.
#[tokio::test]
async fn tool_with_no_state_commits_nothing_extra() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![ToolCall {
            call_id: "c1".to_string(),
            tool_id: "plain".to_string(),
            arguments: serde_json::json!({}),
        }])))
        .with_tool(Arc::new(FixedTool {
            id: "plain",
            state: vec![],
            concurrency: ToolConcurrency::Serial,
        }))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["plain"]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert!(
        committed
            .state
            .iter()
            .all(|command| command.key.0 == "runtime.active_tool_batch.v1"),
        "a tool staging no commands must commit only runtime recovery state, got {:?}",
        committed.state
    );
}

/// Decision table:
///
/// | C1: two ordinary calls | C2: no concurrency contract | Effect |
/// |------------------------|-----------------------------|--------|
/// | true                   | true                        | maximum active executor count is two |
///
/// Ordinary `RawTool`s include external/MCP-style tools. They use the largest
/// execution wave unless a tool explicitly narrows concurrency.
#[tokio::test]
async fn ordinary_tools_default_to_maximum_parallelism() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let barrier = Arc::new(Barrier::new(2));
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "observed_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "observed_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(ObservedTool {
            id: "observed_a",
            probe: probe.clone(),
            concurrency: None,
            delay: Duration::from_millis(20),
            barrier: Some(barrier.clone()),
        }))
        .with_tool(Arc::new(ObservedTool {
            id: "observed_b",
            probe: probe.clone(),
            concurrency: None,
            delay: Duration::from_millis(20),
            barrier: Some(barrier),
        }))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = runtime
        .execute(
            activation(&["observed_a", "observed_b"]),
            RuntimeRunContext::new().with_commit(commit),
        )
        .await
        .expect("ordinary tool batch completes");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(
        probe.maximum_active.load(Ordering::SeqCst),
        2,
        "ordinary tools without an explicit restriction must overlap"
    );
}

/// Cause/effect decision table for a mixed parallel wave:
///
/// | Gate result for call 1 | Call 1 executor | Allowed calls 2+3 |
/// |------------------------|-----------------|-------------------|
/// | Block                  | never entered   | overlap           |
/// | SetResult              | never entered   | overlap           |
///
/// A non-executing gate result is local to its call. It must neither enter that
/// executor nor collapse compatible siblings into a sequential fallback.
#[tokio::test]
async fn immediate_gate_results_do_not_serialize_compatible_siblings() {
    for (case, decision) in [
        ("block", ImmediateDecision::Block),
        ("set result", ImmediateDecision::SetResult),
    ] {
        let probe = Arc::new(ConcurrencyProbe::default());
        let immediate_calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let runtime = Runtime::new()
            .with_llm(Arc::new(CallsThenEnd::new(vec![
                ToolCall {
                    call_id: "c1".to_string(),
                    tool_id: "immediate".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    call_id: "c2".to_string(),
                    tool_id: "observed_a".to_string(),
                    arguments: serde_json::json!({}),
                },
                ToolCall {
                    call_id: "c3".to_string(),
                    tool_id: "observed_b".to_string(),
                    arguments: serde_json::json!({}),
                },
            ])))
            .with_tool(Arc::new(CountedTool {
                id: "immediate",
                calls: immediate_calls.clone(),
            }))
            .with_tool(Arc::new(ObservedTool {
                id: "observed_a",
                probe: probe.clone(),
                concurrency: None,
                delay: Duration::from_millis(10),
                barrier: Some(barrier.clone()),
            }))
            .with_tool(Arc::new(ObservedTool {
                id: "observed_b",
                probe: probe.clone(),
                concurrency: None,
                delay: Duration::from_millis(10),
                barrier: Some(barrier),
            }))
            .with_gate(Arc::new(ImmediateGate {
                tool_id: "immediate",
                decision,
            }));

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.execute(
                activation(&["immediate", "observed_a", "observed_b"]),
                RuntimeRunContext::new(),
            ),
        )
        .await
        .expect("compatible allowed calls must not be serialized")
        .expect("mixed gate batch runs");

        assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "{case}");
        assert_eq!(immediate_calls.load(Ordering::SeqCst), 0, "{case}");
        assert_eq!(probe.maximum_active.load(Ordering::SeqCst), 2, "{case}");
    }
}

/// Plugin constraints compose with, and can only narrow, the tools' intrinsic
/// declarations. The same resource key serializes otherwise parallel tools;
/// distinct keys retain maximum overlap through the identical runtime path.
#[tokio::test]
async fn plugin_resource_constraint_partitions_parallel_tools_by_argument_key() {
    for (case, keys, expected_maximum, synchronize) in [
        ("same resource", ["shared", "shared"], 1, false),
        ("different resources", ["left", "right"], 2, true),
    ] {
        let probe = Arc::new(ConcurrencyProbe::default());
        let barrier = synchronize.then(|| Arc::new(Barrier::new(2)));
        let calls = ["a", "b"]
            .into_iter()
            .zip(keys)
            .map(|(id, resource)| ToolCall {
                call_id: format!("call-{id}"),
                tool_id: id.to_string(),
                arguments: serde_json::json!({"resource": resource}),
            });
        let mut runtime = Runtime::new()
            .with_llm(Arc::new(CallsThenEnd::new(calls.collect())))
            .with_plugin(Arc::new(ConstraintPlugin));
        for id in ["a", "b"] {
            runtime = runtime.with_tool(Arc::new(ObservedTool {
                id,
                probe: probe.clone(),
                concurrency: Some(ToolConcurrency::Parallel),
                delay: Duration::from_millis(10),
                barrier: barrier.clone(),
            }));
        }
        let mut configured = activation(&["a", "b"]);
        configured.snapshot.resolved_spec.plugin_ids = vec!["constraint-plugin".to_string()];
        let outcome = runtime
            .execute(configured, RuntimeRunContext::new())
            .await
            .expect("constrained batch runs");

        assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd), "{case}");
        assert_eq!(
            probe.maximum_active.load(Ordering::SeqCst),
            expected_maximum,
            "{case}"
        );
    }
}

async fn run_observed_pair(
    left: ToolConcurrency,
    right: ToolConcurrency,
    synchronize: bool,
    delays: [Duration; 2],
) -> (usize, Vec<String>) {
    let probe = Arc::new(ConcurrencyProbe::default());
    let barrier = synchronize.then(|| Arc::new(Barrier::new(2)));
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "observed_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "observed_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(ObservedTool {
            id: "observed_a",
            probe: probe.clone(),
            concurrency: Some(left),
            delay: delays[0],
            barrier: barrier.clone(),
        }))
        .with_tool(Arc::new(ObservedTool {
            id: "observed_b",
            probe: probe.clone(),
            concurrency: Some(right),
            delay: delays[1],
            barrier,
        }))
        .with_gate(Arc::new(AllowGate));
    let commit = Arc::new(MemoryCommitCoordinator::new());
    let outcome = tokio::time::timeout(
        Duration::from_secs(2),
        runtime.execute(
            activation(&["observed_a", "observed_b"]),
            RuntimeRunContext::new().with_commit(commit.clone()),
        ),
    )
    .await
    .expect("a compatible pair must not deadlock")
    .expect("observed pair runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    let results = commit
        .committed()
        .messages
        .into_iter()
        .filter(|message| message.role == Role::Tool)
        .map(|message| message.text_content())
        .collect();
    (probe.maximum_active.load(Ordering::SeqCst), results)
}

/// Narrowing rule: an explicitly serial tool forms a one-call wave and cannot
/// overlap even another otherwise parallel call.
#[tokio::test]
async fn serial_concurrency_explicitly_prevents_overlap() {
    let (maximum, _) = run_observed_pair(
        ToolConcurrency::Serial,
        ToolConcurrency::Parallel,
        false,
        [Duration::from_millis(10), Duration::from_millis(10)],
    )
    .await;

    assert_eq!(maximum, 1);
}

/// Cause/effect rule: two calls classified as parallel enter the executor
/// together, but their model-visible results remain in request order even when
/// the second call completes first.
#[tokio::test]
async fn parallel_tools_overlap_and_publish_results_in_model_order() {
    let (maximum, results) = run_observed_pair(
        ToolConcurrency::Parallel,
        ToolConcurrency::Parallel,
        true,
        [Duration::from_millis(30), Duration::from_millis(1)],
    )
    .await;

    assert_eq!(maximum, 2, "parallel executor futures must overlap");
    assert_eq!(results, ["observed_a", "observed_b"]);
}

/// Resource-access decision table:
///
/// | Left | Right | Same address | Expected maximum active |
/// |------|-------|--------------|-------------------------|
/// | read | read  | yes          | 2 |
/// | read | write | yes          | 1 |
/// | write| write | yes          | 1 |
/// | write| write | no           | 2 |
///
/// This covers the scheduler's conflict relation rather than individual tool
/// names: names never imply safety; only trusted resource claims do.
#[tokio::test]
async fn resource_claims_enforce_read_write_conflicts() {
    let shared = ToolResource::new("test", "shared");
    let other = ToolResource::new("test", "other");
    let cases = [
        (
            "read/read",
            ToolResourceAccess::Read(shared.clone()),
            ToolResourceAccess::Read(shared.clone()),
            2,
        ),
        (
            "read/write",
            ToolResourceAccess::Read(shared.clone()),
            ToolResourceAccess::Write(shared.clone()),
            1,
        ),
        (
            "write/write",
            ToolResourceAccess::Write(shared.clone()),
            ToolResourceAccess::Write(shared),
            1,
        ),
        (
            "different resources",
            ToolResourceAccess::Write(ToolResource::new("test", "shared")),
            ToolResourceAccess::Write(other),
            2,
        ),
    ];

    for (case, left, right, expected) in cases {
        let (maximum, _) = run_observed_pair(
            ToolConcurrency::Resources(vec![left]),
            ToolConcurrency::Resources(vec![right]),
            expected == 2,
            [Duration::from_millis(10), Duration::from_millis(10)],
        )
        .await;
        assert_eq!(maximum, expected, "{case}");
    }
}

/// Boundary rule: semantic compatibility does not bypass the per-attempt
/// capacity limit. Three parallel calls with limit two execute as waves 2+1.
#[tokio::test]
async fn compatible_tools_respect_the_bounded_concurrency_limit() {
    let probe = Arc::new(ConcurrencyProbe::default());
    let calls = ["a", "b", "c"].map(|id| ToolCall {
        call_id: format!("call-{id}"),
        tool_id: id.to_string(),
        arguments: serde_json::json!({}),
    });
    let mut runtime = Runtime::new().with_llm(Arc::new(CallsThenEnd::new(calls.to_vec())));
    for id in ["a", "b", "c"] {
        runtime = runtime.with_tool(Arc::new(ObservedTool {
            id,
            probe: probe.clone(),
            concurrency: Some(ToolConcurrency::Parallel),
            delay: Duration::from_millis(20),
            barrier: None,
        }));
    }
    let limit = NonZeroUsize::new(2).expect("two is non-zero");
    let outcome = runtime
        .execute(
            activation(&["a", "b", "c"]),
            RuntimeRunContext::new().with_tool_concurrency_limit(limit),
        )
        .await
        .expect("bounded batch runs");

    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));
    assert_eq!(probe.maximum_active.load(Ordering::SeqCst), 2);
}

/// Two tool calls in one Step, each staging a `Commutative` object write to the
/// same key, both commit and shallow-merge into one value.
#[tokio::test]
async fn batched_commutative_tool_writes_merge() {
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "mutate_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "mutate_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(FixedTool {
            id: "mutate_a",
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "acc",
                serde_json::json!({"a": 1}),
            )],
            concurrency: ToolConcurrency::Parallel,
        }))
        .with_tool(Arc::new(FixedTool {
            id: "mutate_b",
            state: vec![StateCommand::set(
                Scope::Thread,
                MergePolicy::Commutative,
                "acc",
                serde_json::json!({"b": 2}),
            )],
            concurrency: ToolConcurrency::Parallel,
        }))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["mutate_a", "mutate_b"]), context)
        .await
        .expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let committed = commit.committed();
    assert_eq!(
        committed
            .state
            .iter()
            .filter(|command| command.key.0 == "acc")
            .count(),
        2,
        "both tool-owned writes are committed"
    );

    // Replay proves the two commutative writes shallow-merge into one value.
    let store = replay_state(&committed);
    assert_eq!(
        store.get(Scope::Thread, &Key("acc".into())),
        Some(&serde_json::json!({"a": 1, "b": 2})),
        "both commutative writes must be visible after replay"
    );
}

/// Two tool calls in one Step each staging an `Exclusive` write to the same key
/// conflict across the accumulated batch and fail closed — no state is committed.
#[tokio::test]
async fn batched_exclusive_tool_writes_conflict_fail_closed() {
    let exclusive = |v: i64| {
        vec![StateCommand::set(
            Scope::Run,
            MergePolicy::Exclusive,
            "lock",
            serde_json::json!(v),
        )]
    };
    let runtime = Runtime::new()
        .with_llm(Arc::new(CallsThenEnd::new(vec![
            ToolCall {
                call_id: "c1".to_string(),
                tool_id: "lock_a".to_string(),
                arguments: serde_json::json!({}),
            },
            ToolCall {
                call_id: "c2".to_string(),
                tool_id: "lock_b".to_string(),
                arguments: serde_json::json!({}),
            },
        ])))
        .with_tool(Arc::new(FixedTool {
            id: "lock_a",
            state: exclusive(1),
            concurrency: ToolConcurrency::Parallel,
        }))
        .with_tool(Arc::new(FixedTool {
            id: "lock_b",
            state: exclusive(2),
            concurrency: ToolConcurrency::Parallel,
        }))
        .with_gate(Arc::new(AllowGate));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime
        .execute(activation(&["lock_a", "lock_b"]), context)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        RunState::Ended(EndCause::Error(Failure::StateConflict)),
        "two exclusive writes to one key must fail closed"
    );
    let committed = commit.committed();
    assert!(
        committed
            .state
            .iter()
            .all(|command| command.key.0 != "lock"),
        "a conflicting tool-owned batch must never partially commit, got {:?}",
        committed.state
    );
}
