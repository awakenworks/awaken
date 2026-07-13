//! Run-end continuation guards: what a guard sees at the natural-end boundary and
//! the decision it returns. The runtime owns *when* the loop stops; a guard
//! supplies the predicate and any feedback.

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Store;

/// What a run-end guard sees when the model/tool loop reaches a natural end (a
/// text-only turn). Immutable: a guard reads the conversation and the run-scoped
/// forced-continuation count, then returns a decision.
pub struct RunEndContext<'a> {
    pub run_id: RunId,
    /// The full conversation transcript at the natural-end boundary.
    pub conversation: &'a [Message],
    /// How many times a guard has already steered this run — the runtime's
    /// run-scoped continuation counter. A guard reads it to enforce its own
    /// iteration budget; the runtime also caps total steps as a runaway backstop.
    pub forced_continuations: usize,
    /// The run's cancellation token, if any. A guard that grades through a judge
    /// sub-run forwards it, so cancelling the parent cancels the judge too rather
    /// than orphaning it.
    pub cancellation: Option<&'a tokio_util::sync::CancellationToken>,
    /// The run's read-only materialized state, so a continuation predicate can
    /// inspect accumulated state (e.g. whether a machine instance is terminal).
    pub state: &'a Store,
}

/// A run-end guard's decision at a natural-end boundary. The runtime owns *when*
/// the loop stops; the guard supplies the *predicate* and any feedback. `detail`
/// is opaque to the runtime (anti-corruption): the guard's own classification,
/// forwarded to the host without the kernel interpreting it.
pub enum RunEndDecision {
    /// End the run. `detail` is surfaced to the host as an opaque round result.
    Complete { detail: serde_json::Value },
    /// Continue for another turn: append `feedback` as a user message and loop.
    /// `detail` describes this non-terminal round, opaque to the runtime.
    Steer {
        feedback: String,
        detail: serde_json::Value,
    },
}

/// A run-end continuation guard: consulted at the natural-end boundary to decide
/// whether the run ends or takes another steered turn (e.g. goal/outcome
/// evaluation). The runtime consults registered guards in dependency order and
/// takes the first that steers; if none steer, the run ends carrying the last
/// guard's completion detail. Async so a guard can grade through an external
/// judge before deciding.
#[async_trait]
pub trait RunEndGuard: Send + Sync {
    /// Stable id, checked against the plugin's `CapabilityBound` (G30).
    fn id(&self) -> &str;
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision;
}
