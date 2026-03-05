//! State trait for typed state access.
//!
//! The `State` trait provides a unified interface for typed access to JSON documents.
//! It is typically implemented via the derive macro `#[derive(State)]`.

use crate::{DocCell, LatticeRegistry, Op, Patch, Path, TireaResult, TrackedPatch};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Lifecycle scope of a [`StateSpec`] type.
///
/// Determines when the framework automatically resets the state:
///
/// - **`Thread`** — persists across runs, never automatically cleaned.
///   Examples: reminders, permission overrides, delegation records.
/// - **`Run`** — reset at the start of each run (in `prepare_run`).
///   Examples: run lifecycle state, per-run token counters.
/// - **`ToolCall`** — scoped to a single tool call, cleaned up after execution.
///   Examples: tool-call progress, suspended-call state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateScope {
    /// State that persists across runs (thread lifetime).
    Thread,
    /// State that is reset at the start of each run.
    Run,
    /// State that is scoped to a single tool call.
    ToolCall,
}

type CollectHook<'a> = Arc<dyn Fn(&Op) -> TireaResult<()> + Send + Sync + 'a>;

/// Collector for patch operations.
///
/// `PatchSink` collects operations that will be combined into a `Patch`.
/// It is used internally by `StateRef` types to automatically collect
/// all state modifications.
///
/// # Thread Safety
///
/// `PatchSink` uses a `Mutex` internally to support async contexts.
/// In single-threaded usage, the lock overhead is minimal.
pub struct PatchSink<'a> {
    ops: Option<&'a Mutex<Vec<Op>>>,
    on_collect: Option<CollectHook<'a>>,
}

impl<'a> PatchSink<'a> {
    /// Create a new PatchSink wrapping a Mutex.
    #[doc(hidden)]
    pub fn new(ops: &'a Mutex<Vec<Op>>) -> Self {
        Self {
            ops: Some(ops),
            on_collect: None,
        }
    }

    /// Create a new PatchSink with a collect hook.
    ///
    /// The hook is invoked after each operation is collected.
    #[doc(hidden)]
    pub fn new_with_hook(ops: &'a Mutex<Vec<Op>>, hook: CollectHook<'a>) -> Self {
        Self {
            ops: Some(ops),
            on_collect: Some(hook),
        }
    }

    /// Create a child sink that shares the same collector and hook.
    ///
    /// Nested state refs use this so write-through behavior is preserved.
    #[doc(hidden)]
    pub fn child(&self) -> Self {
        Self {
            ops: self.ops,
            on_collect: self.on_collect.clone(),
        }
    }

    /// Create a read-only PatchSink that errors on collect.
    ///
    /// Used for `SealedState::get()` where writes are a programming error.
    #[doc(hidden)]
    pub fn read_only() -> Self {
        Self {
            ops: None,
            on_collect: None,
        }
    }

    /// Collect an operation.
    #[inline]
    pub fn collect(&self, op: Op) -> TireaResult<()> {
        let ops = self.ops.ok_or_else(|| {
            crate::TireaError::invalid_operation("write attempted on read-only state reference")
        })?;
        let mut guard = ops.lock().map_err(|_| {
            crate::TireaError::invalid_operation("state operation collector mutex poisoned")
        })?;
        guard.push(op.clone());
        drop(guard);
        if let Some(hook) = &self.on_collect {
            hook(&op)?;
        }
        Ok(())
    }

    /// Get the inner Mutex reference (for creating nested PatchSinks).
    #[doc(hidden)]
    pub fn inner(&self) -> &'a Mutex<Vec<Op>> {
        self.ops
            .expect("PatchSink::inner called on read-only sink (programming error)")
    }
}

/// Pure state context with automatic patch collection.
pub struct StateContext<'a> {
    doc: &'a DocCell,
    ops: Mutex<Vec<Op>>,
}

impl<'a> StateContext<'a> {
    /// Create a new pure state context.
    pub fn new(doc: &'a DocCell) -> Self {
        Self {
            doc,
            ops: Mutex::new(Vec::new()),
        }
    }

    /// Get a typed state reference at the specified path.
    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        let base = parse_path(path);
        let hook: CollectHook<'_> = Arc::new(|op: &Op| self.doc.apply(op));
        T::state_ref(self.doc, base, PatchSink::new_with_hook(&self.ops, hook))
    }

    /// Get a typed state reference at the type's canonical path.
    ///
    /// Requires `T` to have `#[tirea(path = "...")]` set.
    /// Panics if `T::PATH` is empty.
    pub fn state_of<T: State>(&self) -> T::Ref<'_> {
        assert!(
            !T::PATH.is_empty(),
            "State type has no bound path; use state::<T>(path) instead"
        );
        self.state::<T>(T::PATH)
    }

    /// Extract collected operations as a plain patch.
    pub fn take_patch(&self) -> Patch {
        let ops = std::mem::take(&mut *self.ops.lock().unwrap());
        Patch::with_ops(ops)
    }

    /// Extract collected operations as a tracked patch with a source.
    pub fn take_tracked_patch(&self, source: impl Into<String>) -> TrackedPatch {
        TrackedPatch::new(self.take_patch()).with_source(source)
    }

    /// Check if any operations have been collected.
    pub fn has_changes(&self) -> bool {
        !self.ops.lock().unwrap().is_empty()
    }

    /// Get the number of operations collected.
    pub fn ops_count(&self) -> usize {
        self.ops.lock().unwrap().len()
    }
}

/// Parse a dot-separated path string into a `Path`.
pub fn parse_path(path: &str) -> Path {
    if path.is_empty() {
        return Path::root();
    }

    let mut result = Path::root();
    for segment in path.split('.') {
        if !segment.is_empty() {
            result = result.key(segment);
        }
    }
    result
}

/// Trait for types that can create typed state references.
///
/// This trait is typically derived using `#[derive(State)]`.
/// It provides the interface for creating `StateRef` types that
/// allow typed read/write access to JSON documents.
///
/// # Example
///
/// ```ignore
/// use tirea_state::State;
/// use tirea_state_derive::State;
///
/// #[derive(State)]
/// struct User {
///     pub name: String,
///     pub age: i64,
/// }
///
/// // In a StateContext:
/// let user = ctx.state::<User>("users.alice");
/// let name = user.name()?;
/// user.set_name("Alice");
/// user.set_age(30);
/// ```
pub trait State: Sized {
    /// The reference type that provides typed access.
    type Ref<'a>;

    /// Canonical JSON path for this state type.
    ///
    /// When set via `#[tirea(path = "...")]`, enables `state_of::<T>()` access
    /// without an explicit path argument. Empty string means no bound path.
    const PATH: &'static str = "";

    /// Create a state reference at the specified path.
    ///
    /// # Arguments
    ///
    /// * `doc` - The JSON document to read from
    /// * `base` - The base path for this state
    /// * `sink` - The operation collector
    fn state_ref<'a>(doc: &'a DocCell, base: Path, sink: PatchSink<'a>) -> Self::Ref<'a>;

    /// Deserialize this type from a JSON value.
    fn from_value(value: &Value) -> TireaResult<Self>;

    /// Serialize this type to a JSON value.
    fn to_value(&self) -> TireaResult<Value>;

    /// Register lattice fields into the given registry.
    ///
    /// Auto-generated by `#[derive(State)]` for structs with `#[tirea(lattice)]`
    /// fields. The default implementation is a no-op (no lattice fields).
    fn register_lattice(_registry: &mut LatticeRegistry) {}

    /// Return the JSON keys of fields annotated with `#[tirea(lattice)]`.
    ///
    /// Used by the reducer pipeline to emit `Op::LatticeMerge` (instead of
    /// `Op::set`) for CRDT fields, enabling proper conflict suppression.
    /// The default implementation returns an empty slice (no lattice fields).
    fn lattice_keys() -> &'static [&'static str] {
        &[]
    }

    /// Compare two instances and emit minimal ops for changed fields.
    ///
    /// The derive macro generates an optimized version that does typed
    /// per-field comparison and only serializes changed fields. Lattice
    /// fields emit `Op::LatticeMerge`; regular fields emit `Op::Set`.
    ///
    /// The default implementation serializes both values and diffs at JSON
    /// level. When `lattice_keys()` is non-empty, changed lattice fields
    /// emit `Op::LatticeMerge`; all others emit `Op::Set`.
    fn diff_ops(old: &Self, new: &Self, base_path: &Path) -> TireaResult<Vec<Op>> {
        let old_val = old.to_value()?;
        let new_val = new.to_value()?;
        if old_val == new_val {
            return Ok(Vec::new());
        }
        let lattice_keys = Self::lattice_keys();
        if lattice_keys.is_empty() {
            return Ok(vec![Op::set(base_path.clone(), new_val)]);
        }
        // Per-field diff with LatticeMerge for lattice fields
        Ok(diff_state_fields(
            &old_val,
            &new_val,
            base_path,
            lattice_keys,
        ))
    }

    /// Create a patch that sets this value at the root.
    fn to_patch(&self) -> TireaResult<Patch> {
        Ok(Patch::with_ops(vec![Op::set(
            Path::root(),
            self.to_value()?,
        )]))
    }
}

/// Diff two JSON objects field-by-field, emitting `Op::LatticeMerge` for
/// lattice-annotated fields and `Op::Set` / `Op::Delete` for the rest.
///
/// Used by the default `State::diff_ops` implementation for manual impls
/// that declare `lattice_keys()`.
fn diff_state_fields(
    old_value: &Value,
    new_value: &Value,
    base_path: &Path,
    lattice_keys: &[&str],
) -> Vec<Op> {
    let empty_obj = serde_json::Map::new();
    let old_obj = old_value.as_object().unwrap_or(&empty_obj);
    let new_obj = new_value.as_object().unwrap_or(&empty_obj);

    let mut ops = Vec::new();

    for (key, new_val) in new_obj {
        let old_val = old_obj.get(key);
        if old_val == Some(new_val) {
            continue;
        }
        let field_path = base_path.clone().key(key);
        if lattice_keys.contains(&key.as_str()) {
            ops.push(Op::lattice_merge(field_path, new_val.clone()));
        } else {
            ops.push(Op::set(field_path, new_val.clone()));
        }
    }

    for key in old_obj.keys() {
        if !new_obj.contains_key(key) {
            ops.push(Op::delete(base_path.clone().key(key)));
        }
    }

    ops
}

/// Extension trait providing convenience methods for State types.
pub trait StateExt: State {
    /// Create a state reference at the document root.
    fn at_root<'a>(doc: &'a DocCell, sink: PatchSink<'a>) -> Self::Ref<'a> {
        Self::state_ref(doc, Path::root(), sink)
    }
}

impl<T: State> StateExt for T {}

/// Extends [`State`] with a typed action and a pure reducer.
///
/// Implementors define what actions their state accepts and how the state
/// transitions in response. The kernel applies actions via type-erased
/// `AnyStateAction` without knowing the concrete types.
///
/// Scope (Run vs ToolCall) is determined at the call site — `AnyStateAction::new()`
/// for run-scoped state, `AnyStateAction::new_for_call()` for tool-call-scoped state —
/// rather than being encoded in the trait, keeping business semantics out of `tirea-state`.
///
/// # Usage
///
/// Typically generated by `#[derive(State)]` with `#[tirea(action = "...")]`:
///
/// ```ignore
/// #[derive(State, Clone, Serialize, Deserialize)]
/// #[tirea(path = "counters.main", action = "CounterAction")]
/// struct Counter { value: i64 }
///
/// impl Counter {
///     fn reduce(&mut self, action: CounterAction) {
///         match action {
///             CounterAction::Increment(n) => self.value += n,
///         }
///     }
/// }
/// ```
pub trait StateSpec: State + Clone + Sized + Send + 'static {
    /// The action type accepted by this state.
    type Action: serde::Serialize + serde::de::DeserializeOwned + Send + 'static;

    /// Lifecycle scope of this state type.
    ///
    /// Defaults to `Thread` (never automatically cleaned) for backward
    /// compatibility. Override via `#[tirea(scope = "run")]` or
    /// `#[tirea(scope = "tool_call")]` in the derive macro.
    const SCOPE: StateScope = StateScope::Thread;

    /// Pure reducer: apply an action to produce the next state.
    fn reduce(&mut self, action: Self::Action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_patch_sink_collect() {
        let ops = Mutex::new(Vec::new());
        let sink = PatchSink::new(&ops);

        sink.collect(Op::set(Path::root().key("a"), Value::from(1)))
            .unwrap();
        sink.collect(Op::set(Path::root().key("b"), Value::from(2)))
            .unwrap();

        let collected = ops.lock().unwrap();
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn test_patch_sink_collect_hook() {
        let ops = Mutex::new(Vec::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_hook = seen.clone();
        let hook = Arc::new(move |op: &Op| {
            seen_hook.lock().unwrap().push(format!("{:?}", op));
            Ok(())
        });
        let sink = PatchSink::new_with_hook(&ops, hook);

        sink.collect(Op::set(Path::root().key("a"), Value::from(1)))
            .unwrap();
        sink.collect(Op::delete(Path::root().key("b"))).unwrap();

        let collected = ops.lock().unwrap();
        assert_eq!(collected.len(), 2);
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn test_patch_sink_child_preserves_collect_and_hook() {
        let ops = Mutex::new(Vec::new());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_hook = seen.clone();
        let hook = Arc::new(move |op: &Op| {
            seen_hook.lock().unwrap().push(format!("{:?}", op));
            Ok(())
        });
        let sink = PatchSink::new_with_hook(&ops, hook);
        let child = sink.child();

        child
            .collect(Op::set(Path::root().key("nested"), Value::from(1)))
            .unwrap();

        assert_eq!(ops.lock().unwrap().len(), 1);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn test_patch_sink_read_only_child_collect_errors() {
        let sink = PatchSink::read_only();
        let child = sink.child();
        let err = child
            .collect(Op::set(Path::root().key("x"), Value::from(1)))
            .unwrap_err();
        assert!(matches!(err, crate::TireaError::InvalidOperation { .. }));
    }

    #[test]
    fn test_patch_sink_read_only_collect_errors() {
        let sink = PatchSink::read_only();
        let err = sink
            .collect(Op::set(Path::root().key("x"), Value::from(1)))
            .unwrap_err();
        assert!(matches!(err, crate::TireaError::InvalidOperation { .. }));
    }

    #[test]
    #[should_panic(expected = "read-only sink")]
    fn test_patch_sink_read_only_inner_panics() {
        let sink = PatchSink::read_only();
        let _ = sink.inner();
    }

    #[test]
    fn test_parse_path_empty() {
        let path = parse_path("");
        assert!(path.is_empty());
    }

    #[test]
    fn test_parse_path_nested() {
        let path = parse_path("tool_calls.call_123.data");
        assert_eq!(path.to_string(), "$.tool_calls.call_123.data");
    }

    #[test]
    fn test_state_context_collects_ops() {
        struct Counter;

        struct CounterRef<'a> {
            base: Path,
            sink: PatchSink<'a>,
        }

        impl<'a> CounterRef<'a> {
            fn set_value(&self, value: i64) -> TireaResult<()> {
                self.sink
                    .collect(Op::set(self.base.clone().key("value"), Value::from(value)))
            }
        }

        impl State for Counter {
            type Ref<'a> = CounterRef<'a>;

            fn state_ref<'a>(_: &'a DocCell, base: Path, sink: PatchSink<'a>) -> Self::Ref<'a> {
                CounterRef { base, sink }
            }

            fn from_value(_: &Value) -> TireaResult<Self> {
                Ok(Counter)
            }

            fn to_value(&self) -> TireaResult<Value> {
                Ok(Value::Null)
            }
        }

        let doc = DocCell::new(json!({"counter": {"value": 1}}));
        let ctx = StateContext::new(&doc);
        let counter = ctx.state::<Counter>("counter");
        counter.set_value(2).unwrap();

        assert!(ctx.has_changes());
        assert_eq!(ctx.ops_count(), 1);
        assert_eq!(ctx.take_patch().len(), 1);
    }
}
