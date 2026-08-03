//! Read ports for the authored control-plane aggregates — the store traits the
//! resolver and the runtime host consult at bind time.
//!
//! These live here, with the aggregate types they carry, so the **read** path
//! stays in the open resolver crate and never reaches back up into the authoring
//! HTTP surface. `awaken-admin-config-api` (the authoring plane) writes *through*
//! these same ports and provides the durable SQLite backend for them; the
//! runtime host reads *through* them at session-prepare time.

use awaken_credential_vault::SecretRef;

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

/// Authored fields accepted from one workspace-scoped webhook PUT. Operational
/// delivery state and sealed-material ownership are deliberately absent: the
/// repository preserves them while applying this patch atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookAuthoringPatch {
    pub id: String,
    pub workspace_id: String,
    pub url: String,
    pub event_types: Vec<String>,
    pub disabled: Option<bool>,
}

/// Result of atomically applying an authored patch to an existing aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookAuthoringState {
    Missing,
    OwnerMismatch,
    Updated(WebhookEndpointDef),
}

/// Durable cross-store mutation intent. The admin repository journals this
/// before the SecretStore side effect and removes it only after both projections
/// converge. `before=None` is create; `after=None` is delete.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WebhookMutationIntent {
    pub before: Option<WebhookEndpointDef>,
    pub after: Option<WebhookEndpointDef>,
}

impl WebhookMutationIntent {
    #[must_use]
    pub fn create(after: WebhookEndpointDef) -> Self {
        Self {
            before: None,
            after: Some(after),
        }
    }

    #[must_use]
    pub fn delete(before: WebhookEndpointDef) -> Self {
        Self {
            before: Some(before),
            after: None,
        }
    }

    pub fn id(&self) -> Result<&str, ConfigRepositoryError> {
        match (&self.before, &self.after) {
            (Some(before), None) => Ok(&before.id),
            (None, Some(after)) => Ok(&after.id),
            (Some(_), Some(_)) => Err(ConfigRepositoryError::InvalidMutation(
                "webhook material mutation must be create or delete".into(),
            )),
            (None, None) => Err(ConfigRepositoryError::InvalidMutation(
                "webhook mutation has no before or after state".into(),
            )),
        }
    }

    #[must_use]
    pub fn material_refs(&self) -> Vec<SecretRef> {
        self.before
            .iter()
            .chain(self.after.iter())
            .map(|definition| definition.secret_ref.clone())
            .collect()
    }
}

/// Infrastructure failure from a synchronous authored-config repository.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigRepositoryError {
    #[error("config repository storage failure: {0}")]
    Storage(String),
    #[error("config repository mutation conflict: {0}")]
    MutationConflict(String),
    #[error("invalid config repository mutation: {0}")]
    InvalidMutation(String),
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
    fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Every endpoint owned by `workspace_id` (including disabled), for CRUD list
    /// and dispatch matching.
    fn list(&self, workspace_id: &str) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Atomically update only operator-authored fields on an existing row. The
    /// owner, secret reference, and concurrent delivery state remain authoritative.
    fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError>;
    /// Durably journal a create/delete before any SecretStore side effect. The
    /// current aggregate must exactly equal `before`, and only one intent per id
    /// may exist.
    fn begin_mutation(&self, intent: WebhookMutationIntent) -> Result<(), ConfigRepositoryError>;
    /// Atomically publish the journaled `after` state (or remove the row for a
    /// delete). Both the pending intent and current `before` must still match.
    fn apply_mutation(&self, intent: &WebhookMutationIntent) -> Result<(), ConfigRepositoryError>;
    fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError>;
    /// Idempotently retire this exact converged intent. A different newer intent
    /// at the same id is a conflict rather than an ABA deletion.
    fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError>;
    /// All committed signing-material references, for protected inventory
    /// reconciliation across the shared SecretStore.
    fn material_refs(&self) -> Result<Vec<SecretRef>, ConfigRepositoryError>;
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

    fn commit_webhook(store: &dyn WebhookStore, definition: WebhookEndpointDef) {
        let intent = WebhookMutationIntent::create(definition);
        store.begin_mutation(intent.clone()).unwrap();
        store.apply_mutation(&intent).unwrap();
        store.complete_mutation(&intent).unwrap();
    }

    #[test]
    fn webhook_store_lists_by_workspace_and_journals_delete() {
        let store = InMemoryWebhookStore::new();
        commit_webhook(&store, webhook("wh-a1", "wrkspc_a", false));
        commit_webhook(&store, webhook("wh-a2", "wrkspc_a", true)); // disabled, still enumerated
        commit_webhook(&store, webhook("wh-b1", "wrkspc_b", false));

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

        // Delete is a journaled aggregate transition; replay observes the missing
        // row instead of inventing a second direct-delete path.
        let before = store.get("wh-a1").unwrap().unwrap();
        let intent = WebhookMutationIntent::delete(before);
        store.begin_mutation(intent.clone()).unwrap();
        store.apply_mutation(&intent).unwrap();
        store.complete_mutation(&intent).unwrap();
        assert!(store.get("wh-a1").unwrap().is_none());

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
        commit_webhook(&store, webhook("wh", "wrkspc", false));
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

    #[test]
    fn webhook_authoring_and_material_intents_follow_the_decision_table() {
        // Cause graph: C1 existing row with delivery count; C2 authored patch;
        // C3 pending delete intent; C4 delivery/update during intent; C5 apply and
        // complete. Effects E1 authored fields change but operational/ref fields
        // survive; E2 C4 conflicts; E3 delete is published while intent remains;
        // E4 completion retires only the journal. Decision rules:
        // R1=C1∧C2 -> E1; R2=C3∧C4 -> E2; R3=C3∧C5(apply) -> E3;
        // R4=R3∧C5(complete) -> E4.
        let store = InMemoryWebhookStore::new();
        let original = webhook("wh", "wrkspc", false);
        let secret_ref = original.secret_ref.clone();
        commit_webhook(&store, original);
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
            .unwrap();
        let updated = store
            .update_authored(WebhookAuthoringPatch {
                id: "wh".into(),
                workspace_id: "wrkspc".into(),
                url: "https://new.example/hook".into(),
                event_types: vec!["run.completed".into()],
                disabled: None,
            })
            .unwrap();
        let WebhookAuthoringState::Updated(updated) = updated else {
            panic!("R1 must update")
        };
        assert_eq!(updated.consecutive_failures, 1, "R1/E1");
        assert_eq!(updated.secret_ref, secret_ref, "R1/E1");

        let intent = WebhookMutationIntent::delete(updated);
        store.begin_mutation(intent.clone()).unwrap();
        assert!(
            matches!(
                store.record_delivery("wh", WebhookDeliveryOutcome::Failed, 20),
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2/E2"
        );
        assert!(
            matches!(
                store.update_authored(WebhookAuthoringPatch {
                    id: "wh".into(),
                    workspace_id: "wrkspc".into(),
                    url: "https://racing.example/hook".into(),
                    event_types: vec![],
                    disabled: None,
                }),
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2/E2"
        );
        store.apply_mutation(&intent).unwrap();
        assert!(store.get("wh").unwrap().is_none(), "R3/E3");
        assert_eq!(
            store.pending_mutations().unwrap(),
            vec![intent.clone()],
            "R3/E3"
        );
        store.complete_mutation(&intent).unwrap();
        assert!(store.pending_mutations().unwrap().is_empty(), "R4/E4");

        // R5 a stale completion races a newer same-id intent -> conflict and the
        // newer journal remains (ABA protection).
        let replacement = WebhookMutationIntent::create(webhook("wh", "wrkspc", false));
        store.begin_mutation(replacement.clone()).unwrap();
        assert!(
            matches!(
                store.complete_mutation(&intent),
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R5"
        );
        assert_eq!(store.pending_mutations().unwrap(), vec![replacement], "R5");

        // R6 an attempted direct before→after row replacement is not a material
        // lifecycle command and cannot reopen the removed generic put path.
        let invalid = WebhookMutationIntent {
            before: Some(webhook("other", "wrkspc", false)),
            after: Some(webhook("other", "wrkspc", true)),
        };
        assert!(
            matches!(invalid.id(), Err(ConfigRepositoryError::InvalidMutation(_))),
            "R6"
        );
    }
}
