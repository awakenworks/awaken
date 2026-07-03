//! The in-memory reference store, re-exported (ADR-0039 slice 2.2).
//!
//! The canonical `CommitCoordinator` / `CheckpointReader` / `StreamSink`
//! implementation now lives in the `awaken-store-inmem` backend crate; this path
//! stays for the kernel's default local wiring and the test suite, so existing
//! `awaken_runtime::memory::*` users are unchanged.

pub use awaken_store_inmem::*;
