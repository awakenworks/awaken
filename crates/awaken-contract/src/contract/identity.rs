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
    fn run_origin_serde_roundtrip() {
        for origin in [RunOrigin::User, RunOrigin::Subagent, RunOrigin::Internal] {
            let json = serde_json::to_string(&origin).unwrap();
            let parsed: RunOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, origin);
        }
    }
}
