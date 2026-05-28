//! Reusable executor / judge stubs for awaken-eval's lib + integration
//! tests, and for downstream crates (e.g. `awaken-server`) that wire the
//! same fakes into their own harnesses. Gated by the `test-support`
//! Cargo feature so production binaries never pull these in.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_runtime_contract::contract::content::ContentBlock;
use awaken_runtime_contract::contract::executor::{
    InferenceExecutionError, InferenceRequest, LlmExecutor,
};
use awaken_runtime_contract::contract::inference::{StopReason, StreamResult, TokenUsage};

use crate::judge::{Judge, JudgeError, JudgeResult};
use crate::outcome::ReplayOutcome;

/// LLM executor that returns canned text from a FIFO queue. When the
/// queue depletes the LAST response is repeated, so single-shot tests
/// stay terse and revise-loop terminators stay deterministic.
pub struct ScriptedExecutor {
    name: String,
    responses: Mutex<Vec<String>>,
    last: Mutex<Option<String>>,
    tokens: TokenUsage,
}

impl ScriptedExecutor {
    pub fn new(name: &str, responses: Vec<&str>) -> Self {
        Self {
            name: name.into(),
            responses: Mutex::new(responses.into_iter().map(String::from).collect()),
            last: Mutex::new(None),
            tokens: TokenUsage {
                prompt_tokens: Some(1),
                completion_tokens: Some(1),
                total_tokens: Some(2),
                ..Default::default()
            },
        }
    }

    pub fn with_tokens(mut self, tokens: TokenUsage) -> Self {
        self.tokens = tokens;
        self
    }

    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[async_trait]
impl LlmExecutor for ScriptedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        let text = {
            let mut queue = self.responses.lock().unwrap();
            let mut last = self.last.lock().unwrap();
            if let Some(next) = (!queue.is_empty()).then(|| queue.remove(0)) {
                *last = Some(next.clone());
                next
            } else {
                last.clone().unwrap_or_default()
            }
        };
        Ok(StreamResult {
            content: vec![ContentBlock::text(text)],
            tool_calls: vec![],
            usage: Some(self.tokens.clone()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// LLM executor that yields an empty `StreamResult`. Use this when a
/// runtime builder needs a provider wired in but the test path is not
/// expected to drive any inference (e.g. scripted-fixture replay).
pub struct UnusedExecutor;

#[async_trait]
impl LlmExecutor for UnusedExecutor {
    async fn execute(
        &self,
        _request: InferenceRequest,
    ) -> Result<StreamResult, InferenceExecutionError> {
        Ok(StreamResult {
            content: vec![],
            tool_calls: vec![],
            usage: Some(TokenUsage::default()),
            stop_reason: Some(StopReason::EndTurn),
            has_incomplete_tool_calls: false,
        })
    }

    fn name(&self) -> &str {
        "unused"
    }
}

/// Judge that returns scores from a FIFO queue. When depleted it yields
/// 1.0, so revise-loop tests cannot deadlock by exhausting the script.
pub struct ScriptedJudge {
    scores: Mutex<Vec<f32>>,
    reasoning: Option<String>,
}

impl ScriptedJudge {
    pub fn new(scores: Vec<f32>) -> Self {
        Self {
            scores: Mutex::new(scores),
            reasoning: Some("scripted".into()),
        }
    }

    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning;
        self
    }

    pub fn arc(self) -> Arc<Self> {
        Arc::new(self)
    }
}

#[async_trait]
impl Judge for ScriptedJudge {
    async fn judge(
        &self,
        _outcome: &ReplayOutcome,
        _user_prompt: &str,
        _rubric: Option<&str>,
    ) -> Result<JudgeResult, JudgeError> {
        let score = {
            let mut queue = self.scores.lock().unwrap();
            if queue.is_empty() {
                1.0
            } else {
                queue.remove(0)
            }
        };
        Ok(JudgeResult {
            score,
            reasoning: self.reasoning.clone(),
            inference_id: None,
        })
    }
}

/// Judge that panics on every call. Used to guard cache-hit paths from
/// regressing into re-judging — if the executor is invoked the test
/// fails loudly instead of silently double-billing the upstream model.
pub struct ExplodingJudge;

#[async_trait]
impl Judge for ExplodingJudge {
    async fn judge(
        &self,
        _outcome: &ReplayOutcome,
        _user_prompt: &str,
        _rubric: Option<&str>,
    ) -> Result<JudgeResult, JudgeError> {
        panic!("cache should prevent re-judging");
    }
}
