//! Re-export cancellation types from awaken-contract.
//!
//! The canonical definition now lives in `awaken_contract::cancellation`.
//! This module preserves `crate::cancellation::*` import paths within the runtime.

pub use awaken_contract::cancellation::{CancellationHandle, CancellationToken};
