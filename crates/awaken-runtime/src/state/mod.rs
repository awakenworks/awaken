mod persistence;
mod store;

pub use store::{CommitEvent, CommitHook, StateStore};

// Re-export contract state types for convenience
pub use awaken_runtime_contract::state::{
    KeyScope, MergeStrategy, MutationBatch, MutationOp, MutationTarget, PersistedState, Snapshot,
    StateCommand, StateKey, StateKeyOptions, StateMap,
};
