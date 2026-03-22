//! Pattern matching engine — re-exports from [`awaken_contract::tool_pattern`].
//!
//! The matching logic has been extracted to `awaken-contract` so it can be
//! reused by other extensions (e.g. reminder, observability).

pub use awaken_contract::tool_pattern::{
    MatchResult, Specificity, evaluate_field_condition, evaluate_op, op_precision, pattern_matches,
    resolve_path, schema_has_path, validate_pattern_fields, wildcard_match,
};
