//! Fixture format and loader.
//!
//! Each fixture is a single JSON file describing a deterministic scenario:
//! a user prompt, the upstream `provider_script` to replay, and an
//! [`Expectation`] block declaring success criteria.
//!
//! Legacy fixtures that only set `mock_response` keep loading; their
//! response is shimmed into a single-element `provider_script` by
//! [`Fixture::effective_script`].

use std::fs;
use std::path::{Path, PathBuf};

use awaken_contract::contract::inference::TokenUsage;
use awaken_runtime::engine::ProviderScriptEvent;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::expectation::Expectation;

/// A single replayable scenario.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fixture {
    /// Stable identifier — used as the report key.  Must be unique across a
    /// fixtures directory.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// User prompt that drives the run.
    pub user_input: String,
    /// Upstream events the [`ScriptedLlmExecutor`](awaken_runtime::engine::ScriptedLlmExecutor)
    /// returns when this fixture is replayed. Empty for legacy fixtures —
    /// see [`Fixture::effective_script`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_script: Vec<ProviderScriptEvent>,
    /// Originating production `run_id` when this fixture was curated from
    /// a trace via `POST /v1/eval/datasets/:id/items`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_run_id: Option<String>,
    /// Originating model id for the curated trace. Used as a mismatch
    /// guard at replay time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model_id: Option<String>,
    /// When `true`, replay tolerates `provider_script` events that the
    /// runtime never consumed (default: `false`). Use only for fixtures
    /// where the runtime is genuinely allowed to stop early; otherwise
    /// the assert catches dropped rounds, missed tool calls, or absent
    /// retries that would otherwise pass silently on the legacy
    /// "final_text only" expectation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_unused_provider_script: bool,
    /// Legacy single-response field. Superseded by [`Self::provider_script`]
    /// and removed once all fixtures have been migrated.
    #[serde(default)]
    pub mock_response: MockResponse,
    /// Success criteria.
    #[serde(default)]
    pub expect: Expectation,
    /// Additional dialogue turns after the initial `user_input`. When
    /// non-empty, [`RuntimeReplayer`] runs N+1 successive agent loops on
    /// the same thread (turn 0 = `user_input` + `provider_script`, turns
    /// 1..N = `continued_turns[i].user_input` + their scripts), so
    /// multi-user-turn conversations replay faithfully.
    ///
    /// Empty by default; legacy single-turn fixtures behave unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continued_turns: Vec<DialogueTurn>,
}

/// One follow-up user turn in a multi-turn dialogue fixture. Mirrors the
/// `(user_input, provider_script)` pair on [`Fixture`] but without the
/// fixture-wide metadata (id / source_run_id / expect — those live on
/// the parent `Fixture` only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DialogueTurn {
    pub user_input: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_script: Vec<ProviderScriptEvent>,
}

/// Legacy single-turn response specifier, superseded by
/// [`Fixture::provider_script`]. Kept so already-committed fixtures keep
/// loading; new fixtures should use `provider_script` directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockResponse {
    /// Return a single assistant text block. [`Fixture::effective_script`]
    /// shims this into a `ChatResponse` event with token counts seeded by
    /// a `chars / 4` heuristic so `max_tokens_total` keeps having teeth
    /// for un-migrated fixtures.
    Text { text: String },
    /// Return an inference error of the given type.
    Error { error_type: String, message: String },
}

impl Default for MockResponse {
    fn default() -> Self {
        Self::Text {
            text: String::new(),
        }
    }
}

/// Errors raised by [`Fixture::load`] / [`load_directory`].
#[derive(Debug, Error)]
pub enum FixtureError {
    #[error("fixture path is not readable: {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("fixture {path} is not valid JSON")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("fixture directory contains duplicate id {id}")]
    DuplicateId { id: String },
}

impl Fixture {
    /// Load a fixture from a JSON file on disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, FixtureError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| FixtureError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice(&bytes).map_err(|source| FixtureError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Parse a fixture from an in-memory JSON string. Useful for tests.
    pub fn from_json(input: &str) -> Result<Self, FixtureError> {
        serde_json::from_str(input).map_err(|source| FixtureError::Parse {
            path: PathBuf::from("<inline>"),
            source,
        })
    }

    /// Provider events to drive a [`ScriptedLlmExecutor`](awaken_runtime::engine::ScriptedLlmExecutor)
    /// for this fixture. Prefers the explicit [`Self::provider_script`];
    /// falls back to a single-element script synthesised from
    /// [`Self::mock_response`] for legacy fixtures.
    ///
    /// Legacy `MockResponse::Text` does not carry a token count, so the
    /// shim seeds tokens via a `chars / 4` heuristic. Preserving this
    /// preserves the meaning of `max_tokens_total` for any fixture that
    /// hasn't been migrated to explicit `provider_script` — otherwise
    /// legacy budget assertions would silently pass against 0-token
    /// usage.
    pub fn effective_script(&self) -> Vec<ProviderScriptEvent> {
        if !self.provider_script.is_empty() {
            return self.provider_script.clone();
        }
        match &self.mock_response {
            MockResponse::Text { text } => {
                let prompt_tokens = approximate_tokens(&self.user_input);
                let completion_tokens = approximate_tokens(text);
                let total_tokens = prompt_tokens.saturating_add(completion_tokens);
                vec![ProviderScriptEvent::ChatResponse {
                    content: text.clone(),
                    tokens: TokenUsage {
                        prompt_tokens: Some(prompt_tokens),
                        completion_tokens: Some(completion_tokens),
                        total_tokens: Some(total_tokens),
                        ..Default::default()
                    },
                    finish_reason: awaken_contract::contract::inference::StopReason::EndTurn,
                }]
            }
            MockResponse::Error {
                error_type,
                message,
            } => vec![ProviderScriptEvent::Error {
                error_type: error_type.clone(),
                message: message.clone(),
            }],
        }
    }

    /// `effective_script` (turn 0) concatenated with every
    /// [`DialogueTurn::provider_script`]. The replayer consumes this as
    /// one combined script — the [`ScriptedLlmExecutor`] pointer advances
    /// naturally as each turn's agent loop pulls events.
    pub fn combined_script(&self) -> Vec<ProviderScriptEvent> {
        let mut script = self.effective_script();
        for turn in &self.continued_turns {
            script.extend(turn.provider_script.iter().cloned());
        }
        script
    }
}

/// Coarse `chars / 4` token estimate. Used by [`Fixture::effective_script`]
/// to seed legacy `MockResponse::Text` fixtures with a non-zero token usage
/// so budget assertions don't silently pass on missing data.
fn approximate_tokens(text: &str) -> i32 {
    let chars = text.chars().count();
    let tokens = chars.div_ceil(4);
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

/// Load every `*.json` file in `dir` as a [`Fixture`], returning them sorted
/// by `id` for deterministic iteration.
///
/// Files are skipped silently when their name starts with `.`. Returns an
/// error when two fixtures share the same `id`.
pub fn load_directory(dir: impl AsRef<Path>) -> Result<Vec<Fixture>, FixtureError> {
    let dir = dir.as_ref();
    let entries = fs::read_dir(dir).map_err(|source| FixtureError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut fixtures: Vec<Fixture> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| FixtureError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        fixtures.push(Fixture::load(&path)?);
    }

    fixtures.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::HashSet::new();
    for fx in &fixtures {
        if !seen.insert(fx.id.clone()) {
            return Err(FixtureError::DuplicateId { id: fx.id.clone() });
        }
    }
    Ok(fixtures)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir(suffix: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!("awaken-eval-fixture-{suffix}-{now}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_fixture(dir: &Path, name: &str, json: &str) {
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
    }

    fn sample_json(id: &str) -> String {
        format!(
            r#"{{"id": "{id}", "user_input": "hi", "expect": {{"final_answer_contains": ["hello"]}}}}"#
        )
    }

    // ── MockResponse ────────────────────────────────────────────────

    #[test]
    fn mock_response_default_is_empty_text() {
        let mr = MockResponse::default();
        match mr {
            MockResponse::Text { text } => assert!(text.is_empty()),
            other => panic!("expected Text default, got {other:?}"),
        }
    }

    #[test]
    fn mock_response_text_serde_roundtrip() {
        let mr = MockResponse::Text {
            text: "answer 42".into(),
        };
        let json = serde_json::to_string(&mr).unwrap();
        assert!(json.contains(r#""kind":"text""#));
        let parsed: MockResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mr);
    }

    #[test]
    fn mock_response_error_serde_roundtrip() {
        let mr = MockResponse::Error {
            error_type: "rate_limit".into(),
            message: "429".into(),
        };
        let json = serde_json::to_string(&mr).unwrap();
        assert!(json.contains(r#""kind":"error""#));
        let parsed: MockResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mr);
    }

    // ── Fixture from_json / load ────────────────────────────────────

    #[test]
    fn fixture_from_json_minimal_succeeds() {
        let fx = Fixture::from_json(r#"{"id": "x", "user_input": "hi"}"#).unwrap();
        assert_eq!(fx.id, "x");
        assert_eq!(fx.user_input, "hi");
        assert!(fx.description.is_none());
        assert!(fx.expect.is_empty());
        match fx.mock_response {
            MockResponse::Text { text } => assert!(text.is_empty()),
            _ => panic!("default mock_response must be empty Text"),
        }
    }

    #[test]
    fn fixture_from_json_full_succeeds() {
        let json = r#"{
            "id": "calc",
            "description": "calculator tool",
            "user_input": "Multiply 6 by 7.",
            "mock_response": {"kind": "text", "text": "42"},
            "expect": {
                "final_answer_contains": ["42"],
                "tool_sequence": ["calculator"],
                "forbidden_tools": ["delete"],
                "max_tokens_total": 1000,
                "max_duration_ms": 5000
            }
        }"#;
        let fx = Fixture::from_json(json).unwrap();
        assert_eq!(fx.id, "calc");
        assert_eq!(fx.description.as_deref(), Some("calculator tool"));
        assert_eq!(fx.expect.final_answer_contains, vec!["42".to_string()]);
        assert_eq!(fx.expect.tool_sequence, vec!["calculator".to_string()]);
        assert_eq!(fx.expect.max_tokens_total, Some(1000));
    }

    #[test]
    fn fixture_from_json_rejects_garbage() {
        let err = Fixture::from_json("not-json").unwrap_err();
        match err {
            FixtureError::Parse { .. } => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn fixture_from_json_rejects_missing_id() {
        let err = Fixture::from_json(r#"{"user_input": "hi"}"#).unwrap_err();
        match err {
            FixtureError::Parse { .. } => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn fixture_load_returns_io_error_for_missing_file() {
        let err = Fixture::load("/nonexistent/awaken-eval/missing.json").unwrap_err();
        match err {
            FixtureError::Io { .. } => {}
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn fixture_load_round_trips_through_disk() {
        let dir = temp_dir("load-disk");
        let path = dir.join("fixture.json");
        let json = sample_json("disk");
        fs::write(&path, &json).unwrap();
        let fx = Fixture::load(&path).unwrap();
        assert_eq!(fx.id, "disk");
        let _ = fs::remove_dir_all(&dir);
    }

    // ── load_directory ──────────────────────────────────────────────

    #[test]
    fn load_directory_returns_sorted_fixtures() {
        let dir = temp_dir("sorted");
        write_fixture(&dir, "b.json", &sample_json("beta"));
        write_fixture(&dir, "a.json", &sample_json("alpha"));
        write_fixture(&dir, "c.json", &sample_json("gamma"));
        let fixtures = load_directory(&dir).unwrap();
        let ids: Vec<_> = fixtures.iter().map(|f| f.id.clone()).collect();
        assert_eq!(ids, vec!["alpha", "beta", "gamma"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_directory_ignores_non_json() {
        let dir = temp_dir("non-json");
        write_fixture(&dir, "fixture.json", &sample_json("only"));
        write_fixture(&dir, "README.txt", "ignore me");
        write_fixture(&dir, ".hidden.json", &sample_json("hidden"));
        let fixtures = load_directory(&dir).unwrap();
        assert_eq!(fixtures.len(), 1);
        assert_eq!(fixtures[0].id, "only");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_directory_detects_duplicate_ids() {
        let dir = temp_dir("duplicate");
        write_fixture(&dir, "a.json", &sample_json("dup"));
        write_fixture(&dir, "b.json", &sample_json("dup"));
        let err = load_directory(&dir).unwrap_err();
        match err {
            FixtureError::DuplicateId { id } => assert_eq!(id, "dup"),
            other => panic!("expected DuplicateId, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_directory_returns_empty_for_empty_dir() {
        let dir = temp_dir("empty");
        let fixtures = load_directory(&dir).unwrap();
        assert!(fixtures.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_directory_propagates_parse_errors() {
        let dir = temp_dir("garbage");
        write_fixture(&dir, "bad.json", "not-json");
        let err = load_directory(&dir).unwrap_err();
        match err {
            FixtureError::Parse { .. } => {}
            other => panic!("expected Parse error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ── provider_script / effective_script ──────────────────────────

    #[test]
    fn effective_script_uses_provider_script_when_present() {
        let json = r#"{
            "id": "explicit",
            "user_input": "hi",
            "provider_script": [
                {"kind": "chat_response", "content": "hi back"},
                {"kind": "tool_call", "id": "t1", "name": "noop", "arguments": {}}
            ]
        }"#;
        let fx = Fixture::from_json(json).unwrap();
        let script = fx.effective_script();
        assert_eq!(script.len(), 2);
        match &script[0] {
            ProviderScriptEvent::ChatResponse { content, .. } => assert_eq!(content, "hi back"),
            other => panic!("expected chat_response, got {other:?}"),
        }
        match &script[1] {
            ProviderScriptEvent::ToolCall { name, .. } => assert_eq!(name, "noop"),
            other => panic!("expected tool_call, got {other:?}"),
        }
    }

    #[test]
    fn effective_script_shims_legacy_text_mock_response() {
        let fx =
            Fixture::from_json(r#"{"id": "legacy", "user_input": "hi", "mock_response": {"kind": "text", "text": "ok"}}"#)
                .unwrap();
        let script = fx.effective_script();
        assert_eq!(script.len(), 1);
        match &script[0] {
            ProviderScriptEvent::ChatResponse { content, .. } => assert_eq!(content, "ok"),
            other => panic!("expected chat_response, got {other:?}"),
        }
    }

    #[test]
    fn effective_script_legacy_text_seeds_non_zero_token_estimate() {
        // Regression guard for the review #1 finding: the legacy shim must
        // populate TokenUsage instead of defaulting to zero, otherwise
        // `max_tokens_total` assertions trivially pass against legacy
        // fixtures even when the budget is exceeded.
        let fx = Fixture::from_json(
            r#"{
              "id": "legacy_budget",
              "user_input": "Long enough prompt to exceed one token",
              "mock_response": {"kind": "text", "text": "A reply that is also longer than a single token bucket"}
            }"#,
        )
        .unwrap();
        let script = fx.effective_script();
        match &script[0] {
            ProviderScriptEvent::ChatResponse { tokens, .. } => {
                assert!(
                    tokens.prompt_tokens.unwrap_or(0) > 0,
                    "expected non-zero prompt tokens, got {tokens:?}"
                );
                assert!(
                    tokens.completion_tokens.unwrap_or(0) > 0,
                    "expected non-zero completion tokens, got {tokens:?}"
                );
                assert!(
                    tokens.total_tokens.unwrap_or(0) > 0,
                    "expected non-zero total tokens, got {tokens:?}"
                );
            }
            other => panic!("expected chat_response, got {other:?}"),
        }
    }

    #[test]
    fn effective_script_shims_legacy_error_mock_response() {
        let fx = Fixture::from_json(
            r#"{
              "id": "legacy_err",
              "user_input": "hi",
              "mock_response": {"kind": "error", "error_type": "rate_limit", "message": "429"}
            }"#,
        )
        .unwrap();
        let script = fx.effective_script();
        assert_eq!(script.len(), 1);
        match &script[0] {
            ProviderScriptEvent::Error {
                error_type,
                message,
            } => {
                assert_eq!(error_type, "rate_limit");
                assert_eq!(message, "429");
            }
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[test]
    fn effective_script_shims_empty_default_mock_response() {
        // No mock_response, no provider_script -> single empty-text event.
        let fx = Fixture::from_json(r#"{"id": "empty", "user_input": "hi"}"#).unwrap();
        let script = fx.effective_script();
        assert_eq!(script.len(), 1);
        match &script[0] {
            ProviderScriptEvent::ChatResponse { content, .. } => assert!(content.is_empty()),
            other => panic!("expected empty chat_response, got {other:?}"),
        }
    }

    #[test]
    fn fixture_round_trips_provider_script_and_source_metadata() {
        let json = r#"{
            "id": "curated",
            "user_input": "hi",
            "provider_script": [{"kind": "chat_response", "content": "hi"}],
            "source_run_id": "01HXYZ",
            "source_model_id": "claude-opus-4-7"
        }"#;
        let fx = Fixture::from_json(json).unwrap();
        assert_eq!(fx.source_run_id.as_deref(), Some("01HXYZ"));
        assert_eq!(fx.source_model_id.as_deref(), Some("claude-opus-4-7"));
        let reserialised = serde_json::to_string(&fx).unwrap();
        let round: Fixture = serde_json::from_str(&reserialised).unwrap();
        assert_eq!(round, fx);
    }
}
