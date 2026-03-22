//! Tool call pattern matching engine.
//!
//! Provides types and functions for matching tool calls by name (glob/regex/exact)
//! and optionally by argument-level conditions on JSON fields. Used by permission
//! rules, reminder rules, and other extensions that need to match tool calls.

mod matcher;
mod parser;
mod types;

pub use matcher::{
    evaluate_field_condition, evaluate_op, op_precision, pattern_matches, resolve_path,
    schema_has_path, validate_pattern_fields, value_to_string, wildcard_match,
};
pub use parser::{PatternParseError, parse_pattern};
pub use types::{
    ArgMatcher, FieldCondition, MatchOp, MatchResult, PathSegment, Specificity, ToolCallPattern,
    ToolMatcher,
};
