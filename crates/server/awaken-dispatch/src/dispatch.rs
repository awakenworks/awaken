//! The dispatch ports now live in `awaken-dispatch-contract` (ADR-0039 2.1),
//! re-exported here so the host's internal `crate::dispatch::*` paths and existing
//! consumers are unchanged.

pub use awaken_dispatch_contract::dispatch::*;
