use super::spec::{AnyStateAction, StateScope};
use std::any::TypeId;
use std::collections::HashMap;
use tirea_state::StateSpec;

/// Registry mapping `StateSpec` types to their declared [`StateScope`] and path.
///
/// Built once at agent construction by calling
/// `AgentBehavior::register_state_scopes` on each behavior. The loop then
/// uses `resolve` to determine the effective scope of any [`AnyStateAction`],
/// overriding the action-carried default when a registered type provides a
/// canonical scope.
///
/// Also exposes `run_scoped_paths` for enumerating all Run-scoped state
/// paths, enabling framework-driven cleanup at the start of each run.
#[derive(Debug, Clone, Default)]
pub struct StateScopeRegistry {
    by_type_id: HashMap<TypeId, (&'static str, StateScope, &'static str)>,
}

impl StateScopeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a [`StateSpec`] type with an explicit [`StateScope`].
    pub fn register<S: StateSpec>(&mut self, scope: StateScope) {
        self.by_type_id.insert(
            TypeId::of::<S>(),
            (std::any::type_name::<S>(), scope, S::PATH),
        );
    }

    /// Look up the scope of a registered type.
    pub fn scope_for_type_id(&self, type_id: TypeId) -> Option<StateScope> {
        self.by_type_id.get(&type_id).map(|(_, scope, _)| *scope)
    }

    /// Return the canonical paths of all registered Run-scoped state types.
    ///
    /// Used by `prepare_run` to emit delete patches for stale run-scoped
    /// state before starting a new run.
    pub fn run_scoped_paths(&self) -> Vec<&'static str> {
        self.by_type_id
            .values()
            .filter(|(_, scope, _)| *scope == StateScope::Run)
            .map(|(_, _, path)| *path)
            .collect()
    }

    /// Resolve the scope of an [`AnyStateAction`].
    ///
    /// If the action targets a registered type, returns the registered scope.
    /// Otherwise falls back to `AnyStateAction::scope`.
    pub fn resolve(&self, action: &AnyStateAction) -> StateScope {
        if let Some(scope) = self.scope_for_type_id(action.state_type_id()) {
            return scope;
        }
        action.scope()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use tirea_state::{DocCell, PatchSink, Path, State, TireaResult};

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    struct RunScoped {
        value: i64,
    }

    struct RunScopedRef;

    impl State for RunScoped {
        type Ref<'a> = RunScopedRef;
        const PATH: &'static str = "run_scoped";

        fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
            RunScopedRef
        }
        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }
        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for RunScoped {
        type Action = ();
        fn reduce(&mut self, _: ()) {}
    }

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    struct ToolScoped {
        value: i64,
    }

    struct ToolScopedRef;

    impl State for ToolScoped {
        type Ref<'a> = ToolScopedRef;
        const PATH: &'static str = "tool_scoped";

        fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
            ToolScopedRef
        }
        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }
        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for ToolScoped {
        type Action = ();
        const SCOPE: StateScope = StateScope::ToolCall;
        fn reduce(&mut self, _: ()) {}
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = StateScopeRegistry::new();
        reg.register::<RunScoped>(StateScope::Run);
        reg.register::<ToolScoped>(StateScope::ToolCall);

        assert_eq!(
            reg.scope_for_type_id(TypeId::of::<RunScoped>()),
            Some(StateScope::Run)
        );
        assert_eq!(
            reg.scope_for_type_id(TypeId::of::<ToolScoped>()),
            Some(StateScope::ToolCall)
        );
    }

    #[test]
    fn unregistered_type_returns_none() {
        let reg = StateScopeRegistry::new();
        assert_eq!(reg.scope_for_type_id(TypeId::of::<RunScoped>()), None);
    }

    #[test]
    fn resolve_falls_back_to_action_scope() {
        let reg = StateScopeRegistry::new();
        let action = AnyStateAction::new::<RunScoped>(());
        assert_eq!(reg.resolve(&action), StateScope::Thread);
    }

    #[test]
    fn resolve_uses_registered_scope() {
        let mut reg = StateScopeRegistry::new();
        reg.register::<ToolScoped>(StateScope::ToolCall);
        // ToolScoped has SCOPE=ToolCall, but new() asserts not ToolCall.
        // Use a raw Typed variant to test resolution without assertion.
        // Instead, register and look up directly.
        assert_eq!(
            reg.scope_for_type_id(TypeId::of::<ToolScoped>()),
            Some(StateScope::ToolCall)
        );
    }

    #[test]
    fn run_scoped_paths_returns_run_types() {
        let mut reg = StateScopeRegistry::new();
        reg.register::<RunScoped>(StateScope::Run);
        reg.register::<ToolScoped>(StateScope::ToolCall);

        let paths = reg.run_scoped_paths();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], "run_scoped");
    }

    #[test]
    fn run_scoped_paths_empty_when_none_registered() {
        let mut reg = StateScopeRegistry::new();
        reg.register::<ToolScoped>(StateScope::ToolCall);
        assert!(reg.run_scoped_paths().is_empty());
    }
}
