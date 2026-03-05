use super::spec::StateScope;

/// Routing context that resolves storage paths based on scope and call identity.
///
/// `ScopeContext` is threaded through the state reduction pipeline so that
/// `ToolCall`-scoped actions are transparently routed to a per-call namespace
/// (`__tool_call_scope.<call_id>.<base_path>`) without plugins needing to know
/// the call id.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    call_id: Option<String>,
}

impl ScopeContext {
    /// Create a scope context without a call id (used for Thread and Run scopes).
    pub fn run() -> Self {
        Self { call_id: None }
    }

    /// Create a scope context for a specific tool call.
    pub fn for_call(call_id: impl Into<String>) -> Self {
        Self {
            call_id: Some(call_id.into()),
        }
    }

    /// The call id, if this is a tool-call-scoped context.
    pub fn call_id(&self) -> Option<&str> {
        self.call_id.as_deref()
    }

    /// Resolve a (scope, base_path) pair to the actual storage path.
    ///
    /// - `Thread` / `Run` scope: returns `base_path` unchanged.
    /// - `ToolCall` scope with a call id: returns `__tool_call_scope.<id>.<base_path>`.
    /// - `ToolCall` scope without a call id: falls back to `base_path`.
    pub fn resolve_path(&self, scope: StateScope, base_path: &str) -> String {
        match (scope, &self.call_id) {
            (StateScope::ToolCall, Some(id)) => {
                format!("__tool_call_scope.{}.{}", id, base_path)
            }
            _ => base_path.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_scope_returns_base_path() {
        let ctx = ScopeContext::run();
        assert_eq!(ctx.resolve_path(StateScope::Run, "my_state"), "my_state");
    }

    #[test]
    fn run_scope_with_tool_call_scoped_state_falls_back() {
        let ctx = ScopeContext::run();
        assert_eq!(
            ctx.resolve_path(StateScope::ToolCall, "my_state"),
            "my_state"
        );
    }

    #[test]
    fn for_call_routes_tool_call_scope() {
        let ctx = ScopeContext::for_call("call_42");
        assert_eq!(
            ctx.resolve_path(StateScope::ToolCall, "my_plugin.tool_ctx"),
            "__tool_call_scope.call_42.my_plugin.tool_ctx"
        );
    }

    #[test]
    fn for_call_leaves_run_scope_unchanged() {
        let ctx = ScopeContext::for_call("call_42");
        assert_eq!(ctx.resolve_path(StateScope::Run, "my_state"), "my_state");
    }

    #[test]
    fn thread_scope_returns_base_path() {
        let ctx = ScopeContext::run();
        assert_eq!(
            ctx.resolve_path(StateScope::Thread, "reminders"),
            "reminders"
        );
    }

    #[test]
    fn for_call_leaves_thread_scope_unchanged() {
        let ctx = ScopeContext::for_call("call_42");
        assert_eq!(
            ctx.resolve_path(StateScope::Thread, "reminders"),
            "reminders"
        );
    }

    #[test]
    fn call_id_accessor() {
        assert_eq!(ScopeContext::run().call_id(), None);
        assert_eq!(ScopeContext::for_call("x").call_id(), Some("x"));
    }
}
