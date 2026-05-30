//! Runtime checkpoint read port.
//!
//! `RuntimeCheckpointStore` is the narrow read port the runtime consumes; it
//! lives in `awaken-runtime-contract`. The runtime library references nothing
//! from `awaken-server-contract` (that would be a reverse dependency). The
//! `ThreadRunStore`-backed adapter (`ThreadRunCheckpointStore`) is a
//! server/store concern in server-contract; runtime tests that wire a
//! store-backed reader import it directly through the dev-dependency.

pub use awaken_runtime_contract::contract::storage::RuntimeCheckpointStore;
