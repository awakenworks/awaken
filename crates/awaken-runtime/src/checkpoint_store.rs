//! Runtime checkpoint read port.
//!
//! `RuntimeCheckpointStore` is the narrow read port the runtime consumes; it
//! lives in `awaken-runtime-contract`. The `ThreadRunStore`-backed adapter
//! moved to `awaken-server-contract` (it names the full store, a server/store
//! concern); it is re-exported here under `test-utils` for the runtime's own
//! tests that wire a store-backed reader.

pub use awaken_runtime_contract::contract::storage::RuntimeCheckpointStore;

#[cfg(feature = "test-utils")]
pub use awaken_server_contract::contract::store_traits::ThreadRunCheckpointStore;
