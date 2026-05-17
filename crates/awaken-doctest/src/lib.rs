//! Compile-tests for Awaken documentation code examples.
//!
//! ## How this works
//!
//! Each `examples/*.rs` file under this crate is a self-contained smoke test
//! for one public API surface that user docs reference. CI runs
//! `cargo build --examples -p awaken-doctest` to catch API drift — if a
//! rename or signature change in the runtime breaks an example, the build
//! fails before the docs go stale.
//!
//! Adding coverage: drop a new `examples/<surface>.rs`. Keep it minimal
//! (no live LLM calls, no network) and exercise only the trait / struct
//! shape — `Box::new(...)` or `let _ = SpecStruct { ... }` is enough to
//! pin the contract.
//!
//! ## History
//!
//! Previously this crate used `doc_comment::doctest!` to compile-test
//! every `rust` fence in `docs/book/src/**/*.md` (mdBook source). The
//! Starlight docs migration stripped `rust,ignore` modifiers for Shiki
//! compatibility, which flipped ~170 display-only snippets into compiled
//! doctests — most of them pseudocode that never compiled standalone.
//! The macro was retired in favour of the explicit `examples/` approach.
