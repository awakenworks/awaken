//! Run identity and execution policy types.

use serde::{Deserialize, Serialize};

/// Origin of the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    /// End-user initiated run.
    #[default]
    User,
    /// Internal sub-agent delegated run.
    Subagent,
    /// Other internal origin.
    Internal,
}

/// Strongly typed identity for the active run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunIdentity {
    pub thread_id: String,
    pub parent_thread_id: Option<String>,
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub agent_id: String,
    pub origin: RunOrigin,
    pub parent_tool_call_id: Option<String>,
}

impl RunIdentity {
    #[must_use]
    pub fn for_thread(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            ..Self::default()
        }
    }

    #[must_use]
    pub fn new(
        thread_id: String,
        parent_thread_id: Option<String>,
        run_id: String,
        parent_run_id: Option<String>,
        agent_id: String,
        origin: RunOrigin,
    ) -> Self {
        Self {
            thread_id,
            parent_thread_id,
            run_id,
            parent_run_id,
            agent_id,
            origin,
            parent_tool_call_id: None,
        }
    }

    #[must_use]
    pub fn with_parent_tool_call_id(mut self, parent_tool_call_id: impl Into<String>) -> Self {
        let value = parent_tool_call_id.into();
        if !value.trim().is_empty() {
            self.parent_tool_call_id = Some(value);
        }
        self
    }

    pub fn thread_id_opt(&self) -> Option<&str> {
        let v = self.thread_id.trim();
        if v.is_empty() { None } else { Some(v) }
    }

    pub fn parent_thread_id_opt(&self) -> Option<&str> {
        self.parent_thread_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    pub fn run_id_opt(&self) -> Option<&str> {
        let v = self.run_id.trim();
        if v.is_empty() { None } else { Some(v) }
    }

    pub fn parent_run_id_opt(&self) -> Option<&str> {
        self.parent_run_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }

    pub fn agent_id_opt(&self) -> Option<&str> {
        let v = self.agent_id.trim();
        if v.is_empty() { None } else { Some(v) }
    }

    pub fn parent_tool_call_id_opt(&self) -> Option<&str> {
        self.parent_tool_call_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    }
}

/// Allow/exclude filter for a single resource kind (tools, skills, or agents).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterPolicy {
    allowed: Option<Vec<String>>,
    excluded: Option<Vec<String>>,
}

impl FilterPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allowed(&self) -> Option<&[String]> {
        self.allowed.as_deref()
    }

    pub fn excluded(&self) -> Option<&[String]> {
        self.excluded.as_deref()
    }

    pub fn set_allowed_if_absent(&mut self, values: Option<&[String]>) {
        if self.allowed.is_none() {
            self.allowed = Self::normalize(values);
        }
    }

    pub fn set_excluded_if_absent(&mut self, values: Option<&[String]>) {
        if self.excluded.is_none() {
            self.excluded = Self::normalize(values);
        }
    }

    fn normalize(values: Option<&[String]>) -> Option<Vec<String>> {
        let parsed: Vec<String> = values
            .into_iter()
            .flatten()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if parsed.is_empty() {
            None
        } else {
            Some(parsed)
        }
    }
}

/// Strongly typed scope and execution policy carried with a resolved run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunPolicy {
    pub tools: FilterPolicy,
    pub skills: FilterPolicy,
    pub agents: FilterPolicy,
}

impl RunPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_policy_normalizes_values() {
        let mut filter = FilterPolicy::new();
        filter.set_allowed_if_absent(Some(&[" a ".to_string(), "".to_string()]));
        assert_eq!(filter.allowed(), Some(&["a".to_string()][..]));
    }

    #[test]
    fn filter_policy_if_absent_does_not_overwrite() {
        let mut filter = FilterPolicy::new();
        filter.set_allowed_if_absent(Some(&["first".to_string()]));
        filter.set_allowed_if_absent(Some(&["second".to_string()]));
        assert_eq!(filter.allowed(), Some(&["first".to_string()][..]));
    }

    #[test]
    fn run_policy_delegates_to_filter_policy() {
        let mut policy = RunPolicy::new();
        policy
            .tools
            .set_allowed_if_absent(Some(&["read".to_string()]));
        policy
            .skills
            .set_excluded_if_absent(Some(&["debug".to_string()]));
        policy
            .agents
            .set_allowed_if_absent(Some(&["bot".to_string()]));
        assert_eq!(policy.tools.allowed(), Some(&["read".to_string()][..]));
        assert_eq!(policy.skills.excluded(), Some(&["debug".to_string()][..]));
        assert_eq!(policy.agents.allowed(), Some(&["bot".to_string()][..]));
    }

    #[test]
    fn run_identity_ignores_blank_parent_tool_call_id() {
        let identity = RunIdentity::new(
            "thread-1".to_string(),
            None,
            "run-1".to_string(),
            None,
            "agent-1".to_string(),
            RunOrigin::Internal,
        )
        .with_parent_tool_call_id("   ");
        assert!(identity.parent_tool_call_id_opt().is_none());
    }

    #[test]
    fn run_identity_for_thread() {
        let identity = RunIdentity::for_thread("t1");
        assert_eq!(identity.thread_id, "t1");
        assert!(identity.run_id.is_empty());
        assert_eq!(identity.origin, RunOrigin::User);
    }

    #[test]
    fn run_identity_opt_methods_trim_whitespace() {
        let identity = RunIdentity {
            thread_id: "  ".into(),
            parent_thread_id: Some(" p1 ".into()),
            run_id: " r1 ".into(),
            parent_run_id: Some(" pr1 ".into()),
            agent_id: " agent-1 ".into(),
            parent_tool_call_id: Some(" tc1 ".into()),
            ..Default::default()
        };
        assert!(identity.thread_id_opt().is_none());
        assert_eq!(identity.parent_thread_id_opt(), Some("p1"));
        assert_eq!(identity.run_id_opt(), Some("r1"));
        assert_eq!(identity.parent_run_id_opt(), Some("pr1"));
        assert_eq!(identity.agent_id_opt(), Some("agent-1"));
        assert_eq!(identity.parent_tool_call_id_opt(), Some("tc1"));
    }

    #[test]
    fn filter_policy_empty_values_normalize_to_none() {
        let mut tools = FilterPolicy::new();
        tools.set_excluded_if_absent(Some(&[" ".to_string(), "".to_string()]));
        assert!(tools.excluded().is_none());

        let mut agents = FilterPolicy::new();
        agents.set_allowed_if_absent(None);
        assert!(agents.allowed().is_none());
    }

    #[test]
    fn set_excluded_if_absent_does_not_overwrite() {
        let mut filter = FilterPolicy::new();
        filter.set_excluded_if_absent(Some(&["first".to_string()]));
        filter.set_excluded_if_absent(Some(&["second".to_string()]));
        assert_eq!(filter.excluded(), Some(&["first".to_string()][..]));
    }

    #[test]
    fn default_run_policy_all_none() {
        let policy = RunPolicy::new();
        assert!(policy.tools.allowed().is_none());
        assert!(policy.tools.excluded().is_none());
        assert!(policy.skills.allowed().is_none());
        assert!(policy.skills.excluded().is_none());
        assert!(policy.agents.allowed().is_none());
        assert!(policy.agents.excluded().is_none());
    }

    #[test]
    fn run_origin_serde_roundtrip() {
        for origin in [RunOrigin::User, RunOrigin::Subagent, RunOrigin::Internal] {
            let json = serde_json::to_string(&origin).unwrap();
            let parsed: RunOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, origin);
        }
    }

    // -- Lineage & identity propagation tests --

    #[test]
    fn run_origin_default_is_user() {
        let identity = RunIdentity::default();
        assert_eq!(identity.origin, RunOrigin::User);
    }

    #[test]
    fn run_identity_parent_fields_roundtrip() {
        let identity = RunIdentity::new(
            "child-thread-1".to_string(),
            Some("parent-thread-1".to_string()),
            "child-run-1".to_string(),
            Some("parent-run-1".to_string()),
            "child-agent".to_string(),
            RunOrigin::Subagent,
        )
        .with_parent_tool_call_id("tool-call-abc");

        // Verify all fields via direct access
        assert_eq!(identity.thread_id, "child-thread-1");
        assert_eq!(
            identity.parent_thread_id.as_deref(),
            Some("parent-thread-1")
        );
        assert_eq!(identity.run_id, "child-run-1");
        assert_eq!(identity.parent_run_id.as_deref(), Some("parent-run-1"));
        assert_eq!(identity.agent_id, "child-agent");
        assert_eq!(identity.origin, RunOrigin::Subagent);
        assert_eq!(
            identity.parent_tool_call_id.as_deref(),
            Some("tool-call-abc")
        );

        // Verify opt getters
        assert_eq!(identity.thread_id_opt(), Some("child-thread-1"));
        assert_eq!(identity.parent_thread_id_opt(), Some("parent-thread-1"));
        assert_eq!(identity.run_id_opt(), Some("child-run-1"));
        assert_eq!(identity.parent_run_id_opt(), Some("parent-run-1"));
        assert_eq!(identity.agent_id_opt(), Some("child-agent"));
        assert_eq!(identity.parent_tool_call_id_opt(), Some("tool-call-abc"));

        // Verify clone preserves all fields
        let cloned = identity.clone();
        assert_eq!(cloned, identity);
    }

    #[test]
    fn sub_agent_identity_construction_mirrors_local_backend() {
        // Simulates the identity chain that LocalBackend::execute builds
        // (lines 52-65 of local_backend.rs)
        let parent_run_id = "parent-run-uuid";
        let parent_thread_id = "parent-thread-uuid";
        let parent_tool_call_id = "call_42";
        let child_agent_id = "worker-agent";

        // Simulate the parent identity
        let parent_identity = RunIdentity::new(
            parent_thread_id.to_string(),
            None,
            parent_run_id.to_string(),
            None,
            "orchestrator".to_string(),
            RunOrigin::User,
        );

        // Build sub-identity the same way LocalBackend does
        let sub_run_id = "child-run-uuid".to_string();
        let sub_thread_id = sub_run_id.clone();
        let mut sub_identity = RunIdentity::new(
            sub_thread_id.clone(),
            Some(sub_thread_id),
            sub_run_id,
            Some(parent_identity.run_id.clone()),
            child_agent_id.to_string(),
            RunOrigin::Subagent,
        );
        sub_identity = sub_identity.with_parent_tool_call_id(parent_tool_call_id.to_string());

        // Verify lineage fields
        assert_eq!(sub_identity.origin, RunOrigin::Subagent);
        assert_eq!(sub_identity.parent_run_id.as_deref(), Some(parent_run_id));
        assert_eq!(
            sub_identity.parent_tool_call_id.as_deref(),
            Some(parent_tool_call_id)
        );
        assert_eq!(sub_identity.agent_id, child_agent_id);

        // Sub-agent's thread_id is its own, not the parent's
        assert_ne!(sub_identity.thread_id, parent_identity.thread_id);
        // Parent's run_id is linked
        assert_eq!(
            sub_identity.parent_run_id_opt(),
            Some(parent_identity.run_id.as_str())
        );
    }

    #[test]
    fn sub_agent_identity_without_parent_tool_call_id() {
        // When no parent_tool_call_id is provided (None branch in LocalBackend)
        let sub_identity = RunIdentity::new(
            "sub-thread".to_string(),
            Some("sub-thread".to_string()),
            "sub-run".to_string(),
            Some("parent-run".to_string()),
            "worker".to_string(),
            RunOrigin::Subagent,
        );

        assert_eq!(sub_identity.origin, RunOrigin::Subagent);
        assert_eq!(sub_identity.parent_run_id.as_deref(), Some("parent-run"));
        assert!(sub_identity.parent_tool_call_id.is_none());
        assert!(sub_identity.parent_tool_call_id_opt().is_none());
    }

    #[test]
    fn identity_chain_two_levels_deep() {
        // L0: user-initiated root
        let root = RunIdentity::new(
            "thread-root".to_string(),
            None,
            "run-root".to_string(),
            None,
            "orchestrator".to_string(),
            RunOrigin::User,
        );
        assert_eq!(root.origin, RunOrigin::User);
        assert!(root.parent_run_id.is_none());
        assert!(root.parent_thread_id.is_none());

        // L1: first sub-agent delegated from root
        let l1 = RunIdentity::new(
            "thread-l1".to_string(),
            Some("thread-l1".to_string()),
            "run-l1".to_string(),
            Some(root.run_id.clone()),
            "planner".to_string(),
            RunOrigin::Subagent,
        )
        .with_parent_tool_call_id("call-l0-to-l1");

        assert_eq!(l1.origin, RunOrigin::Subagent);
        assert_eq!(l1.parent_run_id.as_deref(), Some("run-root"));
        assert_eq!(l1.parent_tool_call_id.as_deref(), Some("call-l0-to-l1"));

        // L2: second sub-agent delegated from L1
        let l2 = RunIdentity::new(
            "thread-l2".to_string(),
            Some("thread-l2".to_string()),
            "run-l2".to_string(),
            Some(l1.run_id.clone()),
            "executor".to_string(),
            RunOrigin::Subagent,
        )
        .with_parent_tool_call_id("call-l1-to-l2");

        assert_eq!(l2.origin, RunOrigin::Subagent);
        assert_eq!(l2.parent_run_id.as_deref(), Some("run-l1"));
        assert_eq!(l2.parent_tool_call_id.as_deref(), Some("call-l1-to-l2"));
        assert_eq!(l2.agent_id, "executor");

        // Verify the chain is traceable: l2 -> l1 -> root
        assert_eq!(l2.parent_run_id.as_deref(), Some(l1.run_id.as_str()));
        assert_eq!(l1.parent_run_id.as_deref(), Some(root.run_id.as_str()));
    }
}
