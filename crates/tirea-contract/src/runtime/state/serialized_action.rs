use super::spec::{AnyStateAction, StateScope};
use crate::runtime::action::Action;
use crate::runtime::phase::step::StepContext;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tirea_state::StateSpec;

/// Serialized state action, sufficient to reconstruct an [`AnyStateAction::Typed`].
///
/// Captured at the point where a tool completes execution, before the batch
/// commit. On crash recovery, these entries are deserialized back into
/// `AnyStateAction` via [`ActionDeserializerRegistry`] and re-reduced against
/// the base state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializedAction {
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
pub enum PendingWriteError {
    #[error("unknown state type: {0}")]
    UnknownStateType(String),
    #[error("action deserialization failed for {state_type}: {source}")]
    DeserializationFailed {
        state_type: String,
        source: serde_json::Error,
    },
}

// ---------------------------------------------------------------------------
// AnyStateAction → SerializedAction
// ---------------------------------------------------------------------------

impl AnyStateAction {
    /// Convert this action into a serialized form for persistence.
    ///
    /// Returns `None` for raw `Patch` actions (they bypass the typed reducer
    /// pipeline and are not recoverable via action replay).
    pub fn to_serialized_action(&self) -> Option<SerializedAction> {
        match self {
            Self::Typed {
                state_type_name,
                scope,
                base_path,
                call_id_override,
                serialized_payload,
                ..
            } => Some(SerializedAction {
                state_type_name: (*state_type_name).to_owned(),
                base_path: (*base_path).to_owned(),
                scope: *scope,
                call_id_override: call_id_override.clone(),
                payload: serialized_payload.clone(),
            }),
            Self::Patch(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Action impl for AnyStateAction
// ---------------------------------------------------------------------------

impl Action for AnyStateAction {
    fn label(&self) -> &'static str {
        "state_action"
    }

    fn is_state_action(&self) -> bool {
        true
    }

    fn into_state_action(self: Box<Self>) -> Option<AnyStateAction> {
        Some(*self)
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.emit_state_action(*self);
    }
}

// ---------------------------------------------------------------------------
// ActionDeserializerRegistry
// ---------------------------------------------------------------------------

type ActionFactory =
    Box<dyn Fn(&SerializedAction) -> Result<AnyStateAction, PendingWriteError> + Send + Sync>;

/// Registry that maps `state_type_name` → factory closure for reconstructing
/// `AnyStateAction` from a [`SerializedAction`].
///
/// Built once at agent construction (alongside `StateScopeRegistry` and
/// `LatticeRegistry`) by calling `register::<S>()` for every `StateSpec` type.
pub struct ActionDeserializerRegistry {
    factories: HashMap<String, ActionFactory>,
}

impl ActionDeserializerRegistry {
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
            Box::new(|entry: &SerializedAction| {
                let action: S::Action =
                    serde_json::from_value(entry.payload.clone()).map_err(|e| {
                        PendingWriteError::DeserializationFailed {
                            state_type: entry.state_type_name.clone(),
                            source: e,
                        }
                    })?;
                match entry.scope {
                    StateScope::Thread | StateScope::Run => Ok(AnyStateAction::new::<S>(action)),
                    StateScope::ToolCall => {
                        let call_id = entry.call_id_override.as_deref().unwrap_or("");
                        Ok(AnyStateAction::new_for_call::<S>(
                            action,
                            call_id.to_owned(),
                        ))
                    }
                }
            }),
        );
    }

    /// Deserialize a [`SerializedAction`] back into an [`AnyStateAction`].
    pub fn deserialize(
        &self,
        entry: &SerializedAction,
    ) -> Result<AnyStateAction, PendingWriteError> {
        let factory = self
            .factories
            .get(&entry.state_type_name)
            .ok_or_else(|| PendingWriteError::UnknownStateType(entry.state_type_name.clone()))?;
        factory(entry)
    }
}

impl Default for ActionDeserializerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ActionDeserializerRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionDeserializerRegistry")
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
    fn to_serialized_action_roundtrip() {
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(42));
        let serialized = original.to_serialized_action().expect("Typed → Some");

        assert!(serialized.state_type_name.contains("TestCounter"));
        assert_eq!(serialized.base_path, "test_counter");
        assert_eq!(serialized.scope, StateScope::Thread);
        assert!(serialized.call_id_override.is_none());
        assert_eq!(serialized.payload, json!({"Increment": 42}));
    }

    #[test]
    fn to_serialized_action_returns_none_for_patch() {
        let raw =
            AnyStateAction::Patch(tirea_state::TrackedPatch::new(tirea_state::Patch::default()));
        assert!(raw.to_serialized_action().is_none());
    }

    #[test]
    fn registry_deserialize_and_reduce_roundtrip() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        // Create original action, serialize, then deserialize through registry
        let original = AnyStateAction::new::<TestCounter>(TestCounterAction::Increment(7));
        let serialized = original.to_serialized_action().unwrap();

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
        let registry = ActionDeserializerRegistry::new();
        let entry = SerializedAction {
            state_type_name: "unknown::Type".into(),
            base_path: "x".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!(null),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(err, PendingWriteError::UnknownStateType(_)));
    }

    #[test]
    fn registry_bad_payload_returns_deserialization_error() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<TestCounter>();

        let entry = SerializedAction {
            state_type_name: std::any::type_name::<TestCounter>().into(),
            base_path: "test_counter".into(),
            scope: StateScope::Run,
            call_id_override: None,
            payload: json!({"BadVariant": 99}),
        };
        let err = registry.deserialize(&entry).unwrap_err();
        assert!(matches!(
            err,
            PendingWriteError::DeserializationFailed { .. }
        ));
    }

    #[test]
    fn tool_call_scoped_roundtrip() {
        let mut registry = ActionDeserializerRegistry::new();
        registry.register::<ToolCallTestCounter>();

        let original = AnyStateAction::new_for_call::<ToolCallTestCounter>(
            TestCounterAction::Increment(3),
            "call_99",
        );
        let serialized = original.to_serialized_action().unwrap();
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
