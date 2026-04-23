//! Compatibility package that re-exports the primary `awaken` umbrella crate.
//!
//! New code should depend on the published `awaken` package directly. This
//! package exists so existing `awaken-agent` users can move to the new release
//! line without changing import paths.

pub use awaken::*;
