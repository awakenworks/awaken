use serde_json::Value;
use std::any::TypeId;
use std::fmt;
use tirea_state::{get_at_path, parse_path, Op, Patch, Path, State, TireaResult};

type ApplyFn = Box<dyn FnOnce(&Value) -> TireaResult<Patch> + Send>;

/// Extends [`State`] with a typed action and a pure reducer.
///
/// Implementors define what actions their state accepts and how the state
/// transitions in response. The kernel applies actions via [`AnyStateAction`]
/// without knowing the concrete types.
///
/// # Example
///
/// ```ignore
/// impl StateSpec for Counter {
///     type Action = CounterAction;
///     fn reduce(&mut self, action: CounterAction) {
///         match action {
///             CounterAction::Increment(n) => self.value += n,
///             CounterAction::Reset => self.value = 0,
///         }
///     }
/// }
/// ```
pub trait StateSpec: State + Sized + Send + 'static {
    /// The action type accepted by this state.
    type Action: Send + 'static;

    /// Pure reducer: apply an action to produce the next state.
    fn reduce(&mut self, action: Self::Action);
}

/// Type-erased state action that can be applied to a JSON document.
///
/// Created via [`AnyStateAction::new`] which captures the concrete `StateSpec`
/// type and action in a closure. The kernel applies these after each phase hook
/// returns, without needing to know the concrete state/action types.
pub struct AnyStateAction {
    state_type_id: TypeId,
    state_type_name: &'static str,
    apply_fn: ApplyFn,
}

impl AnyStateAction {
    /// Create a type-erased action targeting state `S`.
    ///
    /// # Panics
    ///
    /// Panics if `S::PATH` is empty (state must have a bound path).
    pub fn new<S: StateSpec>(action: S::Action) -> Self {
        assert!(
            !S::PATH.is_empty(),
            "StateSpec type has no bound path; cannot create AnyStateAction"
        );

        Self {
            state_type_id: TypeId::of::<S>(),
            state_type_name: std::any::type_name::<S>(),
            apply_fn: Box::new(move |doc: &Value| {
                let path = parse_path(S::PATH);
                let sub_doc = get_at_path(doc, &path).cloned().unwrap_or(Value::Null);
                let mut state = S::from_value(&sub_doc)?;
                state.reduce(action);
                let new_value = state.to_value()?;
                Ok(Patch::with_ops(vec![Op::set(
                    path_from_str(S::PATH),
                    new_value,
                )]))
            }),
        }
    }

    /// The [`TypeId`] of the state type this action targets.
    pub fn state_type_id(&self) -> TypeId {
        self.state_type_id
    }

    /// Human-readable name of the state type (for diagnostics).
    pub fn state_type_name(&self) -> &str {
        self.state_type_name
    }

    /// Apply this action to a JSON document, producing a patch.
    ///
    /// Consumes `self` since the inner closure is `FnOnce`.
    pub fn apply(self, doc: &Value) -> TireaResult<Patch> {
        (self.apply_fn)(doc)
    }
}

impl fmt::Debug for AnyStateAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyStateAction")
            .field("state", &self.state_type_name)
            .field("type_id", &self.state_type_id)
            .finish()
    }
}

/// Convert a dot-separated path string to a `Path` for use in `Op::set`.
fn path_from_str(s: &str) -> Path {
    let mut path = Path::root();
    for seg in s.split('.') {
        if !seg.is_empty() {
            path = path.key(seg);
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use tirea_state::{apply_patch, DocCell, PatchSink, Path as TPath};

    // -- Manual State + StateSpec impl for testing --

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct Counter {
        value: i64,
    }

    impl Default for Counter {
        fn default() -> Self {
            Self { value: 0 }
        }
    }

    struct CounterRef;

    impl State for Counter {
        type Ref<'a> = CounterRef;
        const PATH: &'static str = "counters.main";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            CounterRef
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

    #[derive(Debug)]
    enum CounterAction {
        Increment(i64),
        Reset,
    }

    impl StateSpec for Counter {
        type Action = CounterAction;

        fn reduce(&mut self, action: CounterAction) {
            match action {
                CounterAction::Increment(n) => self.value += n,
                CounterAction::Reset => self.value = 0,
            }
        }
    }

    // -- No-path state for panic test --

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct Unbound {
        x: i64,
    }

    struct UnboundRef;

    impl State for Unbound {
        type Ref<'a> = UnboundRef;
        // PATH defaults to "" (no bound path)

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            UnboundRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    impl StateSpec for Unbound {
        type Action = ();
        fn reduce(&mut self, _: ()) {}
    }

    // -- Tests --

    #[test]
    fn any_state_action_increment() {
        let doc = json!({"counters": {"main": {"value": 5}}});
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(3));
        let patch = action.apply(&doc).unwrap();

        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 8);
    }

    #[test]
    fn any_state_action_reset() {
        let doc = json!({"counters": {"main": {"value": 42}}});
        let action = AnyStateAction::new::<Counter>(CounterAction::Reset);
        let patch = action.apply(&doc).unwrap();

        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 0);
    }

    #[test]
    fn any_state_action_missing_path_defaults() {
        let doc = json!({});
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        let patch = action.apply(&doc).unwrap();

        let result = apply_patch(&doc, &patch).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 1);
    }

    #[test]
    fn any_state_action_label() {
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        assert!(action.state_type_name().contains("Counter"));
    }

    #[test]
    fn any_state_action_debug() {
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        let debug = format!("{action:?}");
        assert!(debug.contains("AnyStateAction"));
        assert!(debug.contains("Counter"));
    }

    #[test]
    fn any_state_action_state_type_id() {
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        assert_eq!(action.state_type_id(), TypeId::of::<Counter>());
    }

    #[test]
    #[should_panic(expected = "no bound path")]
    fn any_state_action_panics_on_empty_path() {
        let _ = AnyStateAction::new::<Unbound>(());
    }
}
