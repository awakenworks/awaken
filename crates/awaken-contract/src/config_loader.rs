//! Generic configuration loading utilities for YAML/JSON config files.
//!
//! Provides [`load_config_from_file`] and [`load_config_from_str`] that
//! handle file I/O and format detection, used by plugin config modules
//! to avoid duplicating the same parsing logic.

use std::path::Path;

use serde::de::DeserializeOwned;

/// Load a deserializable config from a file path.
///
/// Reads the file and delegates to [`load_config_from_str`] with the
/// file extension as format hint.
pub fn load_config_from_file<T: DeserializeOwned>(
    path: impl AsRef<Path>,
) -> Result<T, ConfigLoadError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)?;
    load_config_from_str(&content, path.extension().and_then(|e| e.to_str()))
}

/// Parse a deserializable config from a string with an optional format hint.
///
/// If `ext` is `Some("json")`, parses as JSON directly. Otherwise,
/// auto-detects: if the trimmed content starts with `{` or `[`, parses
/// as JSON; otherwise falls back to JSON parsing (future: YAML support).
pub fn load_config_from_str<T: DeserializeOwned>(
    content: &str,
    ext: Option<&str>,
) -> Result<T, ConfigLoadError> {
    match ext {
        Some("json") => {
            serde_json::from_str(content).map_err(|e| ConfigLoadError::Parse(e.to_string()))
        }
        _ => {
            let trimmed = content.trim();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                serde_json::from_str(content).map_err(|e| ConfigLoadError::Parse(e.to_string()))
            } else {
                serde_json::from_str(content).map_err(|e| ConfigLoadError::Parse(e.to_string()))
            }
        }
    }
}

/// Error type for generic configuration loading.
#[derive(Debug, thiserror::Error)]
pub enum ConfigLoadError {
    /// File I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Deserialization error.
    #[error("parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct TestConfig {
        name: String,
        value: u32,
    }

    #[test]
    fn parse_json_with_hint() {
        let json = r#"{"name": "test", "value": 42}"#;
        let config: TestConfig = load_config_from_str(json, Some("json")).unwrap();
        assert_eq!(config.name, "test");
        assert_eq!(config.value, 42);
    }

    #[test]
    fn parse_json_auto_detect() {
        let json = r#"{"name": "auto", "value": 1}"#;
        let config: TestConfig = load_config_from_str(json, None).unwrap();
        assert_eq!(config.name, "auto");
    }

    #[test]
    fn parse_json_array_auto_detect() {
        let json = r#"[{"name": "a", "value": 1}]"#;
        let configs: Vec<TestConfig> = load_config_from_str(json, None).unwrap();
        assert_eq!(configs.len(), 1);
    }

    #[test]
    fn parse_error_on_invalid_json() {
        let result: Result<TestConfig, _> = load_config_from_str("not json", Some("json"));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse error"));
    }

    #[test]
    fn io_error_on_missing_file() {
        let result: Result<TestConfig, _> = load_config_from_file("/nonexistent/file.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("io error"));
    }
}
