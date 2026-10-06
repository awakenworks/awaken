// @formal entrypoint agent_inputs.applied_revision_is_exact_successor
// @formal entrypoint agent_inputs.rejected_revision_does_not_mutate
// @formal entrypoint agent_inputs.same_revision_same_content_is_idempotent
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

/// Infrastructure failure from an authored-config repository.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigRepositoryError {
    #[error("config repository storage failure: {0}")]
    Storage(String),
    #[error("config repository mutation conflict: {0}")]
    MutationConflict(String),
    #[error("invalid config repository mutation: {0}")]
    InvalidMutation(String),
}

/// A store for authored [`InferenceProfile`]s (an admin-plane aggregate). An
/// in-memory reference store and durable backends implement the same port.
#[async_trait::async_trait]
pub trait InferenceProfileStore: Send + Sync {
    async fn put(&self, id: String, profile: InferenceProfile)
    -> Result<(), ConfigRepositoryError>;
    async fn get(&self, id: &str) -> Result<Option<InferenceProfile>, ConfigRepositoryError>;
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
pub async fn get_workspace_profile(
    store: &dyn InferenceProfileStore,
    workspace_id: &str,
    profile_id: &str,
) -> Result<Option<InferenceProfile>, ConfigRepositoryError> {
    if let Some(profile) = store
        .get(&workspace_profile_key(workspace_id, profile_id))
        .await?
    {
        return Ok((profile.workspace_id == workspace_id).then_some(profile));
    }
    Ok(store
        .get(profile_id)
        .await?
        .filter(|profile| profile.workspace_id == workspace_id))
}

/// Persist under the Workspace-qualified key. Callers must stamp and validate the
/// trusted owner before invoking this helper.
pub async fn put_workspace_profile(
    store: &dyn InferenceProfileStore,
    workspace_id: &str,
    profile_id: &str,
    profile: InferenceProfile,
) -> Result<(), ConfigRepositoryError> {
    store
        .put(workspace_profile_key(workspace_id, profile_id), profile)
        .await
}

/// A store for authored [`WebhookEndpointDef`]s (an admin-plane aggregate,
/// ADR-0048). Product composition supplies a durable admin backend; a process-local
/// reference implementation is available only under `test-support`. Unlike
/// [`InferenceProfileStore`] it enumerates by
/// workspace (dispatch fan-out) and supports delete (unsubscribe).
#[async_trait::async_trait]
pub trait WebhookStore: Send + Sync {
    async fn get(&self, id: &str) -> Result<Option<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Every endpoint owned by `workspace_id` (including disabled), for CRUD list
    /// and dispatch matching.
    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<WebhookEndpointDef>, ConfigRepositoryError>;
    /// Atomically update only operator-authored fields on an existing row. The
    /// owner, secret reference, and concurrent delivery state remain authoritative.
    async fn update_authored(
        &self,
        patch: WebhookAuthoringPatch,
    ) -> Result<WebhookAuthoringState, ConfigRepositoryError>;
    /// Durably journal a create/delete before any SecretStore side effect. The
    /// current aggregate must exactly equal `before`, and only one intent per id
    /// may exist.
    async fn begin_mutation(
        &self,
        intent: WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError>;
    /// Atomically publish the journaled `after` state (or remove the row for a
    /// delete). Both the pending intent and current `before` must still match.
    async fn apply_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError>;
    async fn pending_mutations(&self) -> Result<Vec<WebhookMutationIntent>, ConfigRepositoryError>;
    /// Idempotently retire this exact converged intent. A different newer intent
    /// at the same id is a conflict rather than an ABA deletion.
    async fn complete_mutation(
        &self,
        intent: &WebhookMutationIntent,
    ) -> Result<(), ConfigRepositoryError>;
    /// All committed signing-material references, for protected inventory
    /// reconciliation across the shared SecretStore.
    async fn material_refs(&self) -> Result<Vec<SecretRef>, ConfigRepositoryError>;
    /// Atomically apply one delivery result to the existing row. A success resets
    /// the consecutive-failure count; a failure increments it and disables the row
    /// at `failure_threshold`. Missing rows are a terminal no-op.
    async fn record_delivery(
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
    #[error("Agent input identity contains a reserved storage delimiter or NUL")]
    InvalidIdentity,
    #[error("Agent input revision must be positive, got {0}")]
    InvalidRevision(i64),
    #[error("Agent input revision conflict: current {current}, attempted {attempted}")]
    RevisionConflict { current: i64, attempted: i64 },
    #[error("Agent input repository storage failure: {0}")]
    Storage(String),
}

/// Canonical persisted Workspace prefix. Existing valid keys retain their exact
/// bytes; delimiter/NUL-bearing coordinates fail closed instead of aliasing a
/// different Workspace/Agent pair or relying on backend-specific TEXT behavior.
pub fn agent_input_workspace_prefix(workspace: &str) -> Result<String, AgentInputRepositoryError> {
    validate_agent_input_coordinate(workspace)?;
    Ok(format!("{workspace}\u{1f}"))
}

pub fn agent_input_storage_key(
    workspace: &str,
    agent: &str,
) -> Result<String, AgentInputRepositoryError> {
    validate_agent_input_coordinate(agent)?;
    Ok(format!(
        "{}{agent}",
        agent_input_workspace_prefix(workspace)?
    ))
}

/// Reject corrupt/ambiguous historical rows instead of projecting them under
/// the requested Workspace. All SQL get/list paths use this same identity check.
pub fn validate_stored_agent_inputs(
    workspace: &str,
    key: &str,
    config: AgentInputConfig,
) -> Result<AgentInputConfig, AgentInputRepositoryError> {
    if agent_input_storage_key(workspace, &config.agent_id)? != key {
        return Err(AgentInputRepositoryError::InvalidIdentity);
    }
    Ok(config)
}

fn validate_agent_input_coordinate(value: &str) -> Result<(), AgentInputRepositoryError> {
    if value.bytes().any(|byte| byte == 0 || byte == 0x1f) {
        return Err(AgentInputRepositoryError::InvalidIdentity);
    }
    Ok(())
}

/// Closed admission outcome. Only the owner can construct a write plan; a
/// replay produces no plan and adapters must not execute a write for it.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentInputAdmission<'a> {
    Replay,
    Write(AgentInputWritePlan<'a>),
}

/// A frozen target and expected revision, borrowed from the admitted proposal.
/// This is not a database lock: adapters must hold their transaction/lock from
/// observation through commit and apply the plan's revision predicate.
///
/// ```compile_fail
/// use awaken_config_resolver::AgentInputWritePlan;
/// let forged = AgentInputWritePlan { key: String::new(), expected_revision: None, config: todo!() };
/// ```
///
/// ```compile_fail
/// use awaken_config_resolver::{AgentInputAdmission, AgentInputConfig, plan_agent_input_write};
/// let mut next = AgentInputConfig { agent_id: "agent".into(), environment: None, inputs: vec![], revision: 1 };
/// let AgentInputAdmission::Write(plan) = plan_agent_input_write("workspace", None, &next).unwrap() else { return; };
/// next.revision = 9; // A checked proposal cannot change while its plan is live.
/// assert_eq!(plan.config().revision, 1);
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct AgentInputWritePlan<'a> {
    key: String,
    expected_revision: Option<i64>,
    config: &'a AgentInputConfig,
}

impl<'a> AgentInputWritePlan<'a> {
    pub fn storage_key(&self) -> &str {
        &self.key
    }
    pub fn expected_revision(&self) -> Option<i64> {
        self.expected_revision
    }
    pub fn config(&self) -> &'a AgentInputConfig {
        self.config
    }
}

/// Admit exactly one immutable proposal. No adapter-supplied equality flag and
/// no public plan constructor can substitute different content after admission.
pub fn plan_agent_input_write<'a>(
    workspace: &str,
    current: Option<&AgentInputConfig>,
    next: &'a AgentInputConfig,
) -> Result<AgentInputAdmission<'a>, AgentInputRepositoryError> {
    let key = agent_input_storage_key(workspace, &next.agent_id)?;
    if current.is_some_and(|current| current.agent_id != next.agent_id) {
        return Err(AgentInputRepositoryError::InvalidIdentity);
    }
    if agent_input_revision_decision(
        current.map(|value| value.revision),
        next.revision,
        current.is_some_and(|value| value == next),
    )? {
        Ok(AgentInputAdmission::Write(AgentInputWritePlan {
            key,
            expected_revision: current.map(|value| value.revision),
            config: next,
        }))
    } else {
        Ok(AgentInputAdmission::Replay)
    }
}

/// Scalar decision kernel of the existing aggregate validator. Content equality
/// is projected exactly once by that validator, never supplied by an adapter.
/// This private kernel owns all revision rules; callers own locking and commit.
fn agent_input_revision_decision(
    current: Option<i64>,
    attempted: i64,
    same_aggregate: bool,
) -> Result<bool, AgentInputRepositoryError> {
    if attempted < 1 {
        return Err(AgentInputRepositoryError::InvalidRevision(attempted));
    }
    match current {
        Some(current) if current == attempted && same_aggregate => Ok(false),
        Some(current) if current > 0 && current.checked_add(1) == Some(attempted) => Ok(true),
        Some(current) => Err(AgentInputRepositoryError::RevisionConflict { current, attempted }),
        None if attempted == 1 => Ok(true),
        None => Err(AgentInputRepositoryError::RevisionConflict {
            current: 0,
            attempted,
        }),
    }
}

#[cfg(kani)]
/// @obligation agent_inputs.applied_revision_is_exact_successor
/// @obligation agent_inputs.rejected_revision_does_not_mutate
/// @obligation agent_inputs.same_revision_same_content_is_idempotent
/// @proof-input-debt free_symbol agent_input_revision_decision same_aggregate
#[kani::proof]
fn agent_input_revision_decision_is_exact_non_wrapping_and_replay_first() {
    // Causes: existence x all i64 current/attempted values x aggregate equality.
    // R1 attempted<=0 -> invalid; R2 same positive revision+aggregate -> no write;
    // R3 missing+1 or positive exact successor -> write; R4 all others -> conflict.
    // The i128 specification cannot overflow when calculating the i64 successor.
    // Equality is an unconstrained equivalence-class input (including impossible
    // combinations); Rust structural equality, locks, SQL and commits are not
    // proved by this scalar harness. No database non-mutation claim is implied.
    let current: Option<i64> = kani::any();
    let attempted: i64 = kani::any();
    let same_aggregate: bool = kani::any();
    let decision = agent_input_revision_decision(current, attempted, same_aggregate);
    let replay = current == Some(attempted) && same_aggregate && attempted > 0;
    let successor = match current {
        None => attempted == 1,
        Some(value) => value > 0 && i128::from(attempted) == i128::from(value) + 1,
    };
    // Inspect the closed result directly; formatting the Storage(String) error
    // in assert_eq! would pull unrelated unbounded formatting into the solver.
    match decision {
        Ok(write) => {
            assert!(attempted > 0);
            assert!(if write { successor && !replay } else { replay });
        }
        Err(AgentInputRepositoryError::InvalidRevision(value)) => {
            assert!(attempted <= 0 && value == attempted);
        }
        Err(AgentInputRepositoryError::RevisionConflict {
            current: value,
            attempted: next,
        }) => {
            assert!(attempted > 0 && !replay && !successor);
            assert!(value == current.unwrap_or(0) && next == attempted);
        }
        Err(AgentInputRepositoryError::Storage(_) | AgentInputRepositoryError::InvalidIdentity) => {
            assert!(false)
        }
    }
}

#[async_trait::async_trait]
pub trait AgentInputBindingRepository: Send + Sync {
    /// Apply revision 1 to a new aggregate or exactly current+1 to an existing
    /// aggregate. Replaying byte-equivalent state at the current revision is
    /// idempotent; stale/skipped revisions fail closed.
    async fn put_agent_inputs(
        &self,
        workspace_id: &str,
        config: AgentInputConfig,
    ) -> Result<(), AgentInputRepositoryError>;
    async fn get_agent_inputs(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<Option<AgentInputConfig>, AgentInputRepositoryError>;
    async fn list_agent_inputs(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<AgentInputConfig>, AgentInputRepositoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BindingId, FileId, InputBinding, InputResourceId, ResourceAccess};
    use crate::{InMemoryAgentInputBindingRepository, InMemoryWebhookStore};

    #[tokio::test]
    async fn agent_inputs_are_isolated_by_workspace_even_for_the_same_agent_id() {
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
            .await
            .unwrap();
        store
            .put_agent_inputs("workspace-b", config("file-b"))
            .await
            .unwrap();

        assert_eq!(
            store
                .get_agent_inputs("workspace-a", "shared-agent")
                .await
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
                .await
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
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.list_agent_inputs("workspace-a").await.unwrap().len(),
            1
        );
        assert_eq!(
            store.list_agent_inputs("workspace-a").await.unwrap()[0].inputs[0]
                .target
                .id(),
            "file-a"
        );
        assert!(
            store
                .list_agent_inputs("workspace-c")
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn agent_input_revision_exhaustion_rejects_without_panicking_or_mutating() {
        // C1 current=i64::MAX; C2 exact replay vs changed content; C3 positive
        // vs invalid next revision. R1 exact replay -> no write; R2 changed
        // positive proposal -> conflict (never overflow); R3 nonpositive ->
        // InvalidRevision. Every rejection leaves the borrowed aggregate intact.
        let current = AgentInputConfig {
            agent_id: "agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision: i64::MAX,
        };
        let before = current.clone();
        assert_eq!(
            plan_agent_input_write("workspace", Some(&current), &current),
            Ok(AgentInputAdmission::Replay)
        );
        for revision in [1, i64::MAX - 1, i64::MAX, 0, i64::MIN] {
            let mut next = current.clone();
            next.revision = revision;
            next.environment = Some(crate::AgentEnvironmentBinding {
                environment_id: "changed".into(),
                revision: 1,
            });
            let expected = if revision < 1 {
                AgentInputRepositoryError::InvalidRevision(revision)
            } else {
                AgentInputRepositoryError::RevisionConflict {
                    current: i64::MAX,
                    attempted: revision,
                }
            };
            assert_eq!(
                plan_agent_input_write("workspace", Some(&current), &next),
                Err(expected)
            );
            assert_eq!(current, before);
        }
    }

    #[test]
    fn agent_input_revision_decision_table_preserves_exact_content() {
        // Causes: absent/corrupt/valid/exhausted current x attempted revision x
        // exact/changed aggregate. Identity mismatch rejects before revision
        // admission; corrupt current is not silently repaired. Same revision is
        // replay only when EVERY field agrees. Successors may change authored
        // content, never retarget an already observed aggregate's Agent identity.
        let config = |revision| AgentInputConfig {
            agent_id: "agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision,
        };
        for current_revision in [
            None,
            Some(i64::MIN),
            Some(0),
            Some(1),
            Some(i64::MAX - 1),
            Some(i64::MAX),
        ] {
            let current = current_revision.map(config);
            for revision in [i64::MIN, 0, 1, 2, 3, i64::MAX - 1, i64::MAX] {
                for changed_field in 0..4 {
                    let mut next = config(revision);
                    match changed_field {
                        0 => {}
                        1 => next.agent_id = "different-agent".into(),
                        2 => {
                            next.environment = Some(crate::AgentEnvironmentBinding {
                                environment_id: "environment".into(),
                                revision: 1,
                            })
                        }
                        _ => next.inputs.push(InputBinding {
                            binding_id: BindingId::from("file"),
                            target: InputResourceId::File(FileId::from("file")),
                            mount_path: "/workspace/file".into(),
                            access: ResourceAccess::ReadOnly,
                            instructions: None,
                        }),
                    }
                    let expected = if changed_field == 1 && current.is_some() {
                        Err(AgentInputRepositoryError::InvalidIdentity)
                    } else if revision <= 0 {
                        Err(AgentInputRepositoryError::InvalidRevision(revision))
                    } else if current_revision == Some(revision) && changed_field == 0 {
                        Ok(false)
                    } else if current_revision.map_or(revision == 1, |c| {
                        c > 0 && i128::from(revision) - i128::from(c) == 1
                    }) {
                        Ok(true)
                    } else {
                        Err(AgentInputRepositoryError::RevisionConflict {
                            current: current_revision.unwrap_or(0),
                            attempted: revision,
                        })
                    };
                    assert_eq!(
                        plan_agent_input_write("workspace", current.as_ref(), &next).map(
                            |decision| {
                                match decision {
                                    AgentInputAdmission::Replay => false,
                                    AgentInputAdmission::Write(plan) => {
                                        assert_eq!(plan.expected_revision(), current_revision);
                                        assert_eq!(plan.config(), &next);
                                        assert_eq!(
                                            plan.storage_key(),
                                            format!("workspace\u{1f}{}", next.agent_id)
                                        );
                                        true
                                    }
                                }
                            }
                        ),
                        expected,
                        "current={current_revision:?}, next={revision}, changed field={changed_field}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn agent_input_revisions_are_sequential_and_replays_are_idempotent() {
        let store = InMemoryAgentInputBindingRepository::new();
        let config = |revision| AgentInputConfig {
            agent_id: "agent".into(),
            environment: None,
            inputs: Vec::new(),
            revision,
        };

        store
            .put_agent_inputs("workspace", config(1))
            .await
            .unwrap();
        store
            .put_agent_inputs("workspace", config(1))
            .await
            .unwrap();
        assert_eq!(
            store.put_agent_inputs("workspace", config(3)).await,
            Err(AgentInputRepositoryError::RevisionConflict {
                current: 1,
                attempted: 3,
            })
        );
        assert_eq!(
            store
                .get_agent_inputs("workspace", "agent")
                .await
                .unwrap()
                .unwrap()
                .revision,
            1,
            "a skipped revision must not mutate the accepted aggregate"
        );
        store
            .put_agent_inputs("workspace", config(2))
            .await
            .unwrap();
        assert_eq!(
            store.put_agent_inputs("workspace", config(1)).await,
            Err(AgentInputRepositoryError::RevisionConflict {
                current: 2,
                attempted: 1,
            })
        );
        assert_eq!(
            store
                .get_agent_inputs("workspace", "agent")
                .await
                .unwrap()
                .unwrap()
                .revision,
            2,
            "a stale revision must not roll back the accepted aggregate"
        );
        assert_eq!(
            store.put_agent_inputs("new-workspace", config(0)).await,
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

    async fn commit_webhook(store: &dyn WebhookStore, definition: WebhookEndpointDef) {
        let intent = WebhookMutationIntent::create(definition);
        store.begin_mutation(intent.clone()).await.unwrap();
        store.apply_mutation(&intent).await.unwrap();
        store.complete_mutation(&intent).await.unwrap();
    }

    #[tokio::test]
    async fn webhook_store_lists_by_workspace_and_journals_delete() {
        let store = InMemoryWebhookStore::new();
        commit_webhook(&store, webhook("wh-a1", "wrkspc_a", false)).await;
        commit_webhook(&store, webhook("wh-a2", "wrkspc_a", true)).await; // disabled, still enumerated
        commit_webhook(&store, webhook("wh-b1", "wrkspc_b", false)).await;

        // `list` is workspace-filtered (dispatch fan-out is per workspace), sorted
        // by id, and INCLUDES disabled rows (CRUD list surfaces suspended ones).
        let ids_a: Vec<String> = store
            .list("wrkspc_a")
            .await
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
                .await
                .unwrap()
                .iter()
                .find(|d| d.id == "wh-a2")
                .unwrap()
                .disabled
        );

        // Delete is a journaled aggregate transition; replay observes the missing
        // row instead of inventing a second direct-delete path.
        let before = store.get("wh-a1").await.unwrap().unwrap();
        let intent = WebhookMutationIntent::delete(before);
        store.begin_mutation(intent.clone()).await.unwrap();
        store.apply_mutation(&intent).await.unwrap();
        store.complete_mutation(&intent).await.unwrap();
        assert!(store.get("wh-a1").await.unwrap().is_none());

        // The surviving (disabled) row is still listed and reachable.
        let remaining: Vec<String> = store
            .list("wrkspc_a")
            .await
            .unwrap()
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(remaining, vec!["wh-a2".to_string()]);
        assert!(
            store
                .list("wrkspc_b")
                .await
                .unwrap()
                .iter()
                .any(|d| d.id == "wh-b1")
        );
    }

    #[tokio::test]
    async fn webhook_delivery_state_machine_is_durable_in_the_store_contract() {
        // Cause/effect graph: C1 success; C2 failure below threshold; C3 failure at
        // threshold; C4 missing row. Effects: E1 reset counter; E2 increment while
        // active; E3 atomically disable; E4 terminal no-op. Constraint C1 xor C2.
        // Decision table: R1=C1 after C2 -> E1; R2=C2,count=0,threshold=2 -> E2;
        // R3=C2,count=1,threshold=2 -> E3; R4=C4 -> E4.
        let store = InMemoryWebhookStore::new();
        commit_webhook(&store, webhook("wh", "wrkspc", false)).await;
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .await
                .unwrap(),
            WebhookDeliveryState::Active {
                consecutive_failures: 1
            },
            "R2"
        );
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Succeeded, 2)
                .await
                .unwrap(),
            WebhookDeliveryState::Active {
                consecutive_failures: 0
            },
            "R1"
        );
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
            .await
            .unwrap();
        assert_eq!(
            store
                .record_delivery("wh", WebhookDeliveryOutcome::Failed, 2)
                .await
                .unwrap(),
            WebhookDeliveryState::Disabled {
                consecutive_failures: 2
            },
            "R3"
        );
        assert_eq!(
            store
                .record_delivery("missing", WebhookDeliveryOutcome::Failed, 2)
                .await
                .unwrap(),
            WebhookDeliveryState::Missing,
            "R4"
        );
    }

    #[tokio::test]
    async fn webhook_authoring_and_material_intents_follow_the_decision_table() {
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
        commit_webhook(&store, original).await;
        store
            .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
            .await
            .unwrap();
        let updated = store
            .update_authored(WebhookAuthoringPatch {
                id: "wh".into(),
                workspace_id: "wrkspc".into(),
                url: "https://new.example/hook".into(),
                event_types: vec!["run.completed".into()],
                disabled: None,
            })
            .await
            .unwrap();
        let WebhookAuthoringState::Updated(updated) = updated else {
            panic!("R1 must update")
        };
        assert_eq!(updated.consecutive_failures, 1, "R1/E1");
        assert_eq!(updated.secret_ref, secret_ref, "R1/E1");

        let intent = WebhookMutationIntent::delete(updated);
        store.begin_mutation(intent.clone()).await.unwrap();
        assert!(
            matches!(
                store
                    .record_delivery("wh", WebhookDeliveryOutcome::Failed, 20)
                    .await,
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2/E2"
        );
        assert!(
            matches!(
                store
                    .update_authored(WebhookAuthoringPatch {
                        id: "wh".into(),
                        workspace_id: "wrkspc".into(),
                        url: "https://racing.example/hook".into(),
                        event_types: vec![],
                        disabled: None,
                    })
                    .await,
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R2/E2"
        );
        store.apply_mutation(&intent).await.unwrap();
        assert!(store.get("wh").await.unwrap().is_none(), "R3/E3");
        assert_eq!(
            store.pending_mutations().await.unwrap(),
            vec![intent.clone()],
            "R3/E3"
        );
        store.complete_mutation(&intent).await.unwrap();
        assert!(store.pending_mutations().await.unwrap().is_empty(), "R4/E4");

        // R5 a stale completion races a newer same-id intent -> conflict and the
        // newer journal remains (ABA protection).
        let replacement = WebhookMutationIntent::create(webhook("wh", "wrkspc", false));
        store.begin_mutation(replacement.clone()).await.unwrap();
        assert!(
            matches!(
                store.complete_mutation(&intent).await,
                Err(ConfigRepositoryError::MutationConflict(_))
            ),
            "R5"
        );
        assert_eq!(
            store.pending_mutations().await.unwrap(),
            vec![replacement],
            "R5"
        );

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
