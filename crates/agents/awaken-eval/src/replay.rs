//! Replay a recorded case through the real runtime and score it.
//!
//! The recorded model responses are served by a scripted [`LlmExecutor`], so the
//! run drives the true engine (`RunExecutor::execute`) — the harness contributes
//! no execution logic of its own. The committed assistant text and the terminal
//! phase are what the expectations are scored against.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Phase};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_runtime::Runtime;
use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::capability::RuntimeCapabilityCatalog;
use awaken_runtime_contract::catalog::{RuntimeCatalogInstall, RuntimeCatalogInstaller};
use awaken_runtime_contract::execution::RunExecutor;
use awaken_runtime_contract::llm::{
    AssistantOutput, ChatRequest, ChatResponse, LlmExecutor, StopReason, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

use crate::{Case, CaseScore, Dataset, Report, ScriptedTurn, score_case};

/// Serves the recorded turns in order; past the end it repeats the last turn. A
/// turn with `tool_calls` drives the engine's tool loop (stop reason left open so
/// the loop continues); a text turn ends the turn (`EndTurn`).
struct ScriptLlm {
    turns: Vec<ScriptedTurn>,
    n: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmExecutor for ScriptLlm {
    async fn infer(
        &self,
        _request: ChatRequest,
    ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
        let i = self.n.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.get(i).or_else(|| self.turns.last());
        let (output, stop_reason) = match turn {
            Some(t) if !t.tool_calls.is_empty() => {
                let calls = t
                    .tool_calls
                    .iter()
                    .enumerate()
                    .map(|(j, tc)| ToolCall {
                        call_id: format!("call-{i}-{j}"),
                        tool_id: tc.tool_id.clone(),
                        arguments: tc.arguments.clone(),
                    })
                    .collect();
                (AssistantOutput::from_tool_calls(calls), None)
            }
            Some(t) => (
                AssistantOutput::text(t.text.clone()),
                Some(StopReason::EndTurn),
            ),
            None => (
                AssistantOutput::text(String::new()),
                Some(StopReason::EndTurn),
            ),
        };
        Ok(ChatResponse {
            output,
            usage: None,
            stop_reason,
        })
    }
}

/// A fixed echo tool the eval registers for each tool id a case's script invokes,
/// so a scripted tool call executes on the real engine. Records that it ran, so a
/// [`ToolCalled`](crate::Expectation::ToolCalled) expectation can be scored.
struct EvalEchoTool {
    id: String,
    invoked: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl RawTool for EvalEchoTool {
    fn id(&self) -> &str {
        &self.id
    }
    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        self.invoked.lock().unwrap().push(call.tool_id.clone());
        Ok(ToolOutput::ok(call.call_id, "eval-echo"))
    }
}

/// Replay one case through the real runtime and score it.
pub async fn run_case(case: &Case) -> CaseScore {
    let fingerprint = CatalogFingerprint("eval".to_string());
    let mut runtime = Runtime::new().with_llm(Arc::new(ScriptLlm {
        turns: case.script.clone(),
        n: AtomicUsize::new(0),
    }));
    // Register a fixed echo tool for each distinct tool id the script invokes, so
    // a scripted tool call executes on the real engine and is recorded.
    let invoked = Arc::new(Mutex::new(Vec::new()));
    let mut registered: Vec<String> = Vec::new();
    for turn in &case.script {
        for tc in &turn.tool_calls {
            if !registered.contains(&tc.tool_id) {
                registered.push(tc.tool_id.clone());
                runtime = runtime.with_tool(Arc::new(EvalEchoTool {
                    id: tc.tool_id.clone(),
                    invoked: invoked.clone(),
                }));
            }
        }
    }
    runtime
        .install_catalog(RuntimeCatalogInstall {
            publication_id: "eval".to_string(),
            fingerprint: fingerprint.clone(),
            source_revisions: vec!["eval".to_string()],
            capabilities: RuntimeCapabilityCatalog {
                catalog_fingerprint: fingerprint.clone(),
                runtime_version: "eval".to_string(),
                tools: Vec::new(),
                plugins: Vec::new(),
            },
        })
        .expect("catalog installs");

    let activation = RunActivation {
        run_id: RunId(format!("eval-{}", case.id)),
        thread_id: ThreadId(format!("eval-{}", case.id)),
        snapshot: ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("eval".to_string()),
            root_agent_id: AgentId("eval".to_string()),
            resolved_spec: ResolvedSpec {
                catalog_fingerprint: fingerprint.clone(),
                instructions: case.instructions.clone(),
                max_steps: 16,
                model_binding: ModelBinding::new("eval", "eval-model", "genai"),
                model_candidates: Vec::new(),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: ContextPolicy::KeepAll,
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("eval-input".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text(case.input.clone())],
        }],
        trace: Default::default(),
    };

    let commit = Arc::new(MemoryCommitCoordinator::new());
    let context = RuntimeRunContext::new().with_commit(commit.clone());
    let phase = runtime
        .execute(activation, context)
        .await
        .expect("run executes");

    let succeeded = matches!(phase, Phase::Ended(EndCause::NaturalEnd));
    let output = commit
        .committed()
        .messages
        .into_iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.text_content())
        .collect::<Vec<_>>()
        .join("\n");

    let tools_called = invoked.lock().unwrap().clone();
    score_case(case, &output, succeeded, &tools_called)
}

/// Replay every case in a dataset and collect the report.
pub async fn run_dataset(dataset: &Dataset) -> Report {
    let mut scores = Vec::with_capacity(dataset.cases.len());
    for case in &dataset.cases {
        scores.push(run_case(case).await);
    }
    Report {
        dataset: dataset.name.clone(),
        scores,
    }
}
