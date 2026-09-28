//! Frozen Native pre-effect enrollment vocabulary. Recovery classification is
//! owned by the Run coordinator, not the Runtime port layer.
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey};
use serde::{Deserialize, Serialize};

/// Closed execution surface whose every possible external entry is journalled
/// by the Native runtime. The first supported profile intentionally contains no
/// model-visible tools or plugin execution callbacks. Model invocations still
/// use `ActiveModelOperation`; an empty model history is meaningful only after
/// this marker was committed before the first possible invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeAttemptCoverage {
    #[default]
    Unspecified,
    ModelOnlyV1,
    /// All external entry is through the canonical model journal/ToolBatch.
    /// Untracked hooks/actions/guards and effectful gates are excluded.
    JournalledV1,
}

/// Run-scoped coverage marker stored through the canonical ThreadCommit log.
pub struct ActiveNativeAttemptCoverage;

impl StateKey for ActiveNativeAttemptCoverage {
    const KEY: &'static str = "runtime.native_attempt_coverage.v1";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Disjoint;
    type Value = NativeAttemptCoverage;
}
