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
    AssistantOutput, ChatMessage, ChatRequest, ChatResponse, LlmExecutor, StopReason, ToolCall,
};
use awaken_runtime_contract::resolved::{
    CatalogFingerprint, ContextPolicy, ModelBinding, ResolvedSpec,
};
use awaken_runtime_contract::runtime_context::RuntimeRunContext;
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_runtime_contract::tool::{RawTool, ToolError, ToolOutput};

use crate::{Case, CaseScore, Dataset, ExpectationResult, Report, ScriptedTurn, score_case};

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

/// Replay one case through the real runtime, returning the committed assistant
/// text, whether the run ended naturally, and the ids of the tools it called.
async fn replay(case: &Case) -> (String, bool, Vec<String>) {
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
                tool_presentation: Default::default(),
            },
            fingerprint,
        },
        input: vec![Message {
            id: MessageId("eval-input".to_string()),
            role: Role::User,
            content: vec![ContentBlock::text(case.input.clone())],
        }],
        model_ref_override: None,
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
    (output, succeeded, tools_called)
}

/// Replay one case through the real runtime and score it (no LLM judge — a
/// `JudgeScore` expectation fails with a "no judge configured" detail). For judge
/// scoring, use [`Evaluator::with_judge`].
pub async fn run_case(case: &Case) -> CaseScore {
    Evaluator::new().run_case(case).await
}

/// Replay every case in a dataset and collect the report (no LLM judge).
pub async fn run_dataset(dataset: &Dataset) -> Report {
    Evaluator::new().run_dataset(dataset).await
}

/// Replays cases through the real runtime and scores them, optionally running an
/// injected judge model for [`JudgeScore`](crate::Expectation::JudgeScore)
/// expectations.
#[derive(Default)]
pub struct Evaluator {
    judge: Option<Arc<dyn LlmExecutor>>,
}

impl Evaluator {
    /// An evaluator with no judge (`JudgeScore` expectations fail).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Score `JudgeScore` expectations with `judge` — one inference per judged
    /// expectation, prompted to return a 0–100 score.
    #[must_use]
    pub fn with_judge(mut self, judge: Arc<dyn LlmExecutor>) -> Self {
        self.judge = Some(judge);
        self
    }

    /// Replay one case and score every expectation, running the judge for any
    /// `JudgeScore`.
    pub async fn run_case(&self, case: &Case) -> CaseScore {
        let (output, succeeded, tools_called) = replay(case).await;
        let mut score = score_case(case, &output, succeeded, &tools_called);
        for (i, exp) in case.expectations.iter().enumerate() {
            if let Some((rubric, min_score)) = exp.judge_spec() {
                score.results[i] = self.judge_result(rubric, min_score, &output).await;
            }
        }
        score
    }

    /// Replay every case in a dataset and collect the report.
    pub async fn run_dataset(&self, dataset: &Dataset) -> Report {
        let mut scores = Vec::with_capacity(dataset.cases.len());
        for case in &dataset.cases {
            scores.push(self.run_case(case).await);
        }
        Report {
            dataset: dataset.name.clone(),
            scores,
        }
    }

    /// Run the judge for one `JudgeScore`: one inference scoring `output` against
    /// `rubric`, passing when the parsed 0–100 score is at least `min_score`.
    async fn judge_result(&self, rubric: &str, min_score: u8, output: &str) -> ExpectationResult {
        let kind = "judge_score".to_string();
        let Some(judge) = &self.judge else {
            return ExpectationResult {
                expectation_kind: kind,
                passed: false,
                detail: "no judge configured".to_string(),
            };
        };
        let prompt = format!(
            "You are a strict evaluator. Rubric:\n{rubric}\n\nAgent output:\n{output}\n\n\
             Score from 0 to 100 how well the output satisfies the rubric. \
             Reply with ONLY the integer score."
        );
        let request = ChatRequest {
            model_binding: ModelBinding::new("eval", "judge", "genai"),
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::text(prompt)],
            }],
            tools: Vec::new(),
        };
        match judge.infer(request).await {
            Ok(response) => {
                let text = response.output.text_content();
                match parse_score(&text) {
                    Some(score) => {
                        let passed = score >= min_score;
                        ExpectationResult {
                            expectation_kind: kind,
                            passed,
                            detail: format!("judge scored {score} (needed >= {min_score})"),
                        }
                    }
                    None => ExpectationResult {
                        expectation_kind: kind,
                        passed: false,
                        detail: format!("judge output was not a 0-100 score: {text:?}"),
                    },
                }
            }
            Err(err) => ExpectationResult {
                expectation_kind: kind,
                passed: false,
                detail: format!("judge call failed: {err}"),
            },
        }
    }
}

/// The first run of ASCII digits in `text`, clamped to 0–100. `None` when the
/// judge produced no digits.
fn parse_score(text: &str) -> Option<u8> {
    let digits: String = text
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse::<u32>().ok().map(|n| n.min(100) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Expectation;

    /// A judge that always returns `score` as its text, so a test can drive the
    /// threshold logic deterministically.
    struct ScoreJudge(&'static str);
    #[async_trait::async_trait]
    impl LlmExecutor for ScoreJudge {
        async fn infer(
            &self,
            _request: ChatRequest,
        ) -> awaken_runtime_contract::llm::Result<ChatResponse> {
            Ok(ChatResponse {
                output: AssistantOutput::text(self.0.to_string()),
                usage: None,
                stop_reason: Some(StopReason::EndTurn),
            })
        }
    }

    fn judged_case() -> Case {
        Case {
            id: "j".to_string(),
            instructions: String::new(),
            input: "q".to_string(),
            script: vec![ScriptedTurn {
                text: "a thoughtful answer".to_string(),
                tool_calls: Vec::new(),
            }],
            expectations: vec![Expectation::JudgeScore {
                rubric: "is it thoughtful?".to_string(),
                min_score: 70,
            }],
        }
    }

    #[tokio::test]
    async fn judge_score_passes_at_or_above_threshold_and_fails_below() {
        let case = judged_case();
        let pass = Evaluator::new()
            .with_judge(Arc::new(ScoreJudge("85")))
            .run_case(&case)
            .await;
        assert!(pass.passed(), "85 >= 70 should pass: {:?}", pass.results);

        let fail = Evaluator::new()
            .with_judge(Arc::new(ScoreJudge("40")))
            .run_case(&case)
            .await;
        assert!(!fail.passed(), "40 < 70 should fail");
    }

    #[tokio::test]
    async fn a_judge_score_exactly_at_the_threshold_passes() {
        // Boundary: the threshold is inclusive (`score >= min_score`), so a judge
        // score exactly equal to the minimum passes.
        let case = judged_case(); // min_score = 70
        let score = Evaluator::new()
            .with_judge(Arc::new(ScoreJudge("70")))
            .run_case(&case)
            .await;
        assert!(score.passed(), "70 >= 70 is inclusive: {:?}", score.results);
        assert!(score.results[0].detail.contains("70"));
    }

    #[tokio::test]
    async fn a_non_numeric_judge_output_fails_with_a_clear_detail() {
        // The judge replied with no parseable 0-100 score → fail (not silently pass),
        // and the detail says the output was not a score.
        let case = judged_case();
        let score = Evaluator::new()
            .with_judge(Arc::new(ScoreJudge("I cannot decide")))
            .run_case(&case)
            .await;
        assert!(!score.passed(), "an unparseable judge reply is not a pass");
        assert!(
            score.results[0].detail.contains("not a 0-100 score"),
            "detail: {}",
            score.results[0].detail
        );
    }

    #[tokio::test]
    async fn without_a_judge_a_judge_score_fails_with_a_clear_detail() {
        let score = Evaluator::new().run_case(&judged_case()).await;
        assert!(!score.passed());
        assert!(score.results[0].detail.contains("no judge"));
    }

    #[test]
    fn parse_score_reads_the_first_integer_clamped_to_100() {
        assert_eq!(parse_score("85"), Some(85));
        assert_eq!(parse_score("Score: 42 out of 100"), Some(42));
        assert_eq!(parse_score("999"), Some(100));
        assert_eq!(parse_score("no digits here"), None);
    }
}
