//! Secret-free credential revision adoption protocol.
//!
//! The Credential aggregate publishes this wire fact after committing an exact
//! source/managed-child pair. Consumers may be local or remote, but they only
//! acknowledge convergence after every target in their own bounded context has
//! adopted or retired the named revision.

use serde::{Deserialize, Serialize};

use crate::CredentialSourceId;

/// The closed command vocabulary carried by durable credential mutations and
/// their adoption events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedCredentialOperation {
    Create,
    Update,
    Archive,
    Delete,
}

/// One exact, secret-free credential fence that online consumers must adopt.
///
/// Target discovery, topology and rollout policy deliberately do not appear in
/// this event. A consumer freezes those facts within its own bounded context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedCredentialRollout {
    pub id: String,
    pub workspace_id: String,
    pub vault_id: String,
    pub credential_id: String,
    pub source_id: CredentialSourceId,
    pub source_version: u64,
    pub credential_revision: u64,
    pub operation: ManagedCredentialOperation,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollout_wire_is_secret_free_and_round_trips() {
        let event = ManagedCredentialRollout {
            id: "rollout-1".into(),
            workspace_id: "ws".into(),
            vault_id: "vault".into(),
            credential_id: "credential".into(),
            source_id: CredentialSourceId("source".into()),
            source_version: 2,
            credential_revision: 2,
            operation: ManagedCredentialOperation::Update,
        };
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("secret"));
        assert_eq!(
            serde_json::from_str::<ManagedCredentialRollout>(&wire).unwrap(),
            event
        );
    }
}
