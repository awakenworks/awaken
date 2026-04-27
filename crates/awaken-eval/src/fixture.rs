//! Fixture format and loader.
//!
//! Each fixture is a single JSON file describing a deterministic scenario:
//! a user prompt, a scripted assistant response (consumed by the M4.3 mock
//! executor), and an [`Expectation`] block declaring success criteria.
//!
//! The format is intentionally minimal in 0.4.1 so it can grow without
//! breaking older fixtures: unknown fields are accepted on deserialise and
//! [`Fixture`] gains new optional fields additively.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::expectation::Expectation;

/// A single replayable scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fixture {
    /// Stable identifier — used as the report key.  Must be unique across a
    /// fixtures directory.
    pub id: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// User prompt that drives the run.
    pub user_input: String,
    /// What the mock LLM returns when replayed without a real model.
    #[serde(default)]
    pub mock_response: MockResponse,
    /// Success criteria.
    #[serde(default)]
    pub expect: Expectation,
}

/// What the mock LLM should return for a fixture run.
///
/// `MockResponse::Text` is the only variant in 0.4.1; richer modes (tool
/// calls, multi-round) are deliberately deferred until the replay engine
/// surfaces them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockResponse {
    /// Return a single assistant text block, usage zero.
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
}
