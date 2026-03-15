use super::spec::{AnyStateAction, StateScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tirea_state::StateSpec;

/// Serialized state action, sufficient to reconstruct an [`AnyStateAction`].
///
/// Captured at the point where a tool completes execution, before the batch
/// commit. On crash recovery, these entries are deserialized back into
/// `AnyStateAction` via [`StateActionDeserializerRegistry`] and re-reduced against
/// the base state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedStateAction {
    /// `std::any::type_name::<S>()` — used as the registry lookup key.
    pub state_type_name: String,
    /// `S::PATH` — the canonical JSON path for this state type.
    pub base_path: String,
    /// Whether this action targets thread-, run-, or tool-call-level state.
    pub scope: StateScope,
    /// When set, overrides the scope context call_id for path resolution.
    pub call_id_override: Option<String>,
    /// The serialized `S::Action` value.
    pub payload: Value,
}

/// Errors from action deserialization operations.
#[derive(Debug, thiserror::Error)]
pub enum StateActionDecodeError {
    #[error("unknown state type: {0}")]
    UnknownStateType(String),
    #[error("action deserialization failed for {state_type}: {source}")]
    DeserializationFailed {
        state_type: String,
        source: serde_json::Error,
    },
}

// ---------------------------------------------------------------------------
// AnyStateAction → SerializedStateAction
// ---------------------------------------------------------------------------

impl AnyStateAction {
    /// Convert this action into a serialized form for persistence.
    pub fn to_serialized_state_action(&self) -> SerializedStateAction {
        SerializedStateAction {
            state_type_name: self.state_type_name().to_owned(),
            base_path: self.base_path().to_owned(),
            scope: self.scope(),
            call_id_override: self.call_id_override().map(str::to_owned),
            payload: self.serialized_payload().clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// StateActionDeserializerRegistry
// ---------------------------------------------------------------------------

type ActionFactory = Box<
    dyn Fn(&SerializedStateAction) -> Result<AnyStateAction, StateActionDecodeError> + Send + Sync,
>;

/// Registry that maps `state_type_name` → factory closure for reconstructing
/// `AnyStateAction` from a [`SerializedStateAction`].
///
/// Built once at agent construction (alongside `StateScopeRegistry` and
/// `LatticeRegistry`) by calling `register::<S>()` for every `StateSpec` type.
pub struct StateActionDeserializerRegistry {
    factories: HashMap<String, ActionFactory>,
}

impl StateActionDeserializerRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a `StateSpec` type so its actions can be deserialized.
    pub fn register<S: StateSpec>(&mut self) {
        let type_name = std::any::type_name::<S>().to_owned();
        self.factories.insert(
            type_name,
            Box::new(|entry: &SerializedStateAction| {
                let action: S::Action =
                    serde_json::from_value(entry.payload.clone()).map_err(|e| {
                        StateActionDecodeError::DeserializationFailed {
                            state_type: entry.state_type_name.clone(),
                            source: e,
                        }
                    })?;
                match entry.scope {
                    StateScope::Thread | StateScope::Run => {
                        Ok(AnyStateAction::new_at::<S>(entry.base_path.clone(), action))
                    }
                    StateScope::ToolCall => {
                        let call_id = entry.call_id_override.as_deref().unwrap_or("");
                        Ok(AnyStateAction::new_for_call_at::<S>(
                            entry.base_path.clone(),
                            action,
                            call_id.to_owned(),
                        ))
                    }
                }
            }),
        );
    }

    /// Deserialize a [`SerializedStateAction`] back into an [`AnyStateAction`].
    pub fn deserialize(
        &self,
        entry: &SerializedStateAction,
    ) -> Result<AnyStateAction, StateActionDecodeError> {
        let factory = self.factories.get(&entry.state_type_name).ok_or_else(|| {
            StateActionDecodeError::UnknownStateType(entry.state_type_name.clone())
        })?;
        factory(entry)
    }
}

impl Default for StateActionDeserializerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for StateActionDeserializerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateActionDeserializerRegistry")
            .field(
                "registered_types",
                &self.factories.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::super::scope_context::ScopeContext;
    use super::super::spec::reduce_state_actions;
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use tirea_state::{apply_patch, DocCell, PatchSink, Path, State, TireaResult};

    // -- Test state type --

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct TestCounter {
        value: i64,
    }

    struct TestCounterRef;

    impl State for TestCounter {
        type Ref<'a> = TestCounterRef;
        const PATH: &'static str = "test_counter";

        fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
            TestCounterRef
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

    #[derive(Debug, Serialize, Deserialize)]
    enum TestCounterAction {
        Increment(i64),
        Reset,
    }

    impl StateSpec for TestCounter {
        type Action = TestCounterAction;

        fn reduce(&mut self, action: TestCounterAction) {
            match action {
                TestCounterAction::Increment(n) => self.value += n,
                TestCounterAction::Reset => self.value = 0,
            }
        }
    }

    // -- ToolCall-scoped test state type --

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct ToolCallTestCounter {
        value: i64,
    }

    struct ToolCallTestCounterRef;

    impl State for ToolCallTestCounter {
        type Ref<'a> = ToolCallTestCounterRef;
        const PATH: &'static str = "tc_counter";

        fn state_ref<'a>(_: &'a DocCell, _: Path, _: PatchSink<'a>) -> Self::Ref<'a> {
            ToolCallTestCounterRef
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

    impl StateSpec for ToolCallTestCounter {
        type Action = TestCounterAction;
        const SCOPE: StateScope = StateScope::ToolCall;

        fn reduce(&mut self, action: TestCounterAction) {
            match action {
                TestCounterAction::Increment(n) => self.value += n,
                TestCounterAction::Reset => self.value = 0,
            }
        }
    }

    #[test]
    fn to_serialized_state_action_roundtrip() {
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(42));
        let serialized = original.to_serialized_state_action();

        assert!(serialized.state_type_name.contains("TestCounter"));
        assert_eq!(serialized.base_path, "test_counter");
        assert_eq!(serialized.scope, StateScope::Thread);
        assert!(serialized.call_id_override.is_none());
        assert_eq!(serialized.payload, json!({"Increment": 42}));
    }

    #[test]
    fn registry_deserialize_and_reduce_roundtrip() {
        let mut registry = StateActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        // Create original action, serialize, then deserialize through registry
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(7));
        let serialized = original.to_serialized_state_action();

        let reconstructed = registry.deserialize(&serialized).unwrap();

        // Reduce both against the same base state
        let base = json!({});
        let original_patches =
            reduce_state_actions(vec![original], &base, "test", &ScopeContext::run()).unwrap();
        let reconstructed_patches =
            reduce_state_actions(vec![reconstructed], &base, "test", &ScopeContext::run()).unwrap();

        // Both should produce identical results
        let result_a = apply_patch(&base, original_patches[0].patch()).unwrap();
        let result_b = apply_patch(&base, reconstructed_patches[0].patch()).unwrap();
        assert_eq!(result_a, result_b);
        assert_eq!(result_a["test_counter"]["value"], 7);
    }

    #[test]
    fn registry_unknown_type_returns_error() {
        let registry = StateActionDeserializerRegistry::new();
        let entry = SerializedStateAction {
            state_type_name: "unknown::Type".into(),
            base_path: "x".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!(null),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(err, StateActionDecodeError::UnknownStateType(_)));
    }

    #[test]
    fn registry_bad_payload_returns_deserialization_error() {
        let mut registry = StateActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let entry = SerializedStateAction {
            state_type_name: std::any::type_name::<TestCounter>().into(),
            base_path: "test_counter".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!({"BadVariant": 99}),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(
            err,
            StateActionDecodeError::DeserializationFailed { .. }
        ));
    }

    #[test]
    fn tool_call_scoped_roundtrip() {
        let mut registry = StateActionDeserializerRegistry::new();
        registry.register::<ToolCallTestCounter>();

        let original = AnyStateAction::new_for_call::<ToolCallTestCounter>(
            TestCounterAction::Increment(3),
            "call_99",
        );
        let serialized = original.to_serialized_state_action();
        assert_eq!(serialized.scope, StateScope::ToolCall);
        assert_eq!(serialized.call_id_override, Some("call_99".into()));

        let reconstructed = registry.deserialize(&serialized).unwrap();

        let base = json!({});
        let patches =
            reduce_state_actions(vec![reconstructed], &base, "test", &ScopeContext::run()).unwrap();
        let result = apply_patch(&base, patches[0].patch()).unwrap();
        assert_eq!(
            result["__tool_call_scope"]["call_99"]["tc_counter"]["value"],
            3
        );
    }
}
