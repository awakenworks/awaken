//! Model auto-binding (ADR-0052 D5).
//!
//! A [`ModelSelection::Auto`] config must become a concrete binding before it can
//! compile (compile is pure and cannot reach the provider catalog). The
//! [`ModelResolver`] port answers "what is the first provider-backed offering" — the
//! host implements it over the shared model catalog — and [`ConfigService`] calls it
//! in `publish`, *before* `compile`, so the stored `ExecutableAgentSnapshot` is self-contained,
//! content-addressed, and reproducible. `Pinned` selections skip the resolver.
//!
//! Freshness (an `Auto` binding can go stale when the catalog changes) is recovered
//! by one explicit seam — [`AssistantBindingReconciler`] — the catalog write path
//! calls after a mutation. It re-resolves and re-publishes `Auto` configs and skips
//! `Pinned` ones; re-publish is idempotent by content address, so a retry is safe.

use awaken_config_store::ModelSelection;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_tenancy::ScopeId;

/// A resolved `Auto` selection: the first provider-backed offering as the primary
/// binding, the remaining offerings as ordered pool candidates (which feed the
/// engine's existing pool-failover at run time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub primary: ModelBinding,
    pub candidates: Vec<ModelBinding>,
}

/// Resolve an `Auto` model selection to a concrete binding (ADR-0052 D5). The host
/// implements this over the org-shared provider catalog.
pub trait ModelResolver: Send + Sync {
    /// The first provider-backed offering as primary, the rest as candidates.
    /// `Err` when the catalog has no provider-backed model ("configure and publish a
    /// model first") — surfaced by `publish` as a 409.
    fn resolve_auto(&self) -> Result<ResolvedModel, String>;

    /// The published context window (max tokens) of a resolved model, when the catalog
    /// carries it — the source `resolve_agent_config` derives an agent's compaction
    /// window from. Defaults to `None` (a resolver with no catalog attributes), so the
    /// compaction window stays whatever the agent authored.
    fn context_window(&self, _model_id: &str) -> Option<u32> {
        None
    }

    /// The published output-token ceiling of a resolved model, when the catalog carries it —
    /// the headroom `resolve_agent_config` reserves when deriving the compaction window. `None`
    /// (no attribute) simply omits the headroom (window derives from `context_window` alone).
    fn max_output_tokens(&self, _model_id: &str) -> Option<u32> {
        None
    }
}

/// The freshness seam (ADR-0052 D5): the model-catalog write path calls this after a
/// catalog mutation to re-resolve every `Auto`-bound managed agent and re-publish it.
/// Named for its consumer (the catalog write path). One testable home for the one
/// place the design bends: who triggers it (the catalog write), what it touches (only
/// `Auto` configs), and how failure surfaces (a returned error the caller logs/retries
/// — a failed reconcile leaves the last good published binding in place).
#[async_trait::async_trait]
pub trait AssistantBindingReconciler: Send + Sync {
    /// Re-resolve and re-publish the `Auto`-bound assistant(s); returns how many were
    /// actually re-published (a `Pinned` one is skipped, an already-current `Auto` is
    /// a no-op by content address).
    async fn reconcile(&self) -> Result<usize, String>;
}

/// Whether a selection needs the resolver. `Pinned` passes through; `Auto` must be
/// resolved. Kept here so `publish` and the reconciler share one predicate.
#[must_use]
pub fn needs_resolution(selection: &ModelSelection) -> bool {
    selection.is_auto()
}

/// The concrete reconciler the host wires: it re-publishes a fixed set of agent ids
/// (the reserved-scope assistant) in a scope through the ordinary config-plane publish
/// path. It holds the scope edge ([`ConfigPlane`](crate::config_plane::ConfigPlane)) —
/// which binds the scope onto the scope-free service — plus the scope + ids.
pub struct ConfigServiceReconciler {
    plane: crate::config_plane::ConfigPlane,
    scope: ScopeId,
    agent_ids: Vec<String>,
}

impl ConfigServiceReconciler {
    pub fn new(
        plane: crate::config_plane::ConfigPlane,
        scope: impl Into<ScopeId>,
        agent_ids: Vec<String>,
    ) -> Self {
        Self {
            plane,
            scope: scope.into(),
            agent_ids,
        }
    }
}

#[async_trait::async_trait]
impl AssistantBindingReconciler for ConfigServiceReconciler {
    async fn reconcile(&self) -> Result<usize, String> {
        let mut republished = 0;
        for id in &self.agent_ids {
            if self.plane.reconcile(&self.scope, id).await? {
                republished += 1;
            }
        }
        Ok(republished)
    }
}
