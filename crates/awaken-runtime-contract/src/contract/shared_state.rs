//! Shared state scoping for cross-boundary persistence.
//!
//! Shared state reuses the [`ProfileKey`](super::profile_store::ProfileKey)
//! mechanism with [`StateScope`] as the dynamic key dimension.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Identifies a shared state scope at runtime.
///
/// A plain string that serves as the key dimension in `ProfileAccess`.
/// Convenience constructors cover common patterns; arbitrary strings
/// are accepted via `StateScope::new`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StateScope(String);

impl StateScope {
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    pub fn global() -> Self {
        Self("global".into())
    }

    pub fn parent_thread(id: &str) -> Self {
        Self(format!("parent_thread::{id}"))
    }

    pub fn agent_type(name: &str) -> Self {
        Self(format!("agent_type::{name}"))
    }

    pub fn thread(id: &str) -> Self {
        Self(format!("thread::{id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StateScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_scope_display() {
        assert_eq!(StateScope::global().to_string(), "global");
        assert_eq!(
            StateScope::parent_thread("t1").to_string(),
            "parent_thread::t1"
        );
        assert_eq!(
            StateScope::agent_type("planner").to_string(),
            "agent_type::planner"
        );
        assert_eq!(StateScope::thread("abc").to_string(), "thread::abc");
        assert_eq!(StateScope::new("custom").to_string(), "custom");
    }

    #[test]
    fn state_scope_equality() {
        assert_eq!(StateScope::global(), StateScope::global());
        assert_ne!(StateScope::global(), StateScope::thread("x"));
    }

    #[test]
    fn state_scope_serde_roundtrip() {
        let scopes = vec![
            StateScope::global(),
            StateScope::parent_thread("p1"),
            StateScope::agent_type("worker"),
            StateScope::thread("t1"),
            StateScope::new("custom"),
        ];
        for scope in scopes {
            let json = serde_json::to_string(&scope).expect("serialize");
            let back: StateScope = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(scope, back);
        }
    }
}
