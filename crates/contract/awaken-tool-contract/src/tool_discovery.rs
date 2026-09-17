//! Stable provider-neutral input vocabulary for tool discovery.
//!
//! Query interpretation, ranking, and reveal persistence are runtime behavior
//! and deliberately do not live in this contract crate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum number of definitions one discovery operation may reveal. The same
/// constrained value is shared by portable discovery and native provider
/// projections so their public limits cannot drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ToolSearchLimit(#[schemars(range(min = 1, max = 50))] u8);

impl ToolSearchLimit {
    pub const MAX: u8 = 50;

    pub fn new(value: u8) -> Result<Self, ToolSearchLimitError> {
        if (1..=Self::MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ToolSearchLimitError(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ToolSearchLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("tool search limit must be between 1 and {max}, got {0}", max = ToolSearchLimit::MAX)]
pub struct ToolSearchLimitError(u8);

/// Provider-facing arguments. This boundary DTO is schema-derived and rejects
/// unknown members; runtime code normalizes the text before catalog search.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolSearchInput {
    #[schemars(length(min = 1, max = 500))]
    pub query: String,
    #[serde(default)]
    pub max_results: Option<ToolSearchLimit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_search_limit_makes_zero_and_oversized_values_unconstructable() {
        // Causal graph: C1 the boundary receives the lower edge, upper edge,
        // zero, or an oversized value. E1 only the closed interval 1..=50 enters
        // domain state. Invariant: downstream search never handles zero or an
        // unbounded allocation request.
        assert_eq!(ToolSearchLimit::new(1).unwrap().get(), 1);
        assert_eq!(ToolSearchLimit::new(50).unwrap().get(), 50);
        assert!(ToolSearchLimit::new(0).is_err());
        assert!(ToolSearchLimit::new(51).is_err());
    }
}

#[cfg(kani)]
#[kani::proof]
fn every_u8_is_admitted_exactly_when_it_is_within_the_search_limit() {
    let value: u8 = kani::any();
    assert_eq!(
        ToolSearchLimit::new(value).is_ok(),
        (1..=ToolSearchLimit::MAX).contains(&value)
    );
}
