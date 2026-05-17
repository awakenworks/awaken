//! Compile-tests for Awaken documentation code examples.
//!
//! ## Status
//!
//! Empty after the Starlight docs migration retired the previous mdBook-
//! based `book_doctests!()` macro. That macro compiled every `rust` fence
//! in `docs/book/src/**/*.md` as a doctest; the migration to Starlight
//! stripped `rust,ignore` modifiers (for Shiki compatibility), which would
//! have flipped ~170 display-only snippets into compiled doctests — many of
//! them pseudocode that never compiled standalone.
//!
//! ## Going forward
//!
//! Reintroduce doc snippet coverage by adding self-contained `.rs` files
//! under `examples/` here, one per API surface the docs reference. CI then
//! runs `cargo build --examples -p awaken-doctest` and `cargo test --examples
//! -p awaken-doctest`. Docs in `apps/www/src/content/docs/` remain pure
//! documentation, not test fixtures — they can cite example files by name
//! without coupling their formatting to the test toolchain.
