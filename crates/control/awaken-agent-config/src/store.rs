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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagementAuditEntry {
    pub record: ManagementAuditRecord,
    pub business_committed: bool,
}

/// A secret-free, idempotent effect that must be applied to a separate store
/// after the config transaction commits. The config store durably journals it
/// in the same transaction as the draft and audit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffect {
    UpsertAgentInputs { config: AgentInputConfig },
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditedConfigWrite {
    Applied,
    Replayed,
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
    /// means "create only". Adapters without CAS support fail explicitly.
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

    /// Store a publication, idempotent by fingerprint.
    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

    async fn put_publication_if_config_revision(
        &self,
        publication: &StoredPublication,
        expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError>;

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

    /// Atomically persist the audit record and config. Replaying the same call id
    /// with the same record is a no-op; conflicting reuse fails closed.
    async fn put_config_with_audit_scoped(
        &self,
        _scope: &ScopeId,
        _config: &AgentConfig,
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
        audit: &ManagementAuditRecord,
        effect: Option<&ManagementEffect>,
    ) -> Result<AuditedConfigWrite, ConfigStoreError> {
        if effect.is_some() {
            return Err(ConfigStoreError(
                "config registry does not support durable external effects".to_string(),
            ));
        }
        self.put_config_with_audit_scoped(scope, config, audit)
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
        _scope: &ScopeId,
        _kind: &str,
        _key: &str,
    ) -> Result<(), ConfigStoreError> {
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

    /// Store a publication owned by `scope`, idempotent by fingerprint.
    async fn put_publication_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError>;

    async fn put_publication_if_config_revision_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
        expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError>;

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

    async fn put_publication(
        &self,
        publication: &StoredPublication,
    ) -> Result<(), ConfigStoreError> {
        self.inner
            .put_publication_scoped(&self.scope, publication)
            .await
    }

    async fn put_publication_if_config_revision(
        &self,
        publication: &StoredPublication,
        expected_revision: u64,
    ) -> Result<ConfigWrite, ConfigStoreError> {
        self.inner
            .put_publication_if_config_revision_scoped(&self.scope, publication, expected_revision)
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
    use super::{ManagementEffect, PublicationRevisionDecision, publication_revision_decision};

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
