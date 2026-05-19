use std::collections::BTreeSet;

use awaken_ext_deferred_tools::state::DeferralStateValue;
use awaken_tool_pattern::{parse_pattern, pattern_matches};
use serde_json::Value;

pub(crate) fn promoted_deferred_tool_ids(
    explicit_tool_ids: &[String],
    tool_patterns: &[String],
    deferral_state: &DeferralStateValue,
) -> Vec<String> {
    let mut promoted = explicit_tool_ids.iter().cloned().collect::<BTreeSet<_>>();
    if !tool_patterns.is_empty() {
        let deferred_tool_ids = deferral_state.deferred_tool_ids();
        for pattern_str in tool_patterns {
            let Ok(pattern) = parse_pattern(pattern_str) else {
                continue;
            };
            for tool_id in &deferred_tool_ids {
                if pattern_matches(&pattern, tool_id, &Value::Null).is_match() {
                    promoted.insert((*tool_id).to_string());
                }
            }
        }
    }
    promoted.into_iter().collect()
}
