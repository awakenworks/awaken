//! The opaque ownership coordinate a run executes under.
//!
//! The engine carries these values as labels and never interprets them.
//! `awaken-tenancy` re-exports both types and owns everything that gives them
//! meaning: authority resolution, verification and the persistence decorators
//! (ADR-0051, ADR-0086 §6).

use serde::{Deserialize, Serialize};

/// An opaque tenant/ownership handle. The core never interprets it — it does not
/// know whether the id denotes a workspace, an org, or any tier. Identity only.
///
/// It is deliberately a thin newtype: isolation is `WHERE scope_id = ?` at the
/// persistence boundary, and a change to the tenancy model leaves this type
/// untouched because the value is opaque.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ScopeId(pub String);

/// Opaque execution ownership coordinate carried by a durable dispatch.
///
/// This value is serializable and therefore is an identity claim, not proof of
/// authorization. An ingress edge must resolve it against an authenticated
/// `Authority` and pass the resulting `VerifiedExecutionScope` to trusted
/// composition code before constructing a dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionScopeRef(pub ScopeId);

impl ScopeId {
    /// Borrow the underlying id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ScopeId {
    fn from(value: String) -> Self {
        ScopeId(value)
    }
}

impl From<&str> for ScopeId {
    fn from(value: &str) -> Self {
        ScopeId(value.to_string())
    }
}

impl std::fmt::Display for ScopeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_coordinates_serialize_as_the_bare_string() {
        // The values moved out of `awaken-tenancy` unchanged: persisted dispatch
        // rows and wire payloads hold the bare string for both.
        let scope = ScopeId::from("workspace-a");
        assert_eq!(
            serde_json::to_value(&scope).unwrap(),
            serde_json::json!("workspace-a")
        );
        let execution = ExecutionScopeRef(scope.clone());
        let wire = serde_json::to_value(&execution).unwrap();
        assert_eq!(wire, serde_json::json!("workspace-a"));
        assert_eq!(
            serde_json::from_value::<ExecutionScopeRef>(wire).unwrap(),
            execution
        );
        assert_eq!(
            serde_json::from_value::<ScopeId>(serde_json::json!("workspace-a")).unwrap(),
            scope
        );
        assert_eq!(scope.as_str(), "workspace-a");
        assert_eq!(scope.to_string(), "workspace-a");
        assert_eq!(ScopeId::from(String::from("workspace-a")), scope);
    }
}
