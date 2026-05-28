//! Compatibility facade for Awaken contract crates.
//!
//! New runtime-facing code should depend on `awaken-runtime-contract`. New
//! server/store-facing code should depend on `awaken-server-contract`. This
//! crate preserves the historical `awaken-contract` import path by re-exporting
//! both surfaces.

#![allow(missing_docs)]

pub use awaken_runtime_contract::*;
pub use awaken_server_contract as server_contract;
pub use awaken_server_contract::contract;
pub use awaken_server_contract::*;
