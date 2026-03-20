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

/// Strongly typed scope and execution policy carried with a resolved run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunPolicy {
    allowed_tools: Option<Vec<String>>,
    excluded_tools: Option<Vec<String>>,
    allowed_skills: Option<Vec<String>>,
    excluded_skills: Option<Vec<String>>,
    allowed_agents: Option<Vec<String>>,
    excluded_agents: Option<Vec<String>>,
}

impl RunPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.allowed_tools.as_deref()
    }

    pub fn excluded_tools(&self) -> Option<&[String]> {
        self.excluded_tools.as_deref()
    }

    pub fn allowed_skills(&self) -> Option<&[String]> {
        self.allowed_skills.as_deref()
    }

    pub fn excluded_skills(&self) -> Option<&[String]> {
        self.excluded_skills.as_deref()
    }

    pub fn allowed_agents(&self) -> Option<&[String]> {
        self.allowed_agents.as_deref()
    }

    pub fn excluded_agents(&self) -> Option<&[String]> {
        self.excluded_agents.as_deref()
    }

    pub fn set_allowed_tools_if_absent(&mut self, values: Option<&[String]>) {
        if self.allowed_tools.is_none() {
            self.allowed_tools = normalize_scope_values(values);
        }
    }

    pub fn set_excluded_tools_if_absent(&mut self, values: Option<&[String]>) {
        if self.excluded_tools.is_none() {
            self.excluded_tools = normalize_scope_values(values);
        }
    }

    pub fn set_allowed_skills_if_absent(&mut self, values: Option<&[String]>) {
        if self.allowed_skills.is_none() {
            self.allowed_skills = normalize_scope_values(values);
        }
    }

    pub fn set_excluded_skills_if_absent(&mut self, values: Option<&[String]>) {
        if self.excluded_skills.is_none() {
            self.excluded_skills = normalize_scope_values(values);
        }
    }

    pub fn set_allowed_agents_if_absent(&mut self, values: Option<&[String]>) {
        if self.allowed_agents.is_none() {
            self.allowed_agents = normalize_scope_values(values);
        }
    }

    pub fn set_excluded_agents_if_absent(&mut self, values: Option<&[String]>) {
        if self.excluded_agents.is_none() {
            self.excluded_agents = normalize_scope_values(values);
        }
    }
}

fn normalize_scope_values(values: Option<&[String]>) -> Option<Vec<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_policy_normalizes_values() {
        let mut policy = RunPolicy::new();
        policy.set_allowed_tools_if_absent(Some(&[" a ".to_string(), "".to_string()]));
        assert_eq!(policy.allowed_tools(), Some(&["a".to_string()][..]));
    }

    #[test]
    fn run_policy_if_absent_does_not_overwrite() {
        let mut policy = RunPolicy::new();
        policy.set_allowed_tools_if_absent(Some(&["first".to_string()]));
        policy.set_allowed_tools_if_absent(Some(&["second".to_string()]));
        assert_eq!(policy.allowed_tools(), Some(&["first".to_string()][..]));
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
            run_id: " r1 ".into(),
            ..Default::default()
        };
        assert!(identity.thread_id_opt().is_none());
        assert_eq!(identity.run_id_opt(), Some("r1"));
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
