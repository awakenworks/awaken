//! Runtime checkpoint read port.
//!
//! The trait and its `ThreadRunStore` adapter live in `awaken-runtime-contract`
//! (the runtime's read port belongs at the contract boundary). They are
//! re-exported here for the agent loop and embedders/tests.

pub use awaken_runtime_contract::contract::storage::{
    RuntimeCheckpointStore, ThreadRunCheckpointStore,
};
