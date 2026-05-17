//! Allow/exclude tool filtering applied during agent resolution.

use std::collections::HashMap;
use std::sync::Arc;

use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

/// Apply allow/exclude filtering to a mutable tool map.
///
/// See [`super::catalog::tool_allowed`] for the semantics:
/// `(literals ∪ patterns) − (excluded literals ∪ excluded patterns)`,
/// deny always wins.
pub(super) fn filter_tools(tools: &mut HashMap<String, Arc<dyn Tool>>, spec: &AgentSpec) {
    tools.retain(|id, _| super::catalog::tool_allowed(spec, id));
}
