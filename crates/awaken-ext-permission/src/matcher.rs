//! Deprecated re-export shim for [`awaken_tool_pattern`].
//!
//! The matching logic now lives in `awaken-tool-pattern`. This module
//! re-exports the same symbols so external callers still compile; new
//! code should import from `awaken_tool_pattern` directly.
#![allow(deprecated)]

#[deprecated(
    since = "0.5.1",
    note = "import from `awaken_tool_pattern` directly; this shim will be removed in a future major release"
)]
pub use awaken_tool_pattern::{
    MatchResult, Specificity, evaluate_field_condition, evaluate_op, op_precision, pattern_matches,
    resolve_path, schema_has_path, validate_pattern_fields, wildcard_match,
};
