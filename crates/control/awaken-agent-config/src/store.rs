// @formal entrypoint audit.business_action_follows_audit_record
// @formal entrypoint audit.business_commit_and_audit_transition_are_atomic
// @formal entrypoint audit.call_identity_is_stable
// @formal entrypoint audit.stable_call_replay_does_not_repeat_business_write
// @formal entrypoint config.publication_revision_is_scoped_by_execution_target_and_exact_replay
// @formal entrypoint management_http.no_business_write_without_durable_audit
// @formal entrypoint resource_binding.draft_audit_and_effect_are_atomic
// @formal entrypoint resource_binding.pending_effect_survives_until_readback
// @formal entrypoint resource_binding.replay_is_idempotent_and_identity_stable
//! The `ConfigRegistry` port and its persisted aggregates.

use std::sync::Arc;

use awaken_runtime_contract::snapshot::ExecutableAgentSnapshot;
use awaken_tenancy::ScopeId;
use serde::{Deserialize, Serialize};

use crate::AgentInputConfig;
use crate::config::AgentConfig;

/// A neutral config-store failure (storage or serialization). Compilation errors
/// are separate ([`crate::CompileError`]).
#[derive(Debug, thiserror::Error)]
#[error("config store: {0}")]
pub struct ConfigStoreError(pub String);

/// An authoring config paired with the monotonic revision used for optimistic
/// concurrency control.
#[derive(Debug, Clone)]
pub struct AgentConfigRevision {
    pub config: AgentConfig,
    pub revision: u64,
    /// First durable authoring write, when the store exposes lifecycle time.
    pub created_at_unix_ms: Option<u64>,
    /// Durable write time of this exact revision.
    pub updated_at_unix_ms: Option<u64>,
}

/// Outcome of an atomic compare-and-set config write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigWrite {
    Applied { revision: u64 },
    Conflict { current_revision: Option<u64> },
}

/// Durable result of the sole source-fenced publication write.
///
/// The committed revision is a receipt from the repository transaction. It is
/// checked again at the registration boundary, so a stale/conflicting write or
/// a malformed adapter result cannot authorize a Coordinator effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationWrite {
    Committed { source_revision: u64 },
    Conflict { current_revision: Option<u64> },
}

impl PublicationWrite {
    /// Admit registration only for the exact durable publication revision.
    pub const fn registration_admission(
        self,
        publication_source_revision: u64,
    ) -> Result<(), Option<u64>> {
        match self {
            Self::Committed { source_revision }
                if source_revision == publication_source_revision =>
            {
                Ok(())
            }
            Self::Committed { source_revision } => Err(Some(source_revision)),
            Self::Conflict { current_revision } => Err(current_revision),
        }
    }
}

/// Pure classification of a publication attempt against already durable
/// publications for the same Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationRevisionDecision {
    /// No publication occupies the proposed execution target and source revision.
    Apply,
    /// The exact fingerprint is already durable; retrying is idempotent.
    ExactReplay,
    /// The target and source revision are already bound to another fingerprint.
    Conflict,
}

/// Classify a publication attempt without performing storage I/O.
///
pub fn publication_revision_decision<'a, T, I>(
    proposed_execution_target: &'a T,
    proposed_source_revision: u64,
    proposed_fingerprint: &'a T,
    existing: I,
) -> PublicationRevisionDecision
where
    T: PartialEq + ?Sized + 'a,
    I: IntoIterator<Item = (&'a T, u64, &'a T)>,
{
    let mut conflicting_fingerprint = false;

    for (execution_target, source_revision, fingerprint) in existing {
        if source_revision != proposed_source_revision
            || execution_target != proposed_execution_target
        {
            continue;
        }
        if fingerprint == proposed_fingerprint {
            return PublicationRevisionDecision::ExactReplay;
        }
        conflicting_fingerprint = true;
    }

    if conflicting_fingerprint {
        PublicationRevisionDecision::Conflict
    } else {
        PublicationRevisionDecision::Apply
    }
}

/// Secret-free durable management audit record keyed by stable tool call id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementAuditRecord {
    pub tool: String,
    pub call_id: String,
    pub summary: String,
}

/// Preserve the two opaque audit identity coordinates as a tuple. This is the
/// only production projection used to construct and index a management audit;
/// neither coordinate is parsed or concatenated with the other.
fn management_audit_identity<T>(tool: T, call_id: T) -> (T, T) {
    (tool, call_id)
}

/// Authorization for an audited write, not evidence that a transaction committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementAuditAdmission {
    Commit,
    Replay,
}

fn management_audit_admission(
    recorded_state: Option<i64>,
    exact_record: bool,
) -> Result<ManagementAuditAdmission, &'static str> {
    match recorded_state {
        None => Err("audited config transaction requires a pre-recorded management audit"),
        Some(_) if !exact_record => Err("stable audit call id was reused with different content"),
        Some(0) => Ok(ManagementAuditAdmission::Commit),
        Some(1) => Ok(ManagementAuditAdmission::Replay),
        Some(_) => Err("invalid management audit committed state"),
    }
}

impl ManagementAuditRecord {
    /// Construct one durable audit record from its exact opaque identity tuple.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        call_id: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        let (tool, call_id) = management_audit_identity(tool.into(), call_id.into());
        Self {
            tool,
            call_id,
            summary: summary.into(),
        }
    }

    /// Borrow the exact tuple used by durable repository indexes.
    #[must_use]
    pub fn identity(&self) -> (&str, &str) {
        management_audit_identity(&self.tool, &self.call_id)
    }

    /// Admit only the exact durable intent. Adapters must hold their transaction
    /// fence from this observation through the business write and audit update.
    pub fn admit_write(
        &self,
        recorded: Option<(&Self, i64)>,
    ) -> Result<ManagementAuditAdmission, ConfigStoreError> {
        management_audit_admission(
            recorded.map(|(_, state)| state),
            recorded.is_some_and(|(record, _)| record == self),
        )
        .map_err(|message| ConfigStoreError(message.into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementAuditEntry {
    pub record: ManagementAuditRecord,
    pub business_committed: bool,
}

impl ManagementAuditEntry {
    /// Decode the closed durable state; corruption is never a successful replay.
    pub fn from_stored(
        record: ManagementAuditRecord,
        state: i64,
    ) -> Result<Self, ConfigStoreError> {
        let business_committed =
            record.admit_write(Some((&record, state)))? == ManagementAuditAdmission::Replay;
        Ok(Self {
            record,
            business_committed,
        })
    }
}

/// A secret-free, idempotent effect that must be applied to a separate store
/// after the config transaction commits. The config store durably journals it
/// in the same transaction as the draft and audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffect {
    UpsertAgentInputs { config: AgentInputConfig },
}

/// Exact workspace-scoped readback evidence, not a serialized success flag or
/// proof of storage internals. Only `ManagementEffect::confirm_readback` creates
/// it. Duplicate receipts are safe because retirement checks the current payload.
#[derive(Debug, Clone)]
pub struct ManagementEffectReceipt {
    scope: ScopeId,
    effect: ManagementEffect,
}

fn management_effect_confirmation(revision: i64, exact: bool) -> bool {
    revision > 0 && exact
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagementEffectEnqueueDecision {
    Insert,
    Replay,
    Conflict,
    InvalidRevision,
}

#[must_use]
const fn management_effect_enqueue_decision(
    revision_is_positive: bool,
    pending_exists: bool,
    pending_is_exact: bool,
) -> ManagementEffectEnqueueDecision {
    if !revision_is_positive {
        ManagementEffectEnqueueDecision::InvalidRevision
    } else if !pending_exists {
        ManagementEffectEnqueueDecision::Insert
    } else if pending_is_exact {
        ManagementEffectEnqueueDecision::Replay
    } else {
        ManagementEffectEnqueueDecision::Conflict
    }
}

impl ManagementEffectReceipt {
    #[must_use]
    pub fn scope(&self) -> &ScopeId {
        &self.scope
    }

    #[must_use]
    pub fn effect(&self) -> &ManagementEffect {
        &self.effect
    }

    /// The adapter must fence this observation through deletion.
    #[must_use]
    pub fn matches_pending(&self, pending: &ManagementEffect) -> bool {
        management_effect_confirmation(pending.revision(), &self.effect == pending)
    }
}

impl ManagementEffect {
    pub const AGENT_INPUTS_KIND: &'static str = "agent_inputs.upsert";

    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UpsertAgentInputs { .. } => Self::AGENT_INPUTS_KIND,
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::UpsertAgentInputs { config } => &config.agent_id,
        }
    }

    fn revision(&self) -> i64 {
        match self {
            Self::UpsertAgentInputs { config } => config.revision,
        }
    }

    /// One typed index contract for both adapters; indices are projections only.
    pub fn validate_index(&self, kind: &str, key: &str) -> Result<(), ConfigStoreError> {
        if kind != self.kind() || key != self.key() {
            return Err(ConfigStoreError(
                "management effect index does not match its typed payload".into(),
            ));
        }
        Ok(())
    }

    /// Return whether to insert. A pending logical key cannot be overwritten;
    /// exact retry is semantic equality, not equality of JSON whitespace/order.
    pub fn requires_enqueue(&self, pending: Option<&Self>) -> Result<bool, ConfigStoreError> {
        match management_effect_enqueue_decision(
            self.revision() > 0,
            pending.is_some(),
            pending == Some(self),
        ) {
            ManagementEffectEnqueueDecision::Insert => Ok(true),
            ManagementEffectEnqueueDecision::Replay => Ok(false),
            ManagementEffectEnqueueDecision::Conflict => Err(ConfigStoreError(
                "stable management effect key was reused with different content".into(),
            )),
            ManagementEffectEnqueueDecision::InvalidRevision => Err(ConfigStoreError(
                "management effect requires a positive input revision".into(),
            )),
        }
    }

    /// `observed` must come from the existing input repository in `scope`.
    /// Missing, changed, or nonpositive state cannot mint a receipt.
    #[must_use]
    pub fn confirm_readback(
        &self,
        scope: &ScopeId,
        observed: Option<&AgentInputConfig>,
    ) -> Option<ManagementEffectReceipt> {
        let Self::UpsertAgentInputs { config } = self;
        management_effect_confirmation(config.revision, observed == Some(config)).then(|| {
            ManagementEffectReceipt {
                scope: scope.clone(),
                effect: self.clone(),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditedConfigWrite {
    Applied,
    Replayed,
    Conflict { current_revision: Option<u64> },
}

impl AuditedConfigWrite {
    /// Admit a business handler only after the durable begin authority applied
    /// this exact request. Replays and conflicts remain observational results.
    #[must_use]
    pub const fn permits_business_write(self) -> bool {
        matches!(self, Self::Applied)
    }

    /// A successful business response needs the matching durable completion;
    /// failed business responses and non-applied begins must not advance audit.
    #[must_use]
    pub const fn requires_audit_completion(self, business_succeeded: bool) -> bool {
        self.permits_business_write() && business_succeeded
    }
}

/// The lifecycle spine (ADR-0031). The richer states (installing/active/
/// superseded/rolled_back/rejected) are deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicationState {
    Compiled,
    Published,
}

impl PublicationState {
    pub fn as_str(self) -> &'static str {
        match self {
            PublicationState::Compiled => "compiled",
            PublicationState::Published => "published",
        }
    }
}

/// A persisted publication: the compiled artifact plus its lifecycle state. It is
/// content-addressed by `fingerprint`, so storing it again is idempotent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPublication {
    pub publication_id: String,
    pub fingerprint: String,
    pub agent_id: String,
    #[serde(default, alias = "source_generation")]
    pub source_revision: u64,
    /// Workspace in which this publication is executable. Ordinary Agent
    /// publications use their authoring scope; reserved platform Agents may be
    /// authored once and published into several execution Workspaces.
    ///
    pub execution_workspace: ScopeId,
    pub state: PublicationState,
    pub snapshot: ExecutableAgentSnapshot,
    /// Exact Agent input defaults used to compile this publication. Keeping the
    /// value beside the executable snapshot makes a publication self-contained:
    /// later Draft edits cannot change (or make unavailable) an already-published
    /// Agent, and startup recovery never has to reconstruct an old Resource revision
    /// from the mutable authoring repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_inputs: Option<AgentInputConfig>,
}

impl StoredPublication {
    /// Wrap a freshly compiled config as `published`. The fingerprint and
    /// publication id come from the snapshot itself (the producer stamped it), so
    /// the store never re-derives content identity.
    pub fn published(
        config: ExecutableAgentSnapshot,
        agent_id: impl Into<String>,
        execution_workspace: impl Into<ScopeId>,
    ) -> Self {
        Self::published_at_revision(config, agent_id, 0, execution_workspace)
    }

    pub fn published_at_revision(
        config: ExecutableAgentSnapshot,
        agent_id: impl Into<String>,
        source_revision: u64,
        execution_workspace: impl Into<ScopeId>,
    ) -> Self {
        Self {
            publication_id: config.fingerprint.0.clone(),
            fingerprint: config.fingerprint.0.clone(),
            agent_id: agent_id.into(),
            source_revision,
            execution_workspace: execution_workspace.into(),
            state: PublicationState::Published,
            snapshot: config,
            agent_inputs: None,
        }
    }

    /// Freeze the exact Resource bindings that belong to this publication.
    #[must_use]
    pub fn with_agent_inputs(mut self, inputs: Option<AgentInputConfig>) -> Self {
        self.agent_inputs = inputs;
        self
    }

    #[must_use]
    pub fn targets_execution_workspace(&self, execution_workspace: &str) -> bool {
        self.execution_workspace.as_str() == execution_workspace
    }
}

/// Durable storage for the config domain: agent configs (the authoring
/// aggregate) and publications (the compiled artifact). Adapters live under the
/// `config` table namespace, alongside the runtime's tables (ADR-0029).
#[async_trait::async_trait]
pub trait ConfigRegistry: Send + Sync {
    /// Upsert an agent config by id.
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError>;

    /// Store only when `expected_revision` is still current. Revision zero
    /// means "create only"; a positive revision never creates an absent row.
    /// Success returns the committed revision, not a client-side prediction.
    /// Adapters reject revisions whose successor exceeds their durable range.
    /// Adapters without CAS support fail explicitly.
    async fn put_config_if_revision(
        &self,
        _config: &AgentConfig,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support revision CAS".to_string(),
        ))
    }

    /// Load an agent config by id.
    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError>;

    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError>;

    /// Immutable authoring revisions, oldest first.
    async fn list_config_revisions(
        &self,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        Ok(self.get_config_revision(id).await?.into_iter().collect())
    }

    /// List every stored agent config (the authoring aggregate), ascending by id.
    /// Backs the management console's agent list, which authors against this
    /// config plane directly rather than the SDK-facing `/v1/agents` registry.
    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError>;

    /// Atomically store the publication only while its own source revision is
    /// still the current authored revision. No independent expected revision is
    /// accepted, so callers cannot detach the durable fence from the artifact.
    async fn put_publication_if_source_current(
        &self,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError>;

    /// Load a publication by its fingerprint.
    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError>;
}

// --- Tenant isolation: the ScopedConfig decorator (ADR-0051 D2/D4) ----------

/// The seeded owner scope every un-scoped write lands under and every un-scoped
/// read filters by. It matches the `scope_id` column default in the schema, so a
/// pre-tenancy row and a `DEFAULT_SCOPE` write are the same owner — the
/// single-machine "seeded, not absent" default (ADR-0048 D2).
pub use awaken_tenancy::DEFAULT_WORKSPACE_ID as DEFAULT_SCOPE;

/// The scope-aware backing store — the infrastructure-facing half of the config
/// port. Each method carries an owner [`ScopeId`], persisted as one opaque
/// `scope_id` column: reads filter by it and writes are guarded by it, so a
/// workspace can neither read nor clobber another's agent by id. The core-facing
/// [`ConfigRegistry`] is scope-free; [`ScopedConfig`] bridges the two by binding a
/// scope.
#[async_trait::async_trait]
pub trait ScopedConfigRegistry: Send + Sync {
    /// Enumerate authoring owners that currently have Agent configs. This is a
    /// system-reconciliation port, not a tenant read: callers must bind every
    /// returned scope again before reading or publishing its aggregates.
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError>;

    /// Upsert an agent config owned by `scope` (a write never crosses into
    /// another scope's row of the same id).
    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError>;

    /// Atomically persist config against a pre-recorded pending audit. The
    /// separate `record_management_audit_scoped` port is the sole concurrent
    /// begin authority. A committed exact replay is a no-op; absent or
    /// conflicting audit identity fails closed.
    async fn put_config_with_audit_scoped(
        &self,
        _scope: &ScopeId,
        _config: &AgentConfig,
        _expected_revision: u64,
        _audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support transactional audit".to_string(),
        ))
    }

    async fn put_config_with_audit_effect_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
        expected_revision: u64,
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        if effect.is_some() {
            return Err(ConfigStoreError(
                "config registry does not support durable external effects".to_string(),
            ));
        }
        self.put_config_with_audit_scoped(scope, config, expected_revision, audit)
            .await
    }

    async fn pending_management_effects_scoped(
        &self,
        _scope: &ScopeId,
    ) -> Result<Vec<ManagementEffect>, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable external effects".to_string(),
        ))
    }

    async fn complete_management_effect_scoped(
        &self,
        _receipt: &ManagementEffectReceipt,
    ) -> Result<bool, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable external effects".to_string(),
        ))
    }

    async fn record_management_audit_scoped(
        &self,
        _scope: &ScopeId,
        _audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit".to_string(),
        ))
    }

    async fn get_management_audit_scoped(
        &self,
        _scope: &ScopeId,
        _tool: &str,
        _call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit reads".to_string(),
        ))
    }

    async fn mark_management_audit_committed_scoped(
        &self,
        _scope: &ScopeId,
        _tool: &str,
        _call_id: &str,
    ) -> Result<(), ConfigStoreError> {
        Err(ConfigStoreError(
            "config registry does not support durable audit completion".to_string(),
        ))
    }

    /// Scoped form of [`ConfigRegistry::put_config_if_revision`], including
    /// create-only zero and fail-closed durable revision exhaustion.
    async fn put_config_if_revision_scoped(
        &self,
        _scope: &ScopeId,
        _config: &AgentConfig,
        _expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        Err(ConfigStoreError(
            "scoped config registry does not support revision CAS".to_string(),
        ))
    }

    /// Load an agent config by id **within `scope`** — a row owned by another
    /// scope is invisible.
    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError>;

    async fn get_config_revision_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError>;

    /// Immutable authoring revisions for `id` within `scope`, oldest first.
    async fn list_config_revisions_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        Ok(self
            .get_config_revision_scoped(scope, id)
            .await?
            .into_iter()
            .collect())
    }

    /// List every agent config owned by `scope`, ascending by id — a row owned by
    /// another scope is invisible.
    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError>;

    /// Atomically store the scoped publication only while its embedded source
    /// revision is current. This is the sole production publication write.
    async fn put_publication_if_source_current_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError>;

    /// Load a publication by fingerprint **within `scope`**.
    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError>;

    /// Every **published** publication owned by `scope`, oldest first (ascending by
    /// insertion order), so a warm-load that inserts into an agent-keyed map keeps
    /// the latest publication per agent.
    ///
    /// **Required** — deliberately has no default. It once defaulted to
    /// `Ok(Vec::new())` "because only a durable store reloads across a lifetime,"
    /// but that let a durable backend that simply *forgot* to override it compile
    /// clean and silently reload zero agents on restart (exactly what happened to
    /// the Postgres store). Forcing every implementor to answer means an empty
    /// reload is now an explicit choice, never an accident. An implementor with
    /// nothing to reload (a purely ephemeral store) returns `Ok(Vec::new())` on
    /// purpose.
    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError>;
}

#[cfg(kani)]
/// @obligation resource_binding.draft_audit_and_effect_are_atomic
/// @obligation resource_binding.pending_effect_survives_until_readback
/// @obligation resource_binding.replay_is_idempotent_and_identity_stable
/// @proof-input-debt free_symbol management_effect_confirmation exact
#[kani::proof]
fn management_effect_confirmation_requires_positive_exact_readback() {
    // R1 invalid signed revision => reject even exact readback. R2 positive
    // but missing/mismatched observation => reject. R3 positive+exact => receipt
    // or exact retirement. Scope is carried by the receipt; this scalar proof
    // does not prove repository reads, heap equality or SQL transaction fences.
    let revision: i64 = kani::any();
    let exact: bool = kani::any();
    assert_eq!(
        management_effect_confirmation(revision, exact),
        revision >= 1 && exact
    );
}

#[cfg(kani)]
/// @obligation resource_binding.draft_audit_and_effect_are_atomic
/// @obligation resource_binding.replay_is_idempotent_and_identity_stable
/// @proof-input-debt free_symbol management_effect_enqueue_decision pending_exists
/// @proof-input-debt free_symbol management_effect_enqueue_decision pending_is_exact
/// @proof-input-debt free_symbol management_effect_enqueue_decision revision_is_positive
#[kani::proof]
fn management_effect_enqueue_is_exact_replay_or_absent_insert() {
    // Causes: C1 the effect revision is positive; C2 a pending effect exists;
    // C3 its complete typed payload is identical. Effects: E1 insert only for
    // C1+!C2; E2 replay only for C1+C2+C3; E3 reject a changed stable identity;
    // E4 reject every nonpositive revision. Decision rules R1-R4 are exactly
    // those four disjoint partitions; C3 is irrelevant when C2 is false.
    let positive: bool = kani::any();
    let present: bool = kani::any();
    let exact: bool = kani::any();
    let decision = management_effect_enqueue_decision(positive, present, exact);
    assert_eq!(
        decision == ManagementEffectEnqueueDecision::Insert,
        positive && !present
    );
    assert_eq!(
        decision == ManagementEffectEnqueueDecision::Replay,
        positive && present && exact
    );
    assert_eq!(
        decision == ManagementEffectEnqueueDecision::Conflict,
        positive && present && !exact
    );
    assert_eq!(
        decision == ManagementEffectEnqueueDecision::InvalidRevision,
        !positive
    );
}

#[cfg(test)]
mod effect_receipt_tests {
    use super::*;

    #[test]
    fn complete_payload_and_positive_revision_own_receipt_and_retirement() {
        // R0 absent readback rejects; R1 each signed boundary; R2 independently
        // change Agent/revision/Environment/inputs (16 masks); R3 only exact
        // positive payload confirms and retires. Index mismatch always rejects;
        // enqueue permits absence/exact replay only and never replaces intent.
        let scope = ScopeId::from("workspace");
        for revision in [i64::MIN, -1, 0, 1, i64::MAX] {
            let config = AgentInputConfig {
                agent_id: "agent".into(),
                environment: None,
                inputs: Vec::new(),
                revision,
            };
            let expected = ManagementEffect::UpsertAgentInputs {
                config: config.clone(),
            };
            assert!(expected.confirm_readback(&scope, None).is_none(), "R0");
            assert_eq!(
                expected.requires_enqueue(None).is_ok(),
                revision > 0,
                "R1 enqueue"
            );
            assert_eq!(
                expected.requires_enqueue(Some(&expected)).ok(),
                (revision > 0).then_some(false)
            );
            assert!(
                expected
                    .validate_index(expected.kind(), expected.key())
                    .is_ok()
            );
            assert!(expected.validate_index("wrong", expected.key()).is_err());
            assert!(expected.validate_index(expected.kind(), "wrong").is_err());
            let receipt = expected.confirm_readback(&scope, Some(&config));
            assert_eq!(receipt.is_some(), revision > 0, "R1");
            for mask in 0..16 {
                let mut observed = config.clone();
                if mask & 1 != 0 {
                    observed.agent_id.push('x');
                }
                if mask & 2 != 0 {
                    observed.revision = if revision == i64::MAX {
                        1
                    } else {
                        revision + 1
                    };
                }
                if mask & 4 != 0 {
                    observed.environment = Some(crate::AgentEnvironmentBinding {
                        environment_id: "env".into(),
                        revision: 1,
                    });
                }
                if mask & 8 != 0 {
                    observed.inputs.push(serde_json::from_value(serde_json::json!({"binding_id":"input", "target":{"kind":"file","id":"file"},"mount_path":"/input", "access":"read_only"})).unwrap());
                }
                assert_eq!(
                    expected.confirm_readback(&scope, Some(&observed)).is_some(),
                    revision > 0 && mask == 0,
                    "R2 mask={mask}"
                );
                let pending = ManagementEffect::UpsertAgentInputs { config: observed };
                assert_eq!(
                    expected.requires_enqueue(Some(&pending)).is_ok(),
                    revision > 0 && mask == 0
                );
                if let Some(receipt) = &receipt {
                    assert_eq!(receipt.scope(), &scope);
                    assert_eq!(receipt.effect(), &expected);
                    assert_eq!(receipt.matches_pending(&pending), mask == 0, "R3");
                }
            }
        }
    }
}

#[cfg(kani)]
/// @obligation audit.call_identity_is_stable
#[kani::proof]
fn management_audit_call_identity_is_stable_and_tuple_exact() {
    // Causes: C1 exact retry repeats both opaque coordinates; C2 only tool
    // changes; C3 only call changes; C4 both change. Effects: E1 exact retry
    // produces the same identity; E2-E4 every coordinate change is distinct.
    // Decision rules are the four complete equality classes over two u64
    // representatives. The production generic tuple projection never parses or
    // concatenates values; String equality and persistence remain outside Kani.
    let tool: u64 = kani::any();
    let call_id: u64 = kani::any();
    let other_tool: u64 = kani::any();
    let other_call_id: u64 = kani::any();
    let identity = management_audit_identity(tool, call_id);
    assert_eq!(identity, management_audit_identity(tool, call_id), "C1/E1");
    assert_eq!(
        identity == management_audit_identity(other_tool, other_call_id),
        tool == other_tool && call_id == other_call_id,
        "C2-C4/E2-E4"
    );
}

#[cfg(kani)]
/// @obligation audit.business_action_follows_audit_record
/// @obligation audit.stable_call_replay_does_not_repeat_business_write
/// @obligation management_http.no_business_write_without_durable_audit
/// @obligation resource_binding.draft_audit_and_effect_are_atomic
/// @proof-input-debt free_symbol management_audit_admission exact_record
#[kani::proof]
fn management_audit_admission_requires_exact_intent_and_closed_state() {
    // Cause/effect rules: A0 absent => reject; A1 any changed record => reject;
    // A2 exact/pending(0) => Commit; A3 exact/committed(1) => Replay;
    // A4 every other signed database value => reject. Presence and equality are
    // independent; SQL tuple selection/locking and heap equality are NOT proved.
    let present: bool = kani::any();
    let exact: bool = kani::any();
    let state: i64 = kani::any();
    let result = management_audit_admission(present.then_some(state), exact);
    assert_eq!(
        result == Ok(ManagementAuditAdmission::Commit),
        present && exact && state == 0
    );
    assert_eq!(
        result == Ok(ManagementAuditAdmission::Replay),
        present && exact && state == 1
    );
    assert_eq!(
        result.is_err(),
        !present || !exact || (state != 0 && state != 1)
    );

    // B0 Applied is the sole business authority; B1 replay/conflict stutter.
    // C0 completion is requested exactly for Applied plus business success.
    // The repository/HTTP adapters consume these exact production decisions;
    // database and async effect ordering remain outside this scalar proof.
    let applied: bool = kani::any();
    let replayed: bool = kani::any();
    let write = if applied {
        AuditedConfigWrite::Applied
    } else if replayed {
        AuditedConfigWrite::Replayed
    } else {
        AuditedConfigWrite::Conflict {
            current_revision: kani::any(),
        }
    };
    let business_succeeded: bool = kani::any();
    assert_eq!(write.permits_business_write(), applied);
    assert_eq!(
        write.requires_audit_completion(business_succeeded),
        applied && business_succeeded
    );
}

#[cfg(test)]
mod audit_admission_tests {
    use super::*;

    #[test]
    fn call_identity_projection_is_stable_and_delimiter_safe() {
        // Cause/effect table: R1 clone or JSON round-trip of one record keeps
        // the exact tool/call tuple; R2 summary-only change keeps its identity;
        // R3 a delimiter-colliding legacy concatenation with either coordinate
        // changed remains a distinct tuple; R4 empty and Unicode coordinates
        // survive construction and JSON round-trip. Effects are exact borrowed
        // String equality, with no parsing, hashing, or normalization.
        let cases = [
            ("http:POST", "a:b"),
            ("http", "POST:a:b"),
            ("", ""),
            ("管理:工具", "调用:λ"),
        ];
        for (tool, call_id) in cases {
            let record = ManagementAuditRecord::new(tool, call_id, "body_sha256=one");
            assert_eq!(record.identity(), (tool, call_id), "R1/R4 construct");
            assert_eq!(record.clone().identity(), record.identity(), "R1 clone");
            let decoded: ManagementAuditRecord =
                serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
            assert_eq!(decoded.identity(), record.identity(), "R1/R4 JSON");
            let changed_summary = ManagementAuditRecord::new(tool, call_id, "body_sha256=two");
            assert_eq!(changed_summary.identity(), record.identity(), "R2");
            assert_ne!(changed_summary, record, "summary remains intent content");
        }
        let left = ManagementAuditRecord::new("http:POST", "a:b", "one");
        let right = ManagementAuditRecord::new("http", "POST:a:b", "one");
        assert_ne!(left.identity(), right.identity(), "R3");
    }

    #[test]
    fn record_projection_and_durable_decode_follow_the_admission_table() {
        // A0 absent; A1 independently change tool/call/summary (all 8 masks);
        // A2/A3 pending/committed; A4 corrupt signed extrema and 2. Only mask 0
        // with 0/1 admits, and decode accepts exactly 0/1. This tests the real
        // String equality projection that the scalar Kani proof assumes.
        let expected = ManagementAuditRecord {
            tool: "http:POST:/管理".into(),
            call_id: "run:step:call".into(),
            summary: "hash=a".into(),
        };
        assert!(expected.admit_write(None).is_err(), "A0");
        for mask in 0..8 {
            let mut recorded = expected.clone();
            if mask & 1 != 0 {
                recorded.tool.push(':');
            }
            if mask & 2 != 0 {
                recorded.call_id.push(':');
            }
            if mask & 4 != 0 {
                recorded.summary.push(':');
            }
            for state in [i64::MIN, 0, 1, 2, i64::MAX] {
                let result = expected.admit_write(Some((&recorded, state)));
                assert_eq!(
                    result.is_ok(),
                    mask == 0 && matches!(state, 0 | 1),
                    "mask={mask}, state={state}"
                );
                if let Ok(decision) = result {
                    assert_eq!(decision == ManagementAuditAdmission::Replay, state == 1);
                }
                let decoded = ManagementAuditEntry::from_stored(recorded.clone(), state);
                assert_eq!(decoded.is_ok(), matches!(state, 0 | 1));
                if let Ok(entry) = decoded {
                    assert_eq!(entry.business_committed, state == 1);
                }
            }
        }
    }

    #[test]
    fn durable_begin_result_owns_business_and_completion_admission() {
        // Cause/effect decision table:
        // | begin result | business success | business write | completion |
        // | Applied      | false            | yes            | no         |
        // | Applied      | true             | yes            | yes        |
        // | Replay/Conflict | either        | no             | no         |
        // Conflict crosses both absent/present revisions. This exercises the
        // same finite owner consumed by Admin tools and the HTTP middleware.
        let outcomes = [
            AuditedConfigWrite::Applied,
            AuditedConfigWrite::Replayed,
            AuditedConfigWrite::Conflict {
                current_revision: None,
            },
            AuditedConfigWrite::Conflict {
                current_revision: Some(u64::MAX),
            },
        ];
        for outcome in outcomes {
            for business_succeeded in [false, true] {
                let applied = outcome == AuditedConfigWrite::Applied;
                assert_eq!(outcome.permits_business_write(), applied);
                assert_eq!(
                    outcome.requires_audit_completion(business_succeeded),
                    applied && business_succeeded
                );
            }
        }
    }
}

#[cfg(kani)]
/// @obligation config.publication_revision_is_scoped_by_execution_target_and_exact_replay
#[kani::proof]
fn publication_revision_decision_is_target_safe_fail_closed_and_replay_first() {
    let proposed_target: u8 = kani::any();
    let other_target: u8 = kani::any();
    let proposed_fingerprint: u8 = kani::any();
    let other_fingerprint: u8 = kani::any();
    let revision: u64 = kani::any();

    kani::assume(other_target != proposed_target);
    assert_ne!(
        publication_revision_decision(
            &proposed_target,
            revision,
            &proposed_fingerprint,
            [(&other_target, revision, &other_fingerprint)],
        ),
        PublicationRevisionDecision::Conflict
    );

    kani::assume(other_fingerprint != proposed_fingerprint);
    assert_eq!(
        publication_revision_decision(
            &proposed_target,
            revision,
            &proposed_fingerprint,
            [(&proposed_target, revision, &other_fingerprint)],
        ),
        PublicationRevisionDecision::Conflict
    );

    assert_eq!(
        publication_revision_decision(
            &proposed_target,
            revision,
            &proposed_fingerprint,
            [
                (&proposed_target, revision, &other_fingerprint),
                (&proposed_target, revision, &proposed_fingerprint),
            ],
        ),
        PublicationRevisionDecision::ExactReplay
    );
}

#[cfg(kani)]
/// @obligation config_activation.stale_publication_never_installs
#[kani::proof]
fn publication_registration_requires_exact_commit_receipt() {
    // Causes: C1 the repository returned Committed/Conflict; C2 a committed
    // receipt revision equals/differs from the exact publication revision; C3
    // a conflict reports an absent/present current revision. Effects: R1 only
    // Committed+exact admits the registrar; R2 mismatched commit fails closed;
    // R3 every conflict rejects while preserving its observed current revision.
    let publication_source_revision: u64 = kani::any();
    let receipt_revision: u64 = kani::any();
    let conflict: bool = kani::any();
    let current_revision: Option<u64> = kani::any();
    let write = if conflict {
        PublicationWrite::Conflict { current_revision }
    } else {
        PublicationWrite::Committed {
            source_revision: receipt_revision,
        }
    };
    let admission = write.registration_admission(publication_source_revision);
    assert_eq!(
        admission.is_ok(),
        !conflict && receipt_revision == publication_source_revision
    );
    if conflict {
        assert_eq!(admission, Err(current_revision));
    } else if receipt_revision != publication_source_revision {
        assert_eq!(admission, Err(Some(receipt_revision)));
    }
}

/// The decorator that makes tenancy an edge aspect for the config plane: it
/// implements the scope-free [`ConfigRegistry`] by binding one [`ScopeId`] and
/// delegating to a [`ScopedConfigRegistry`]. Constructed at the management edge
/// from the request's resolved scope, so every authoring write auto-stamps the
/// bound owner and every read auto-filters by it — no call site can forget.
pub struct ScopedConfig<S: ScopedConfigRegistry + ?Sized> {
    inner: Arc<S>,
    scope: ScopeId,
}

impl<S: ScopedConfigRegistry + ?Sized> ScopedConfig<S> {
    /// Bind `store` to `scope` for one tenant's authoring requests. `S` may be a
    /// trait object (`dyn ScopedConfigRegistry`), so the edge can bind a boxed store.
    pub fn new(store: Arc<S>, scope: ScopeId) -> Self {
        Self {
            inner: store,
            scope,
        }
    }

    /// The scope this registry is bound to.
    #[must_use]
    pub fn scope(&self) -> &ScopeId {
        &self.scope
    }
}

#[async_trait::async_trait]
impl<S: ScopedConfigRegistry + ?Sized> ConfigRegistry for ScopedConfig<S> {
    async fn put_config(&self, config: &AgentConfig) -> Result<(), ConfigStoreError> {
        self.inner.put_config_scoped(&self.scope, config).await
    }

    async fn put_config_if_revision(
        &self,
        config: &AgentConfig,
        expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.inner
            .put_config_if_revision_scoped(&self.scope, config, expected_revision)
            .await
    }

    async fn get_config(&self, id: &str) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.inner.get_config_scoped(&self.scope, id).await
    }

    async fn get_config_revision(
        &self,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        self.inner.get_config_revision_scoped(&self.scope, id).await
    }

    async fn list_config_revisions(
        &self,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        self.inner
            .list_config_revisions_scoped(&self.scope, id)
            .await
    }

    async fn list_configs(&self) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        self.inner.list_configs_scoped(&self.scope).await
    }

    async fn put_publication_if_source_current(
        &self,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
        self.inner
            .put_publication_if_source_current_scoped(&self.scope, publication)
            .await
    }

    async fn get_publication(
        &self,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        self.inner
            .get_publication_scoped(&self.scope, fingerprint)
            .await
    }
}

#[cfg(test)]
mod publication_revision_tests {
    use super::{
        ManagementEffect, PublicationRevisionDecision, PublicationWrite,
        publication_revision_decision,
    };

    #[test]
    fn registration_requires_the_exact_durable_publication_receipt() {
        // Cause/effect decision table owned by PublicationWrite:
        // R1 committed revision equals publication -> admit registration;
        // R2 committed revision is stale/future -> reject with that receipt;
        // R3 conflict with absent/present current -> reject and preserve that
        // exact observation. Zero/u64::MAX cross equality and mismatch edges.
        for publication in [0, 1, u64::MAX] {
            for committed in [0, 1, u64::MAX] {
                let result = PublicationWrite::Committed {
                    source_revision: committed,
                }
                .registration_admission(publication);
                assert_eq!(result.is_ok(), committed == publication, "R1/R2");
                if committed != publication {
                    assert_eq!(result, Err(Some(committed)), "R2");
                }
            }
            for current_revision in [None, Some(0), Some(1), Some(u64::MAX)] {
                assert_eq!(
                    PublicationWrite::Conflict { current_revision }
                        .registration_admission(publication),
                    Err(current_revision),
                    "R3"
                );
            }
        }
    }

    #[test]
    fn management_effect_derives_its_index_from_typed_content() {
        let effect = ManagementEffect::UpsertAgentInputs {
            config: crate::AgentInputConfig {
                agent_id: "agent-a".into(),
                environment: None,
                inputs: Vec::new(),
                revision: 3,
            },
        };
        assert_eq!(effect.kind(), ManagementEffect::AGENT_INPUTS_KIND);
        assert_eq!(effect.key(), "agent-a");
        let wire = serde_json::to_value(&effect).unwrap();
        assert_eq!(
            serde_json::from_value::<ManagementEffect>(wire).unwrap(),
            effect
        );
        assert!(
            serde_json::from_value::<ManagementEffect>(serde_json::json!({
                "type":"unknown", "payload":{}
            }))
            .is_err()
        );
    }

    #[test]
    fn different_execution_target_does_not_conflict() {
        let existing = [("workspace-b", 7, "old")];

        assert_eq!(
            publication_revision_decision("workspace-a", 7, "new", existing),
            PublicationRevisionDecision::Apply
        );
    }

    #[test]
    fn same_execution_target_and_revision_with_different_fingerprint_conflicts() {
        let existing = [("workspace-a", 7, "old")];

        assert_eq!(
            publication_revision_decision("workspace-a", 7, "new", existing),
            PublicationRevisionDecision::Conflict
        );
    }

    #[test]
    fn exact_replay_wins_over_an_explicit_conflicting_publication() {
        let existing = [("workspace-a", 7, "old"), ("workspace-a", 7, "new")];

        assert_eq!(
            publication_revision_decision("workspace-a", 7, "new", existing),
            PublicationRevisionDecision::ExactReplay
        );
    }
}
