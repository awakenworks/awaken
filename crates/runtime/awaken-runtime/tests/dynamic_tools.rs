//! A plugin whose tool set is dynamic (`live_version`) contributes tools that
//! reach the model and execute, and a version bump between steps re-resolves the
//! tool face at the step boundary — the seam MCP `tools/list_changed` uses.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::agent::state::{Command, Key, MergePolicy, Scope, Store};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime_contract::ToolSearchLimit;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, ToolCall,
};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, IdBound, Plugin, PluginManifest,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec, ToolDescriptor,
    ToolDiscoverySettings, ToolExposure, ToolPresentation, ToolPresentationOverride,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};
use awaken_store_inmem::MemoryCommitCoordinator;

/// A trivial dynamic tool that echoes its id.
struct EchoDyn(String);

#[async_trait::async_trait]
impl RawTool for EchoDyn {
    fn id(&self) -> &str {
        &self.0
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok(call.call_id, format!("ran {}", self.0)))
    }
}

/// A plugin whose contributed tool set and `live_version` come from shared state,
/// so a test can mutate them mid-run.
struct DynPlugin {
    version: Arc<AtomicU64>,
    tools: Arc<Mutex<Vec<String>>>,
}

impl Plugin for DynPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "mcp:srv".to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tools: IdBound::Namespace("mcp__srv__".to_string()),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("mcp:srv");
        for id in self.tools.lock().unwrap().iter() {
            contributions.dynamic_tools.push(
                DynamicTool::try_new(
                    ToolDescriptor::pinned(
                        "mcp",
                        id.clone(),
                        "dynamic",
                        serde_json::json!({ "type": "object" }),
                    ),
                    Arc::new(EchoDyn(id.clone())),
                )
                .expect("matching dynamic tool identity"),
            );
        }
        contributions
    }

    fn live_version(&self) -> Option<u64> {
        Some(self.version.load(Ordering::SeqCst))
    }
}

/// Records the tool ids visible on each inference; on the first step it also
/// bumps the plugin version and swaps the tool set (simulating list_changed),
/// then calls the step-0 tool. The second step ends the run.
struct RefreshProbe {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    version: Arc<AtomicU64>,
    tools: Arc<Mutex<Vec<String>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for RefreshProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let visible: Vec<String> = request.tools.iter().map(|t| t.id.clone()).collect();
        self.seen.lock().unwrap().push(visible);
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            // Simulate a tools/list_changed: swap the set and advance the version.
            *self.tools.lock().unwrap() = vec!["mcp__srv__b".to_string()];
            self.version.fetch_add(1, Ordering::SeqCst);
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".to_string(),
                tool_id: "mcp__srv__a".to_string(),
                arguments: serde_json::json!({}),
            }])
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

fn activation() -> RunActivation {
    let fingerprint = CatalogFingerprint("catalog-a".to_string());
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
                max_steps: 8,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding {
                        provider_identity_ref: "provider-1".to_string(),
                        model_ref: "model-1".to_string(),
                        backend_ref: "backend-1".to_string(),
                    },
                ),
                // No static descriptors: every visible tool is dynamic.
                tool_descriptors: Vec::new(),
                plugin_ids: vec!["mcp:srv".to_string()],
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("message-1".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text("go")],
        }],
        delegation_origin: None,
        model_ref_override: None,
        data_subject_id: None,
        tool_capability_narrowing: Default::default(),
    }
}

/// A scripted model that records the visible tool ids each step, calls a tool by its
/// ALIAS on step 0, and ends on step 1.
struct AliasProbe {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for AliasProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.seen
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|t| t.id.clone()).collect());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = if n == 0 {
            AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".to_string(),
                tool_id: "create_issue".to_string(), // the ALIAS, not mcp__srv__a
                arguments: serde_json::json!({}),
            }])
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

/// ADR-0053: an agent's `ToolPresentation` renames an MCP tool for the model, and a
/// call by that alias reverse-maps to the canonical `mcp__…` id at the single ingress —
/// so the aliased MCP tool actually executes. Proves alias + description override + the
/// reverse-map invariant work for a *dynamic (MCP)* tool, not just static catalog tools.
#[tokio::test]
async fn presentation_aliases_an_mcp_tool_and_dispatches_the_alias_to_canonical() {
    let version = Arc::new(AtomicU64::new(1));
    let tools = Arc::new(Mutex::new(vec!["mcp__srv__a".to_string()]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::new()
        .with_llm(Arc::new(AliasProbe {
            seen: seen.clone(),
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(Arc::new(DynPlugin {
            version: version.clone(),
            tools: tools.clone(),
        }));

    // Alias the MCP tool + override its description.
    let mut act = activation();
    act.snapshot.resolved_spec.tool_presentation = ToolPresentation::from_overrides([(
        "mcp__srv__a".to_string(),
        ToolPresentationOverride {
            alias: Some("create_issue".to_string()),
            description: Some("Create a GitHub issue.".to_string()),
            exposure: None,
        },
    )]);

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(act, context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    // The model saw the ALIAS on the face — never the canonical MCP id.
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen[0],
        vec!["create_issue".to_string()],
        "the model sees the alias, not the canonical mcp__ id"
    );

    // A call by the alias reverse-mapped to the canonical MCP tool, which executed.
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("ran mcp__srv__a")),
        "the aliased call dispatched to the canonical MCP tool"
    );
}

/// A scripted model for the discovery flow: step 0 records the catalog view and calls
/// `tool_search`; step 1 records the view and calls the now-revealed tool;
/// step 2 ends.
struct DiscoveryProbe {
    seen: Arc<Mutex<Vec<Vec<String>>>>,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for DiscoveryProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        self.seen
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|t| t.id.clone()).collect());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let output = match n {
            0 => AssistantOutput::from_tool_calls(vec![
                ToolCall {
                    call_id: "open-create".to_string(),
                    tool_id: awaken_runtime_contract::resolved::TOOL_SEARCH_ID.to_string(),
                    arguments: serde_json::json!({ "query": "select:create_issue" }),
                },
                ToolCall {
                    call_id: "open-list".to_string(),
                    tool_id: awaken_runtime_contract::resolved::TOOL_SEARCH_ID.to_string(),
                    arguments: serde_json::json!({ "query": "select:list_issues" }),
                },
            ]),
            1 => AssistantOutput::from_tool_calls(vec![ToolCall {
                call_id: "c1".to_string(),
                tool_id: "create_issue".to_string(),
                arguments: serde_json::json!({}),
            }]),
            _ => AssistantOutput::text("done".to_string()),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn an_on_demand_tool_is_hidden_until_tool_search_then_callable_and_persisted() {
    // Causal graph and state-transition coverage:
    // C1 a live MCP descriptor is OnDemand; C2 it has an alias; C3 no reveal fact
    // exists; C4 two tool_search calls select distinct aliases in one batch under
    // a published one-result-per-call limit; C5 each ToolResult and the cumulative
    // reveal State command commit in model order; C6 the next inference rebuilds
    // its model view.
    // Effects: E1 only tool_search is initially visible; E2 the result carries a
    // tool_reference; E3 both canonical-id/fingerprint facts survive State replay;
    // E4 both aliased descriptors are visible next Step; E5 an alias call dispatches
    // to the canonical MCP executable; E6 sequential discovery results form a union
    // rather than overwriting one another. This single path covers static/dynamic
    // catalog convergence, bounded presentation, persistence, replay and execution.
    let version = Arc::new(AtomicU64::new(1));
    let tools = Arc::new(Mutex::new(vec![
        "mcp__srv__a".to_string(),
        "mcp__srv__b".to_string(),
    ]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::new()
        .with_llm(Arc::new(DiscoveryProbe {
            seen: seen.clone(),
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(Arc::new(DynPlugin {
            version: version.clone(),
            tools: tools.clone(),
        }));

    let mut act = activation();
    act.snapshot.resolved_spec.tool_presentation = ToolPresentation::from_overrides([
        (
            "mcp__srv__a".to_string(),
            ToolPresentationOverride {
                alias: Some("create_issue".to_string()),
                description: Some("Create a GitHub issue.".to_string()),
                exposure: Some(ToolExposure::OnDemand),
            },
        ),
        (
            "mcp__srv__b".to_string(),
            ToolPresentationOverride {
                alias: Some("list_issues".to_string()),
                description: Some("List GitHub issues.".to_string()),
                exposure: Some(ToolExposure::OnDemand),
            },
        ),
    ])
    .with_discovery(ToolDiscoverySettings {
        max_results: Some(ToolSearchLimit::new(1).expect("valid test limit")),
        ..Default::default()
    });

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(act, context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let seen = seen.lock().unwrap();
    let search_id = awaken_runtime_contract::resolved::TOOL_SEARCH_ID.to_string();
    // C1+C3=>E1: only `tool_search` is shown; the full schema is withheld.
    assert!(seen[0].contains(&search_id), "tool_search is offered");
    assert!(
        !seen[0].contains(&"create_issue".to_string()),
        "the on-demand tool is withheld until revealed"
    );
    // C4+C5+C6=>E4: the schema is visible on the next request.
    assert!(
        seen[1].contains(&"create_issue".to_string()),
        "the revealed tool appears on the next step"
    );
    assert!(
        seen[1].contains(&"list_issues".to_string()),
        "C4+C5+C6=>E4+E6: both sequential reveals appear on the next step"
    );
    // C2+call=>E5: execution uses the canonical MCP identity.
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("ran mcp__srv__a")),
        "the revealed tool executed"
    );
    let replayed = Store::rebuild(&committed.state);
    let discovery = replayed
        .get(Scope::Run, &Key("runtime.tool_discovery.v1".into()))
        .expect("committed discovery state replays");
    for canonical_id in ["mcp__srv__a", "mcp__srv__b"] {
        assert!(
            discovery
                .pointer(&format!("/revealed/{canonical_id}"))
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "C5=>E3: state authority remains canonical rather than aliased"
        );
    }
}

#[tokio::test]
async fn dynamic_tool_is_visible_executes_and_refreshes_at_the_step_boundary() {
    let version = Arc::new(AtomicU64::new(1));
    let tools = Arc::new(Mutex::new(vec!["mcp__srv__a".to_string()]));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let runtime = Runtime::new()
        .with_llm(Arc::new(RefreshProbe {
            seen: seen.clone(),
            version: version.clone(),
            tools: tools.clone(),
            calls: AtomicUsize::new(0),
        }))
        .with_plugin(Arc::new(DynPlugin {
            version: version.clone(),
            tools: tools.clone(),
        }));

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let outcome = runtime.execute(activation(), context).await.expect("runs");
    assert_eq!(outcome, RunState::Ended(EndCause::NaturalEnd));

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "two inferences");
    // Step 0 saw the initial dynamic tool.
    assert_eq!(seen[0], vec!["mcp__srv__a".to_string()]);
    // Step 1 saw the refreshed set after the version bump — proving the
    // step-boundary re-resolution took effect.
    assert_eq!(seen[1], vec!["mcp__srv__b".to_string()]);

    // The step-0 dynamic tool actually executed (a model-visible tool result).
    let committed = commit.committed();
    assert!(
        committed
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.text_content().contains("ran mcp__srv__a")),
        "the dynamic tool executed"
    );
}

struct OutOfBoundStateTool;

#[async_trait::async_trait]
impl RawTool for OutOfBoundStateTool {
    fn id(&self) -> &str {
        "state_intruder"
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        Ok(
            ToolOutput::ok(&call.call_id, "false success").with_state(vec![Command::set(
                Scope::Thread,
                MergePolicy::Exclusive,
                "other/secret",
                serde_json::json!(true),
            )]),
        )
    }
}

struct StateBoundPlugin;

impl Plugin for StateBoundPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: "state-boundary".into(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tools: IdBound::Exact(vec!["state_intruder".into()]),
                state_keys: IdBound::Namespace("owned/".into()),
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new("state-boundary");
        contributions.declare_state_key("owned/");
        contributions.register_dynamic_tool(
            DynamicTool::try_new(
                ToolDescriptor::pinned(
                    "state-boundary",
                    "state_intruder",
                    "attempt one forbidden state write",
                    serde_json::json!({"type":"object","additionalProperties":false}),
                ),
                Arc::new(OutOfBoundStateTool),
            )
            .expect("test descriptor and executable identities match"),
        );
        contributions
    }
}

struct StateBoundaryProbe {
    calls: AtomicUsize,
    rejected: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl LlmExecutor for StateBoundaryProbe {
    async fn infer(
        &self,
        request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        if !first {
            self.rejected.store(
                request.messages.iter().any(|message| {
                    message.role == Role::Tool
                        && awaken_agent_contract::agent::content::extract_text(&message.content)
                            .contains("outside its plugin capability")
                }),
                Ordering::SeqCst,
            );
        }
        Ok(ChatResponse {
            output: if first {
                AssistantOutput::from_tool_calls(vec![ToolCall {
                    call_id: "intrusion".into(),
                    tool_id: "state_intruder".into(),
                    arguments: serde_json::json!({}),
                }])
            } else {
                AssistantOutput::text("done")
            },
            usage: None,
            stop_reason: None,
        })
    }
}

#[tokio::test]
async fn dynamic_tool_state_commands_cannot_escape_the_owning_plugin_bound() {
    // Security cause graph: C1 manifest grants only owned/*; C2 executable
    // returns other/secret; E1 Runtime replaces false success with an error;
    // E2 the forbidden command never enters Thread state. The manifest bound is
    // executable authority, not documentation.
    let rejected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let runtime = Runtime::new()
        .with_llm(Arc::new(StateBoundaryProbe {
            calls: AtomicUsize::new(0),
            rejected: rejected.clone(),
        }))
        .with_plugin(Arc::new(StateBoundPlugin));
    let mut run = activation();
    run.snapshot.resolved_spec.plugin_ids = vec!["state-boundary".into()];
    let commits = Arc::new(MemoryCommitCoordinator::new());
    assert_eq!(
        runtime
            .execute(run, RuntimeRunContext::new().with_commit(commits.clone()))
            .await
            .expect("the rejected tool result remains model-visible"),
        RunState::Ended(EndCause::NaturalEnd)
    );
    assert!(rejected.load(Ordering::SeqCst), "C2/E1");
    let state = Store::rebuild(&commits.committed().state);
    assert!(
        state
            .get(Scope::Thread, &Key("other/secret".into()))
            .is_none(),
        "C2/E2"
    );
}
