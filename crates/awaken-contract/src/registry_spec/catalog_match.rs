//! Canonical catalog allow/exclude matcher for [`AgentSpec`].
//!
//! Final allow set = (allowed_tools ∪ allowed_tool_patterns matches)
//!                 − (excluded_tools ∪ excluded_tool_patterns matches)
//!
//! "Deny wins" — a tool excluded by either exclude field is dropped even
//! if it was in the allow set.
//!
//! Lives in `awaken-contract` so every consumer that holds an `AgentSpec`
//! (runtime resolve pipeline, server preview, future tooling) gets the
//! same answer instead of replicating the field traversal — and silently
//! drifting when new catalog fields are added.

use awaken_tool_pattern::tool_id_match;

use super::AgentSpec;

impl AgentSpec {
    /// Decide whether `tool_id` passes this spec's four catalog fields.
    ///
    /// This is the single source of truth for "is tool X visible to this
    /// agent?". Callers must use this rather than reading
    /// `allowed_tools` / `excluded_tools` directly — the literal fields
    /// alone do not capture pattern-based allow/exclude, and `None` on a
    /// literal field is NOT equivalent to "allow all".
    #[must_use]
    pub fn tool_allowed(&self, tool_id: &str) -> bool {
        in_allow_set(self, tool_id) && !in_exclude_set(self, tool_id)
    }
}

fn in_allow_set(spec: &AgentSpec, id: &str) -> bool {
    let literal_hit = spec
        .allowed_tools
        .as_deref()
        .is_some_and(|l| l.iter().any(|t| t == id));
    let pattern_hit = spec
        .allowed_tool_patterns
        .as_deref()
        .is_some_and(|l| l.iter().any(|p| tool_id_match(p, id)));
    literal_hit || pattern_hit
}

fn in_exclude_set(spec: &AgentSpec, id: &str) -> bool {
    let literal_hit = spec
        .excluded_tools
        .as_deref()
        .is_some_and(|l| l.iter().any(|t| t == id));
    let pattern_hit = spec
        .excluded_tool_patterns
        .as_deref()
        .is_some_and(|l| l.iter().any(|p| tool_id_match(p, id)));
    literal_hit || pattern_hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_spec() -> AgentSpec {
        // Use the deserialize migration shim to apply the legacy default
        // (allowed_tool_patterns=["*"]); ad-hoc struct construction would
        // require every field explicitly.
        serde_json::from_str(r#"{"id":"a","model_id":"m","system_prompt":""}"#).unwrap()
    }

    #[test]
    fn legacy_default_allows_all() {
        let spec = empty_spec();
        assert!(spec.tool_allowed("Bash"));
        assert!(spec.tool_allowed("mcp:weather"));
    }

    #[test]
    fn empty_allow_blocks_all() {
        let mut spec = empty_spec();
        spec.allowed_tool_patterns = Some(vec![]);
        spec.allowed_tools = Some(vec![]);
        assert!(!spec.tool_allowed("Bash"));
    }

    #[test]
    fn literal_allow_only() {
        let mut spec = empty_spec();
        spec.allowed_tool_patterns = Some(vec![]);
        spec.allowed_tools = Some(vec!["Bash".into(), "Read".into()]);
        assert!(spec.tool_allowed("Bash"));
        assert!(spec.tool_allowed("Read"));
        assert!(!spec.tool_allowed("Write"));
    }

    #[test]
    fn pattern_allow_only() {
        let mut spec = empty_spec();
        spec.allowed_tool_patterns = Some(vec!["mcp:*".into()]);
        spec.allowed_tools = Some(vec![]);
        assert!(spec.tool_allowed("mcp:weather"));
        assert!(!spec.tool_allowed("Bash"));
    }

    #[test]
    fn literal_and_pattern_union() {
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["Bash".into()]);
        spec.allowed_tool_patterns = Some(vec!["mcp:*".into()]);
        assert!(spec.tool_allowed("Bash"));
        assert!(spec.tool_allowed("mcp:weather"));
        assert!(!spec.tool_allowed("Read"));
    }

    #[test]
    fn exclude_literal_overrides_allow() {
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["Bash".into()]);
        spec.allowed_tool_patterns = Some(vec![]);
        spec.excluded_tools = Some(vec!["Bash".into()]);
        assert!(!spec.tool_allowed("Bash"));
    }

    #[test]
    fn exclude_pattern_overrides_allow_all() {
        let mut spec = empty_spec();
        spec.allowed_tool_patterns = Some(vec!["*".into()]);
        spec.excluded_tool_patterns = Some(vec!["dangerous-*".into()]);
        assert!(spec.tool_allowed("Bash"));
        assert!(!spec.tool_allowed("dangerous-delete"));
    }
}
