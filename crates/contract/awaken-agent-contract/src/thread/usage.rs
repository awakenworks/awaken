// @formal surface requirement=runtime.state_effects_snapshots.state_projection
// @formal reason: The existing committed usage cell and saturating token algebra are
// @formal reason: shared Agent vocabulary; Runtime retains its compatible public paths.
//! Per-model token usage retained in committed Thread state.
use crate::agent::state::{MergePolicy, Scope, StateKey};
use crate::audit::model_request::TokenUsage;
use serde::{Deserialize, Serialize};

/// A thread's accumulated token usage **attributed per model** (a session may span
/// several models — per-Run overrides, sub-agents, native vs an external runtime).
/// The neutral truth the runtime records; adapters project the [`total`](Self::total)
/// (or the per-model breakdown) onto their wire. Stored under [`THREAD_USAGE_STATE_KEY`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUsage {
    /// `model_ref -> usage on that model`.
    pub by_model: std::collections::BTreeMap<String, TokenUsage>,
}

impl ThreadUsage {
    /// Add one step's usage to `model`'s running total.
    pub fn record(&mut self, model: &str, usage: TokenUsage) {
        let entry = self.by_model.entry(model.to_string()).or_default();
        *entry = entry.saturating_add(usage);
    }

    /// Fold another tally into this one, per model — the seam that rolls a
    /// sub-agent's usage (recorded on its own isolated thread) into the parent
    /// Thread's running total, so a delegated Run's tokens are not lost. Each
    /// model's counts accumulate independently (a sub-agent may run a different
    /// model than its parent).
    pub fn merge(&mut self, other: &ThreadUsage) {
        for (model, usage) in &other.by_model {
            self.record(model, *usage);
        }
    }

    /// True when no usage has been recorded (every sub-run over a deterministic
    /// model reports none — nothing to roll up).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_model.is_empty()
    }

    /// The session-level total across every model (what a session-usage field reports).
    #[must_use]
    pub fn total(&self) -> TokenUsage {
        self.by_model
            .values()
            .copied()
            .fold(TokenUsage::default(), TokenUsage::saturating_add)
    }

    /// Recover a thread's committed usage by scanning its committed state for the
    /// last `Set` on [`THREAD_USAGE_STATE_KEY`]. The single reader of the usage
    /// cell — shared by the host's session-usage read and the sub-agent rollup —
    /// so the scan lives beside the key, not copied at each call site.
    #[must_use]
    pub fn from_committed_state(commands: &[crate::agent::state::Command]) -> Self {
        use crate::agent::state::{Action, Scope};
        let mut usage = ThreadUsage::default();
        for cmd in commands {
            if cmd.scope == Scope::Thread
                && cmd.key.0 == THREAD_USAGE_STATE_KEY
                && let Action::Set(value) = &cmd.action
                && let Ok(parsed) = serde_json::from_value::<ThreadUsage>(value.clone())
            {
                usage = parsed;
            }
        }
        usage
    }
}

/// The thread-scoped committed-state key under which the run loop accumulates a
/// thread's [`ThreadUsage`] (serialized). Committed truth, so an adapter surfaces a
/// session's usage by reading thread state — the runtime records the fact without
/// naming any wire, and it survives a restart. Internal (the `__` prefix keeps it out
/// of any agent-authored state namespace).
pub const THREAD_USAGE_STATE_KEY: &str = "__usage";

/// Typed view over the thread-usage cell (ADR-0055). A read fails closed on a
/// shape drift (so a persisted run's accumulated tally is never silently reset),
/// and a write is `Commutative` so a step's tally shallow-merges the committed
/// object. It is write-only: the loop reads the tally, folds a step's usage via
/// [`ThreadUsage::record`], and writes the whole value back.
pub struct ThreadUsageKey;

impl StateKey for ThreadUsageKey {
    const KEY: &'static str = THREAD_USAGE_STATE_KEY;
    const SCOPE: Scope = Scope::Thread;
    const MERGE: MergePolicy = MergePolicy::Commutative;
    type Value = ThreadUsage;
}
