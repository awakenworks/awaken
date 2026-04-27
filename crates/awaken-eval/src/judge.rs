//! LLM-backed grading through a TensorZero `[functions.judge]` chat
//! function.
//!
//! This module is gated behind the `llm-judge` feature; enable it with:
//!
//! ```toml
//! awaken-eval = { version = "0.4", features = ["llm-judge"] }
//! ```
//!
//! The judge sends the user prompt + assistant answer to the configured
//! TensorZero gateway and parses the reply as a JSON object of the shape
//! `{"score": 0.0-1.0, "reasoning": "..."}`. When `submit_feedback` is
//! enabled, the resulting score is also POSTed to the gateway's `/feedback`
//! endpoint as the `response_quality` metric so it accumulates in
//! ClickHouse for offline analysis.
//!
//! The whole module is purely additive on top of M4: replayers, fixtures,
//! and pure scoring continue to work unchanged when the feature is absent.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

use crate::expectation::{Expectation, Failure};
use crate::outcome::ReplayOutcome;

/// Configuration for the bundled [`TensorZeroJudge`].
///
/// All fields default to values that match the docker-compose stack shipped
/// with the repo (`./scripts/e2e-tensorzero.sh`), so a zero-config call is
/// usually enough.
#[derive(Debug, Clone)]
pub struct JudgeConfig {
    /// TensorZero gateway base URL (e.g. `http://127.0.0.1:3000`). The
    /// chat endpoint is appended automatically.
    pub gateway_url: String,
    /// Name of the chat function to invoke. Defaults to `"judge"` to
    /// match `e2e/tensorzero/config/tensorzero.toml`.
    pub function_name: String,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    /// When true, POST the score back to TensorZero `/feedback` so it
    /// shows up as `response_quality` in the gateway's metrics tables.
    pub submit_feedback: bool,
    /// Metric name used when `submit_feedback` is set. Defaults to
    /// `"response_quality"` (matches `tensorzero.toml`).
    pub feedback_metric: String,
    /// Optional system prompt prepended to the judge conversation. When
    /// `None` a built-in rubric is used.
    pub system_prompt: Option<String>,
}

impl Default for JudgeConfig {
    fn default() -> Self {
        Self {
            gateway_url: "http://127.0.0.1:3000".into(),
            function_name: "judge".into(),
            timeout_secs: 30,
            submit_feedback: true,
            feedback_metric: "response_quality".into(),
            system_prompt: None,
        }
    }
}

const DEFAULT_SYSTEM_PROMPT: &str = "You are an evaluation grader. \
You score how well an assistant answer satisfies the user's request. \
Reply with a single JSON object of the form \
{\"score\": <float between 0.0 and 1.0>, \"reasoning\": \"<one short sentence>\"}.";

/// What a judge produced for a single outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct JudgeResult {
    /// Score in `[0.0, 1.0]`. Values outside the range are clamped at
    /// parse time.
    pub score: f32,
    /// Optional human-readable reasoning surfaced by the judge.
    pub reasoning: Option<String>,
    /// `inference_id` returned by the gateway, if any. Tests use this to
    /// correlate with TensorZero's own metrics tables.
    pub inference_id: Option<String>,
}

/// Errors raised by [`Judge::judge`].
#[derive(Debug, Error)]
pub enum JudgeError {
    #[error("HTTP transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("gateway returned HTTP {status}: {body}")]
    GatewayStatus { status: u16, body: String },
    #[error("gateway response did not contain a JSON score: {body}")]
    Parse { body: String },
    #[error("score {0} is not finite")]
    NonFiniteScore(f32),
}

/// A trait so embedders can swap in a deterministic mock judge for
/// testing scoring logic without contacting TensorZero.
#[async_trait]
pub trait Judge: Send + Sync {
    async fn judge(
        &self,
        outcome: &ReplayOutcome,
        user_prompt: &str,
        rubric: Option<&str>,
    ) -> Result<JudgeResult, JudgeError>;
}

/// Bundled `Judge` that talks to a TensorZero gateway over OpenAI-compat
/// chat completions and (optionally) reports the score back through
/// `/feedback`.
pub struct TensorZeroJudge {
    client: reqwest::Client,
    config: JudgeConfig,
}

impl TensorZeroJudge {
    /// Construct a judge from `config`. The HTTP client is built with the
    /// configured timeout; falling back to a default reqwest client when
    /// timeout construction fails (e.g. in a no-network environment).
    pub fn new(config: JudgeConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client, config }
    }

    /// Convenience constructor pointing at the documented defaults.
    pub fn with_defaults() -> Self {
        Self::new(JudgeConfig::default())
    }

    pub fn config(&self) -> &JudgeConfig {
        &self.config
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/openai/v1/chat/completions",
            self.config.gateway_url.trim_end_matches('/')
        )
    }

    fn feedback_url(&self) -> String {
        format!("{}/feedback", self.config.gateway_url.trim_end_matches('/'))
    }

    fn build_payload(
        &self,
        outcome: &ReplayOutcome,
        user_prompt: &str,
        rubric: Option<&str>,
    ) -> serde_json::Value {
        let system = self
            .config
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string());
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": system}),
            serde_json::json!({"role": "user", "content": user_prompt}),
            serde_json::json!({
                "role": "assistant",
                "content": outcome.final_text.clone(),
            }),
        ];
        if let Some(rubric) = rubric {
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "Apply this rubric and respond with the JSON object only:\n{rubric}"
                ),
            }));
        } else {
            messages.push(serde_json::json!({
                "role": "user",
                "content":
                    "Score the assistant answer with respect to the original user request. Respond with the JSON object only.",
            }));
        }

        serde_json::json!({
            "model": format!("tensorzero::function_name::{}", self.config.function_name),
            "messages": messages,
        })
    }

    async fn submit_feedback_if_enabled(
        &self,
        inference_id: Option<&str>,
        score: f32,
    ) -> Result<(), JudgeError> {
        let Some(id) = inference_id else {
            return Ok(());
        };
        if !self.config.submit_feedback {
            return Ok(());
        }
        let payload = serde_json::json!({
            "inference_id": id,
            "metric_name": self.config.feedback_metric,
            "value": score,
        });
        let resp = self
            .client
            .post(self.feedback_url())
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                status,
                body,
                "TensorZero /feedback rejected response_quality submission"
            );
        }
        Ok(())
    }
}

#[async_trait]
impl Judge for TensorZeroJudge {
    async fn judge(
        &self,
        outcome: &ReplayOutcome,
        user_prompt: &str,
        rubric: Option<&str>,
    ) -> Result<JudgeResult, JudgeError> {
        let payload = self.build_payload(outcome, user_prompt, rubric);
        let resp = self
            .client
            .post(self.chat_url())
            .json(&payload)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(JudgeError::GatewayStatus { status, body });
        }
        let value: serde_json::Value = resp.json().await?;
        let inference_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string);
        let content = first_choice_content(&value).ok_or_else(|| JudgeError::Parse {
            body: value.to_string(),
        })?;
        let parsed = parse_score_payload(&content).ok_or(JudgeError::Parse { body: content })?;

        if !parsed.score.is_finite() {
            return Err(JudgeError::NonFiniteScore(parsed.score));
        }
        let score = parsed.score.clamp(0.0, 1.0);

        if let Some(id) = inference_id.as_deref() {
            self.submit_feedback_if_enabled(Some(id), score).await?;
        }

        Ok(JudgeResult {
            score,
            reasoning: parsed.reasoning,
            inference_id,
        })
    }
}

/// Pure scoring + LLM judge in one call.
///
/// Combines the deterministic checks of [`crate::score`] with a single judge
/// invocation, appending a [`Failure::JudgeBelowThreshold`] when the
/// returned score falls below `expect.min_judge_score`.
///
/// Returns `Ok((failures, judge_result))`. `judge_result` is `None` only
/// when `expect.min_judge_score` is `None`; in that case the judge is not
/// invoked at all (saves a round trip).
pub async fn score_with_judge(
    outcome: &ReplayOutcome,
    expect: &Expectation,
    user_prompt: &str,
    rubric: Option<&str>,
    judge: &dyn Judge,
) -> Result<(Vec<Failure>, Option<JudgeResult>), JudgeError> {
    let mut failures = crate::score::score(outcome, expect);
    let Some(threshold) = expect.min_judge_score else {
        return Ok((failures, None));
    };
    let result = judge.judge(outcome, user_prompt, rubric).await?;
    if result.score < threshold {
        failures.push(Failure::JudgeBelowThreshold {
            threshold,
            actual: result.score,
        });
    }
    Ok((failures, Some(result)))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn first_choice_content(value: &serde_json::Value) -> Option<String> {
    value
        .get("choices")?
        .as_array()?
        .first()?
        .get("message")?
        .get("content")?
        .as_str()
        .map(ToString::to_string)
}

#[derive(Debug, Deserialize, PartialEq)]
struct ScoreEnvelope {
    score: f32,
    #[serde(default)]
    reasoning: Option<String>,
}

/// Parse `content` as the score JSON. Supports either a bare object or one
/// embedded in a code fence (LLMs love decorating).
fn parse_score_payload(content: &str) -> Option<ScoreEnvelope> {
    if let Ok(v) = serde_json::from_str::<ScoreEnvelope>(content.trim()) {
        return Some(v);
    }
    let stripped = strip_code_fence(content);
    serde_json::from_str(stripped.trim()).ok()
}

fn strip_code_fence(content: &str) -> &str {
    let trimmed = content.trim();
    let without_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    without_open.trim_end_matches("```").trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_observability::AgentMetrics;
    use std::time::Duration;

    fn make_outcome(text: &str) -> ReplayOutcome {
        ReplayOutcome {
            fixture_id: "judge-test".into(),
            final_text: text.into(),
            metrics: AgentMetrics::default(),
            elapsed: Duration::from_millis(0),
        }
    }

    // ── JudgeConfig ─────────────────────────────────────────────────

    #[test]
    fn judge_config_default_targets_local_gateway() {
        let cfg = JudgeConfig::default();
        assert_eq!(cfg.gateway_url, "http://127.0.0.1:3000");
        assert_eq!(cfg.function_name, "judge");
        assert!(cfg.submit_feedback);
        assert_eq!(cfg.feedback_metric, "response_quality");
        assert!(cfg.system_prompt.is_none());
        assert!(cfg.timeout_secs > 0);
    }

    #[test]
    fn judge_config_allows_custom_function_name() {
        let cfg = JudgeConfig {
            function_name: "custom_judge".into(),
            ..JudgeConfig::default()
        };
        assert_eq!(cfg.function_name, "custom_judge");
    }

    // ── TensorZeroJudge: URL construction ───────────────────────────

    #[test]
    fn tz_judge_chat_url_has_openai_compat_suffix() {
        let judge = TensorZeroJudge::with_defaults();
        assert!(judge.chat_url().ends_with("/openai/v1/chat/completions"));
    }

    #[test]
    fn tz_judge_feedback_url_strips_trailing_slash() {
        let judge = TensorZeroJudge::new(JudgeConfig {
            gateway_url: "http://example:3000///".into(),
            ..JudgeConfig::default()
        });
        assert_eq!(judge.feedback_url(), "http://example:3000/feedback");
    }

    #[test]
    fn tz_judge_payload_pins_model_to_function_name() {
        let judge = TensorZeroJudge::with_defaults();
        let outcome = make_outcome("4");
        let payload = judge.build_payload(&outcome, "What is 2+2?", None);
        assert_eq!(
            payload.get("model").and_then(serde_json::Value::as_str),
            Some("tensorzero::function_name::judge")
        );
        let messages = payload
            .get("messages")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        // system + user + assistant + grading-instruction = 4
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "What is 2+2?");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "4");
        assert_eq!(messages[3]["role"], "user");
    }

    #[test]
    fn tz_judge_payload_includes_custom_rubric_when_provided() {
        let judge = TensorZeroJudge::with_defaults();
        let outcome = make_outcome("4");
        let payload = judge.build_payload(&outcome, "p", Some("Penalise verbosity"));
        let last = payload["messages"].as_array().unwrap().last().unwrap();
        assert!(
            last["content"]
                .as_str()
                .unwrap()
                .contains("Penalise verbosity")
        );
    }

    #[test]
    fn tz_judge_payload_uses_custom_system_prompt_when_set() {
        let judge = TensorZeroJudge::new(JudgeConfig {
            system_prompt: Some("Be strict.".into()),
            ..JudgeConfig::default()
        });
        let outcome = make_outcome("4");
        let payload = judge.build_payload(&outcome, "p", None);
        assert_eq!(
            payload["messages"][0]["content"].as_str().unwrap(),
            "Be strict."
        );
    }

    // ── parse_score_payload ─────────────────────────────────────────

    #[test]
    fn parse_score_accepts_bare_json_object() {
        let env = parse_score_payload(r#"{"score": 0.8, "reasoning": "ok"}"#).unwrap();
        assert!((env.score - 0.8).abs() < 1e-6);
        assert_eq!(env.reasoning.as_deref(), Some("ok"));
    }

    #[test]
    fn parse_score_accepts_code_fenced_object() {
        let payload = "```json\n{\"score\": 0.5}\n```";
        let env = parse_score_payload(payload).unwrap();
        assert!((env.score - 0.5).abs() < 1e-6);
        assert!(env.reasoning.is_none());
    }

    #[test]
    fn parse_score_accepts_unfenced_with_surrounding_whitespace() {
        let env = parse_score_payload("\n  {\"score\":1.0}  \n").unwrap();
        assert!((env.score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parse_score_returns_none_for_garbage() {
        assert!(parse_score_payload("not-json").is_none());
        assert!(parse_score_payload("").is_none());
    }

    #[test]
    fn parse_score_returns_none_when_score_field_missing() {
        assert!(parse_score_payload(r#"{"reasoning": "x"}"#).is_none());
    }

    // ── first_choice_content ────────────────────────────────────────

    #[test]
    fn first_choice_content_extracts_message_content() {
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": "0.9"}}]
        });
        assert_eq!(first_choice_content(&body).as_deref(), Some("0.9"));
    }

    #[test]
    fn first_choice_content_none_when_array_empty() {
        let body = serde_json::json!({"choices": []});
        assert!(first_choice_content(&body).is_none());
    }

    #[test]
    fn first_choice_content_none_when_field_missing() {
        let body = serde_json::json!({});
        assert!(first_choice_content(&body).is_none());
    }

    // ── score_with_judge with a stub Judge ──────────────────────────

    struct StubJudge {
        score: f32,
    }

    #[async_trait]
    impl Judge for StubJudge {
        async fn judge(
            &self,
            _outcome: &ReplayOutcome,
            _user_prompt: &str,
            _rubric: Option<&str>,
        ) -> Result<JudgeResult, JudgeError> {
            Ok(JudgeResult {
                score: self.score,
                reasoning: None,
                inference_id: None,
            })
        }
    }

    #[tokio::test]
    async fn score_with_judge_skips_judge_when_threshold_unset() {
        let outcome = make_outcome("");
        let expect = Expectation::default();
        let judge = StubJudge { score: 0.0 };
        let (failures, result) = score_with_judge(&outcome, &expect, "p", None, &judge)
            .await
            .unwrap();
        assert!(failures.is_empty());
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn score_with_judge_passes_when_score_meets_threshold() {
        let outcome = make_outcome("the answer is 4");
        let expect = Expectation {
            final_answer_contains: vec!["4".into()],
            min_judge_score: Some(0.7),
            ..Expectation::default()
        };
        let judge = StubJudge { score: 0.85 };
        let (failures, result) = score_with_judge(&outcome, &expect, "p", None, &judge)
            .await
            .unwrap();
        assert!(failures.is_empty());
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn score_with_judge_fails_when_score_below_threshold() {
        let outcome = make_outcome("the answer is 4");
        let expect = Expectation {
            min_judge_score: Some(0.7),
            ..Expectation::default()
        };
        let judge = StubJudge { score: 0.4 };
        let (failures, result) = score_with_judge(&outcome, &expect, "p", None, &judge)
            .await
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert!(matches!(
            failures[0],
            Failure::JudgeBelowThreshold { threshold, actual }
                if (threshold - 0.7).abs() < 1e-6 && (actual - 0.4).abs() < 1e-6
        ));
        let result = result.unwrap();
        assert!((result.score - 0.4).abs() < 1e-6);
    }

    #[tokio::test]
    async fn score_with_judge_combines_pure_failures_with_judge_failure() {
        let outcome = make_outcome("nothing");
        let expect = Expectation {
            final_answer_contains: vec!["4".into()],
            min_judge_score: Some(0.7),
            ..Expectation::default()
        };
        let judge = StubJudge { score: 0.4 };
        let (failures, _) = score_with_judge(&outcome, &expect, "p", None, &judge)
            .await
            .unwrap();
        let kinds: Vec<&str> = failures.iter().map(Failure::kind).collect();
        assert!(kinds.contains(&"answer_missing_phrase"));
        assert!(kinds.contains(&"judge_below_threshold"));
        assert_eq!(kinds.len(), 2);
    }
}
