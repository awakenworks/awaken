//! Read ports for the authored control-plane aggregates — the store traits the
//! resolver and the runtime host consult at bind time.
//!
//! These live here, with the aggregate types they carry, so the **read** path
//! stays in the open resolver crate and never reaches back up into the authoring
//! HTTP surface. `awaken-admin-config-api` (the authoring plane) writes *through*
//! these same ports and provides the durable SQLite backend for them; the
//! runtime host reads *through* them at session-prepare time.

use crate::{AgentInputConfig, InferenceProfile, WebhookEndpointDef};

/// The delivery outcome persisted against one webhook subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookDeliveryOutcome {
    Succeeded,
    Failed,
}

/// The durable state after applying one delivery outcome. `Missing` means the
/// subscription was concurrently deleted and therefore has no future delivery
/// obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookDeliveryState {
    Missing,
    Active { consecutive_failures: u32 },
    Disabled { consecutive_failures: u32 },
}

/// Infrastructure failure from a synchronous authored-config repository.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigRepositoryError {
    #[error("config repository storage failure: {0}")]
    Storage(String),
}

/// A store for authored [`InferenceProfile`]s (an admin-plane aggregate). Sync +
/// in-memory by default; a durable backend can implement the same port.
pub trait InferenceProfileStore: Send + Sync {
    fn put(&self, id: String, profile: InferenceProfile) -> Result<(), ConfigRepositoryError>;
    fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError>;
}

/// Opaque durable key for a profile id inside one trusted Workspace. Length
/// framing makes the mapping injective even when either coordinate contains `:`.
#[must_use]
pub fn workspace_profile_key(workspace_id: &str, profile_id: &str) -> String {
    format!("ws:{}:{workspace_id}:{profile_id}", workspace_id.len())
}

/// Read a Workspace-owned profile without allowing a common id such as
/// a shared profile id to collide across tenants. A matching unscoped row
/// remains readable for in-place upgrades; a row owned by another Workspace is
/// treated as absent.
pub fn get_workspace_profile(
    store: &dyn InferenceProfileStore,
    workspace_id: &str,
    profile_id: &str,
) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
    if let Some(profile) = store.get(&workspace_profile_key(workspace_id, profile_id))? {
        return Ok((profile.workspace_id == workspace_id).then_some(profile));
    }
    Ok(store
        .get(profile_id)?
        .filter(|profile| profile.workspace_id == workspace_id))
}

/// Persist under the Workspace-qualified key. Callers must stamp and validate the
/// trusted owner before invoking this helper.
pub fn put_workspace_profile(
    store: &dyn InferenceProfileStore,
    workspace_id: &str,
    profile_id: &str,
    profile: InferenceProfile,
) -> Result<(), ConfigRepositoryError> {
    store.put(workspace_profile_key(workspace_id, profile_id), profile)
}

/// A store for authored [`WebhookEndpointDef`]s (an admin-plane aggregate,
/// ADR-0048). Product composition supplies a durable admin backend; a process-local
/// reference implementation is available only under `test-support`. Unlike
/// [`InferenceProfileStore`] it enumerates by
/// workspace (dispatch fan-out) and supports delete (unsubscribe).
pub trait WebhookStore: Send + Sync {
    fn put(&self, def: WebhookEndpointDef) -> Result<(), ConfigRepositoryError>;
    fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Every endpoint owned by `workspace_id` (including disabled), for CRUD list
    /// and dispatch matching.
    fn list(&self, workspace_id: &str) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Remove by id; `true` if a row was removed (idempotent unsubscribe).
    fn delete(&self, id: &str) -> Result<bool, ConfigRepositoryError>;
    /// Atomically apply one delivery result to the existing row. A success resets
    /// the consecutive-failure count; a failure increments it and disables the row
    /// at `failure_threshold`. Missing rows are a terminal no-op.
    fn record_delivery(
        &self,
        id: &str,
        outcome: WebhookDeliveryOutcome,
        failure_threshold: u32,
    ) -> Result<WebhookDeliveryState, ConfigRepositoryError>;
}

/// Repository for an Agent's default input bindings. Workspace is mandatory on
/// every operation: ownership is an intrinsic aggregate key, while caller identity
/// and policy remain outside this port.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentInputRepositoryError {
    #[error("Agent input revision must be positive, got {0}")]
    InvalidRevision(i64),
    #[error("Agent input revision conflict: current {current}, attempted {attempted}")]
    RevisionConflict { current: i64, attempted: i64 },
    #[error("Agent input repository storage failure: {0}")]
    Storage(String),
}

/// Validate the aggregate revision transition. `Ok(false)` is an idempotent
/// replay and therefore requires no write.
pub fn validate_agent_input_revision(
    current: Option<&AgentInputConfig>,
    next: &AgentInputConfig,
) -> Result<bool, AgentInputRepositoryError> {
    if next.revision < 1 {
        return Err(AgentInputRepositoryError::InvalidRevision(next.revision));
    }
    match current {
        Some(current) if current == next => Ok(false),
        Some(current) if next.revision == current.revision + 1 => Ok(true),
        Some(current) => Err(AgentInputRepositoryError::RevisionConflict {
            current: current.revision,
            attempted: next.revision,
        }),
        None if next.revision == 1 => Ok(true),
        None => Err(AgentInputRepositoryError::RevisionConflict {
            current: 0,
            attempted: next.revision,
        }),
    }
}

pub trait AgentInputBindingRepository: Send + Sync {
    /// Apply revision 1 to a new aggregate or exactly current+1 to an existing
    /// aggregate. Replaying byte-equivalent state at the current revision is
    /// idempotent; stale/skipped revisions fail closed.
    fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError>;
    fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError>;
    fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BindingId, FileId, InputBinding, InputResourceId, ResourceAccess};
    use crate::{InMemoryAgentInputBindingRepository, InMemoryWebhookStore};

    #[test]
    fn agent_inputs_are_isolated_by_workspace_even_for_the_same_agent_id() {
        let store = InMemoryAgentInputBindingRepository::new();
        let config = |resource_id: &str| AgentInputConfig {
            agent_id: "shared-agent".into(),
            environment: None,
            inputs: vec![InputBinding {
                binding_id: BindingId::from("input"),
                target: InputResourceId::File(FileId::from(resource_id)),
                mount_path: "/workspace/input.txt".into(),
                access: ResourceAccess::ReadOnly,
                instructions: None,
            }],
            revision: 1,
        };

        store
            .put_agent_inputs("workspace-a", config("file-a"))
            .unwrap();
        store
            .put_agent_inputs("workspace-b", config("file-b"))
            .unwrap();

        assert_eq!(
            store
                .get_agent_inputs("workspace-a", "shared-agent")
                .unwrap()
                .unwrap()
                .inputs[0]
                .target
                .id(),
            "file-a"
        );
        assert_eq!(
            store
                .get_agent_inputs("workspace-b", "shared-agent")
                .unwrap()
                .unwrap()
                .inputs[0]
                .target
                .id(),
            "file-b"
        );
        assert!(
            store
                .get_agent_inputs("workspace-c", "shared-agent")
                .unwrap()
                .is_none()
        );
        assert_eq!(store.list_agent_inputs("workspace-a").unwrap().len(), 1);
        assert_eq!(
            store.list_agent_inputs("workspace-a").unwrap()[0].inputs[0]
                .target
                .id(),
            "file-a"
        );
        assert!(store.list_agent_inputs("workspace-c").unwrap().is_empty());
    }

    #[test]
    fn agent_input_revisions_are_sequential_and_replays_are_idempotent() {
        let store = InMemoryAgentInputBindingRepository::new();
        let config = |revision| AgentInputConfig {
            agent_id: "agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision,
        };

        store.put_agent_inputs("workspace", config(1)).unwrap();
        store.put_agent_inputs("workspace", config(1)).unwrap();
        assert_eq!(
            store.put_agent_inputs("workspace", config(3)),
            Err(AgentInputRepositoryError::RevisionConflict {
                current: 1,
                attempted: 3,
            })
        );
        assert_eq!(
            store
                .get_agent_inputs("workspace", "agent")
                .unwrap()
                .unwrap()
                .revision,
            1,
            "a skipped revision must not mutate the accepted aggregate"
        );
        store.put_agent_inputs("workspace", config(2)).unwrap();
        assert_eq!(
            store.put_agent_inputs("workspace", config(1)),
            Err(AgentInputRepositoryError::RevisionConflict {
                current: 2,
                attempted: 1,
            })
        );
        assert_eq!(
            store
                .get_agent_inputs("workspace", "agent")
                .unwrap()
                .unwrap()
                .revision,
            2,
            "a stale revision must not roll back the accepted aggregate"
        );
        assert_eq!(
            store.put_agent_inputs("new-workspace", config(0)),
            Err(AgentInputRepositoryError::InvalidRevision(0))
        );
    }

    // ---- SEC: WebhookStore tenant fence + delete/disabled semantics ----

    fn webhook(id: &str, workspace: &str, disabled: bool) -> WebhookEndpointDef {
        WebhookEndpointDef {
            id: id.to_string(),
            workspace_id: workspace.to_string(),
            url: format!("https://example.test/{id}"),
            event_types: Vec::new(),
            disabled,
            consecutive_failures: 0,
            secret_ref: awaken_credential_vault::SecretRef("whsec".into()),
        }
    }

    #[test]
    fn webhook_store_lists_by_workspace_including_disabled_and_deletes_idempotently() {
        let store = InMemoryWebhookStore::new();
        store.put(webhook("wh-a1", "wrkspc_a", false)).unwrap();
        store.put(webhook("wh-a2", "wrkspc_a", true)).unwrap(); // disabled, still enumerated
        store.put(webhook("wh-b1", "wrkspc_b", false)).unwrap();

        // `list` is workspace-filtered (dispatch fan-out is per workspace), sorted
        // by id, and INCLUDES disabled rows (CRUD list surfaces suspended ones).
        let ids_a: Vec<String> = store
            .list("wrkspc_a")
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids_a, vec!["wh-a1".to_string(), "wh-a2".to_string()]);
        // Workspace B's endpoint is never disclosed to A (the tenant fence).
        assert!(
            !ids_a.iter().any(|id| id == "wh-b1"),
            "workspace A must not see workspace B's webhook: {ids_a:?}"
        );
        // The disabled row is present with its flag intact (list ≠ dispatch filter).
        assert!(
            store
                .list("wrkspc_a")
                .unwrap()
                .iter()
                .find(|d| d.id == "wh-a2")
                .unwrap()
                .disabled
        );

        // `delete` is an idempotent unsubscribe: true once, false thereafter.
        assert!(store.delete("wh-a1").unwrap(), "first delete removes a row");
        assert!(
            !store.delete("wh-a1").unwrap(),
            "second delete is a no-op → false"
        );
        assert!(store.get("wh-a1").unwrap().is_none());
        // Deleting an unknown id is also false (never panics, never fabricates).
        assert!(!store.delete("ghost").unwrap());

        // The surviving (disabled) row is still listed and reachable.
        let remaining: Vec<String> = store
            .list("wrkspc_a")
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(remaining, vec!["wh-a2".to_string()]);
        assert!(
            store
                .list("wrkspc_b")
                .unwrap()
                .iter()
                .any(|d| d.id == "wh-b1")
        );
    }

    #[test]
    fn webhook_delivery_state_machine_is_durable_in_the_store_contract() {
        // Cause/effect graph: C1 success; C2 failure below threshold; C3 failure at
        // threshold; C4 missing row. Effects: E1 reset counter; E2 increment while
        // active; E3 atomically disable; E4 terminal no-op. Constraint C1 xor C2.
        // Decision table: R1=C1 after C2 -> E1; R2=C2,count=0,threshold=2 -> E2;
        // R3=C2,count=1,threshold=2 -> E3; R4=C4 -> E4.
        let store = InMemoryWebhookStore::new();
        store.put(webhook("wh", "wrkspc", false)).unwrap();
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .unwrap(),
            WebhookDeliveryState::Active {
                consecutive_failures: 1
            },
            "R2"
        );
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Succeeded, 2)
                .unwrap(),
            WebhookDeliveryState::Active {
                consecutive_failures: 0
            },
            "R1"
        );
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
            .unwrap();
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .unwrap(),
            WebhookDeliveryState::Disabled {
                consecutive_failures: 2
            },
            "R3"
        );
        assert_eq!(
            store
                .record_delivery("missing", WebhookDeliveryOutcome::Failed, 2)
                .unwrap(),
            WebhookDeliveryState::Missing,
            "R4"
        );
    }
}
