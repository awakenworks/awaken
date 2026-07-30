//! Model publication resolution (ADR-0052 D5 / ADR-0062).
//!
//! One port turns authored model selection into complete, ordered snapshot
//! candidates. It owns both auto-selection and provider/credential pinning, so
//! publication cannot combine a binding from one catalog read with access facts
//! from another. Compilation remains pure and runtime only consumes the result.
//!
//! Freshness (a policy binding can go stale when its authority changes) is recovered
//! by one explicit seam — [`PublicationBindingReconciler`] — catalog and Worker
//! observation changes call it after mutation. It re-resolves policy-bound configs
//! and skips `Pinned` ones; re-publish is idempotent by content address.

use awaken_config_store::ModelSelection;
use awaken_runtime_contract::resolved::{ModelBinding, ResolvedModelCandidate};
use awaken_tenancy::ScopeId;

/// Stable failure vocabulary for publication-time model resolution.
///
/// Adapters keep database/provider error details behind these categories so the
/// application edge can map failures without parsing strings. Runtime never sees
/// this type because publication either produces a complete snapshot or fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublicationResolutionError {
    #[error("model catalog is unavailable: {0}")]
    CatalogUnavailable(String),
    #[error("credential inventory is unavailable: {0}")]
    CredentialInventoryUnavailable(String),
    #[error("no active model candidate is available for publication")]
    MissingPrimary,
    #[error("model candidate `{binding:?}` cannot be published: {reason}")]
    CandidateUnavailable {
        binding: ModelBinding,
        reason: String,
    },
    #[error("duplicate model candidate `{0:?}`")]
    DuplicateBinding(ModelBinding),
    #[error("invalid model publication: {0}")]
    Invalid(String),
}

impl From<String> for PublicationResolutionError {
    fn from(reason: String) -> Self {
        Self::Invalid(reason)
    }
}

impl From<&str> for PublicationResolutionError {
    fn from(reason: &str) -> Self {
        Self::Invalid(reason.to_owned())
    }
}

/// Complete, secret-free output of one model-publication resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPublicationModels {
    pub primary: ResolvedModelCandidate,
    pub candidates: Vec<ResolvedModelCandidate>,
    /// Primary model attributes used to derive compaction before compilation.
    pub context_window: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl ResolvedPublicationModels {
    /// Explicit host-executor composition for embedded/dev adapters. Provider
    /// catalog adapters return `Provider` candidates through the same port.
    #[must_use]
    pub fn host(
        primary: ModelBinding,
        candidates: Vec<ModelBinding>,
        context_window: Option<u32>,
        max_output_tokens: Option<u32>,
    ) -> Self {
        Self {
            primary: ResolvedModelCandidate::host(primary),
            candidates: candidates
                .into_iter()
                .map(ResolvedModelCandidate::host)
                .collect(),
            context_window,
            max_output_tokens,
        }
    }
}

/// Resolve authored selection and ordered fallbacks into one immutable model
/// publication. Implementations may read catalogs and credential inventories,
/// but must return one complete candidate per selected binding or fail the whole
/// operation. The trusted execution Workspace is an input, never ambient state.
#[async_trait::async_trait]
pub trait ModelPublicationResolver: Send + Sync {
    async fn resolve_models(
        &self,
        workspace: &ScopeId,
        selection: &ModelSelection,
        candidates: &[ModelBinding],
    ) -> Result<ResolvedPublicationModels, PublicationResolutionError>;
}

/// The freshness seam: model-catalog and Worker-observation changes re-resolve
/// policy-bound Agents through the ordinary publication path. A failed reconcile
/// leaves the last good publication in place and is safe to retry.
#[async_trait::async_trait]
pub trait PublicationBindingReconciler: Send + Sync {
    /// Re-resolve the configured fixed Agent set; concrete `Pinned` selections
    /// are skipped and repeated publications are content-addressed.
    async fn reconcile(&self) -> Result<usize, String>;

    /// Reconcile every policy-bound Agent in every authoring scope. This is the
    /// Worker-observation event path; authored values are preserved.
    async fn reconcile_all(&self) -> Result<usize, String>;
}

/// The concrete reconciler the host wires. Catalog changes use its fixed Agent
/// set; Worker observation changes use its all-scope operation. Both go through
/// the ordinary config-plane publication path. It holds the scope edge
/// ([`ConfigPlane`](crate::ConfigPlane)),
/// the configuration namespace, the execution Workspace, and the agent ids. Keeping
/// both coordinates explicit prevents a reserved authoring namespace from becoming a
/// synthetic resource or credential Workspace.
pub struct ConfigServiceReconciler {
    plane: crate::config_plane::ConfigPlane,
    configuration_scope: ScopeId,
    execution_workspace: String,
    agent_ids: Vec<String>,
}

impl ConfigServiceReconciler {
    pub fn new(
        plane: crate::config_plane::ConfigPlane,
        configuration_scope: impl Into<ScopeId>,
        execution_workspace: impl Into<String>,
        agent_ids: Vec<String>,
    ) -> Self {
        Self {
            plane,
            configuration_scope: configuration_scope.into(),
            execution_workspace: execution_workspace.into(),
            agent_ids,
        }
    }
}

#[async_trait::async_trait]
impl PublicationBindingReconciler for ConfigServiceReconciler {
    async fn reconcile(&self) -> Result<usize, String> {
        let mut republished = 0;
        for id in &self.agent_ids {
            if self
                .plane
                .reconcile_for_execution_workspace(
                    &self.configuration_scope,
                    &self.execution_workspace,
                    id,
                )
                .await?
            {
                republished += 1;
            }
        }
        Ok(republished)
    }

    async fn reconcile_all(&self) -> Result<usize, String> {
        self.plane
            .reconcile_all_policy_bound(&self.execution_workspace)
            .await
    }
}
