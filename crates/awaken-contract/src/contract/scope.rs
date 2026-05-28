use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_SCOPE_ID: &str = "default";
pub const MAX_SCOPE_ID_LEN: usize = 512;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeError {
    #[error("scope_id cannot be empty")]
    Empty,
    #[error("scope_id cannot exceed {max} bytes")]
    TooLong { max: usize },
    #[error("scope_id cannot contain control characters")]
    ControlCharacter,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScopeId(String);

impl ScopeId {
    pub fn new(value: impl Into<String>) -> Result<Self, ScopeError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ScopeError::Empty);
        }
        if value.len() > MAX_SCOPE_ID_LEN {
            return Err(ScopeError::TooLong {
                max: MAX_SCOPE_ID_LEN,
            });
        }
        if value.chars().any(char::is_control) {
            return Err(ScopeError::ControlCharacter);
        }
        Ok(Self(value))
    }

    pub fn default_scope() -> Self {
        Self(DEFAULT_SCOPE_ID.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ScopeId {
    fn default() -> Self {
        Self::default_scope()
    }
}

impl TryFrom<String> for ScopeId {
    type Error = ScopeError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ScopeId {
    type Error = ScopeError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ScopeId> for String {
    fn from(value: ScopeId) -> Self {
        value.0
    }
}

impl From<&ScopeId> for String {
    fn from(value: &ScopeId) -> Self {
        value.as_str().to_string()
    }
}

impl AsRef<str> for ScopeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeContext {
    pub scope_id: ScopeId,
}

pub fn scoped_key(scope_id: &ScopeId, value: &str) -> String {
    let scope = scope_id.as_str();
    format!("scope:{}:{}:{}", scope.len(), scope, value)
}

pub fn unscoped_key<'a>(scope_id: &ScopeId, value: &'a str) -> Option<&'a str> {
    let scope = scope_id.as_str();
    let prefix = format!("scope:{}:{}:", scope.len(), scope);
    value.strip_prefix(&prefix)
}

impl ScopeContext {
    pub fn new(scope_id: ScopeId) -> Self {
        Self { scope_id }
    }

    pub fn default_scope() -> Self {
        Self::new(ScopeId::default_scope())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestSurface {
    Admin,
    AgentInvoke,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_id_rejects_empty_values() {
        assert_eq!(ScopeId::new(""), Err(ScopeError::Empty));
        assert_eq!(ScopeId::new("   "), Err(ScopeError::Empty));
    }

    #[test]
    fn scope_id_rejects_control_characters() {
        assert_eq!(
            ScopeId::new("workspace\n1"),
            Err(ScopeError::ControlCharacter)
        );
    }

    #[test]
    fn scope_id_round_trips_as_string() {
        let scope = ScopeId::new("workspace_123").unwrap();
        let encoded = serde_json::to_string(&scope).unwrap();
        assert_eq!(encoded, "\"workspace_123\"");
        let decoded: ScopeId = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, scope);
    }
}
