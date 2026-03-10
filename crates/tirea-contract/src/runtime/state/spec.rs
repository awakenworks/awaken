use super::scope_context::ScopeContext;
use serde_json::Value;
use std::any::TypeId;
use std::fmt;
use tirea_state::{
    apply_patch_with_registry, get_at_path, parse_path, LatticeRegistry, Patch, Path, TireaResult,
    TrackedPatch,
};

// Re-export from tirea-state so downstream code still works.
pub use tirea_state::{StateScope, StateSpec};

type ReduceFn = Box<dyn FnOnce(&Value, &str) -> TireaResult<Patch> + Send>;

/// Type-erased state action that can be applied to a JSON document.
///
/// Created via [`AnyStateAction::new`] / [`new_at`](Self::new_at) /
/// [`new_for_call`](Self::new_for_call) from a concrete `StateSpec` type and
/// reducer action.
pub struct AnyStateAction {
    state_type_id: TypeId,
    state_type_name: &'static str,
    scope: StateScope,
    base_path: String,
    /// When set, overrides the `ScopeContext` call_id for path resolution.
    /// Used by recovery/framework-internal scenarios that must target a
    /// specific call_id without a live `ScopeContext`.
    call_id_override: Option<String>,
    reduce_fn: ReduceFn,
    /// Type-erased lattice registration function captured from `S::register_lattice`.
    /// Enables `reduce_state_actions` to build a local registry for
    /// CRDT-aware rolling snapshot application.
    register_lattice: fn(&mut LatticeRegistry),
    /// Serialized action payload captured before the action is moved into
    /// `reduce_fn`. Enables pending-write persistence for crash recovery
    /// without requiring access to the concrete action type.
    serialized_payload: Value,
}

impl AnyStateAction {
    /// Create a type-erased action for non-ToolCall-scoped state `S`.
    ///
    /// The scope is read from `S::SCOPE` (Thread or Run). For ToolCall-scoped
    /// state, use [`new_for_call`](Self::new_for_call) instead.
    ///
    /// # Panics
    ///
    /// Panics if `S::PATH` is empty or `S::SCOPE` is `ToolCall`.
    pub fn new<S: StateSpec>(action: S::Action) -> Self {
        assert!(
            S::SCOPE != StateScope::ToolCall,
            "ToolCall-scoped state must use new_for_call(); got new() for {}",
            std::any::type_name::<S>(),
        );
        Self::build::<S>(action, S::SCOPE, S::PATH.to_owned(), None)
    }

    /// Create a type-erased action targeting an explicit thread/run base path.
    ///
    /// This is the preferred way to use typed reducers with dynamically chosen
    /// state paths while still avoiding raw patch actions.
    ///
    /// # Panics
    ///
    /// Panics if `S::SCOPE` is `ToolCall`.
    pub fn new_at<S: StateSpec>(path: impl Into<String>, action: S::Action) -> Self {
        assert!(
            S::SCOPE != StateScope::ToolCall,
            "ToolCall-scoped state must use new_for_call() / new_for_call_at(); got new_at() for {}",
            std::any::type_name::<S>(),
        );
        Self::build::<S>(action, S::SCOPE, path.into(), None)
    }

    /// Create a type-erased action targeting a specific tool call scope.
    ///
    /// The `call_id` determines which `__tool_call_scope.<id>` namespace the
    /// action is routed to.
    ///
    /// # Panics
    ///
    /// Panics if `S::PATH` is empty or `S::SCOPE` is not `ToolCall`.
    pub fn new_for_call<S: StateSpec>(action: S::Action, call_id: impl Into<String>) -> Self {
        assert!(
            S::SCOPE == StateScope::ToolCall,
            "new_for_call() requires ToolCall-scoped state; {} has scope {:?}",
            std::any::type_name::<S>(),
            S::SCOPE,
        );
        Self::build::<S>(
            action,
            StateScope::ToolCall,
            S::PATH.to_owned(),
            Some(call_id.into()),
        )
    }

    /// Create a type-erased tool-call-scoped action targeting an explicit path.
    ///
    /// # Panics
    ///
    /// Panics if `S::SCOPE` is not `ToolCall`.
    pub fn new_for_call_at<S: StateSpec>(
        path: impl Into<String>,
        action: S::Action,
        call_id: impl Into<String>,
    ) -> Self {
        assert!(
            S::SCOPE == StateScope::ToolCall,
            "new_for_call_at() requires ToolCall-scoped state; {} has scope {:?}",
            std::any::type_name::<S>(),
            S::SCOPE,
        );
        Self::build::<S>(
            action,
            StateScope::ToolCall,
            path.into(),
            Some(call_id.into()),
        )
    }

    fn build<S: StateSpec>(
        action: S::Action,
        scope: StateScope,
        base_path: String,
        call_id_override: Option<String>,
    ) -> Self {
        let serialized_payload =
            serde_json::to_value(&action).expect("StateSpec::Action must be serializable");

        Self {
            state_type_id: TypeId::of::<S>(),
            state_type_name: std::any::type_name::<S>(),
            scope,
            base_path,
            call_id_override,
            reduce_fn: Self::make_reduce_fn::<S>(action),
            register_lattice: S::register_lattice,
            serialized_payload,
        }
    }

    fn make_reduce_fn<S: StateSpec>(action: S::Action) -> ReduceFn {
        Box::new(move |doc: &Value, actual_path: &str| {
            let path = parse_path(actual_path);
            let sub_doc = get_at_path(doc, &path).cloned().unwrap_or(Value::Null);
            // Track whether the state is being created for the first time.
            // When true, we must emit a whole-state Op::set rather than a
            // per-field diff, because diff_ops would skip fields that match
            // the default (e.g., status=Running when default is Running).
            let is_creation = sub_doc.is_null() || sub_doc == Value::Object(Default::default());

            // When the path doesn't exist (Null) and from_value fails,
            // fall back to an empty object. This handles derive(State) structs
            // whose #[serde(default)] fields can deserialize from `{}` but not
            // from `null` (serde_json rejects null for struct types).
            let mut state = S::from_value(&sub_doc).or_else(|first_err| {
                if sub_doc.is_null() {
                    S::from_value(&Value::Object(Default::default())).map_err(|_| first_err)
                } else {
                    Err(first_err)
                }
            })?;

            if is_creation && S::lattice_keys().is_empty() {
                // First-time creation of non-CRDT state: emit whole-state
                // Op::set so all fields (including those matching defaults)
                // are materialised in the document.
                state.reduce(action);
                let new_value = state.to_value()?;
                let base_path = path_from_str(actual_path);
                return Ok(Patch::with_ops(vec![tirea_state::Op::set(
                    base_path, new_value,
                )]));
            }

            // For CRDT types (or updates to existing state): use diff_ops
            // so lattice fields correctly emit Op::LatticeMerge.
            let old = state.clone();
            state.reduce(action);

            let base_path = path_from_str(actual_path);
            let ops = S::diff_ops(&old, &state, &base_path)?;
            if ops.is_empty() {
                return Ok(Patch::default());
            }
            Ok(Patch::with_ops(ops))
        })
    }

    /// The [`TypeId`] of the state type this action targets.
    pub fn state_type_id(&self) -> TypeId {
        self.state_type_id
    }

    /// Human-readable name of the state type (for diagnostics).
    pub fn state_type_name(&self) -> &str {
        self.state_type_name
    }

    /// Scope of the targeted state.
    pub fn scope(&self) -> StateScope {
        self.scope
    }

    /// Canonical base JSON path for the targeted state.
    pub fn base_path(&self) -> &str {
        &self.base_path
    }

    /// Optional tool-call scope override captured for recovery/internal flows.
    pub fn call_id_override(&self) -> Option<&str> {
        self.call_id_override.as_deref()
    }

    /// The serialized action payload captured before the action is moved into
    /// the reduce closure.
    pub fn serialized_payload(&self) -> &Value {
        &self.serialized_payload
    }
}

/// Reduce a batch of state actions into tracked patches with rolling snapshot semantics.
///
/// Typed actions are reduced against a snapshot that is updated after each
/// action, so sequential actions in one batch compose deterministically.
///
/// `scope_ctx` controls how `ToolCall`-scoped actions are routed to per-call
/// namespaces. For Thread/Run phases (anything outside a tool-call), pass
/// `ScopeContext::run()`.
pub fn reduce_state_actions(
    actions: Vec<AnyStateAction>,
    base_snapshot: &Value,
    default_source: &str,
    scope_ctx: &ScopeContext,
) -> TireaResult<Vec<TrackedPatch>> {
    // Build a local lattice registry from Typed actions so that the rolling
    // snapshot correctly handles Op::LatticeMerge ops (rather than falling back
    // to Op::Set semantics).
    let mut local_registry = LatticeRegistry::new();
    for action in &actions {
        (action.register_lattice)(&mut local_registry);
    }

    let mut rolling_snapshot = base_snapshot.clone();
    let mut tracked_patches = Vec::new();

    for action in actions {
        // Resolve actual storage path: call_id_override takes priority,
        // then fall back to the ambient scope_ctx.
        let actual_path = if let Some(ref cid) = action.call_id_override {
            let override_ctx = ScopeContext::for_call(cid.as_str());
            override_ctx.resolve_path(action.scope, action.base_path.as_str())
        } else {
            scope_ctx.resolve_path(action.scope, action.base_path.as_str())
        };
        let patch = (action.reduce_fn)(&rolling_snapshot, &actual_path)?;
        if patch.is_empty() {
            continue;
        }
        rolling_snapshot = apply_patch_with_registry(&rolling_snapshot, &patch, &local_registry)?;
        tracked_patches.push(TrackedPatch::new(patch).with_source(default_source));
    }

    Ok(tracked_patches)
}

impl fmt::Debug for AnyStateAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyStateAction")
            .field("state", &self.state_type_name)
            .field("type_id", &self.state_type_id)
            .field("scope", &self.scope)
            .field("payload", &self.serialized_payload)
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
    use tirea_state::{
        apply_patch, conflicts_with_registry, DocCell, GCounter, LatticeRegistry, Op, PatchSink,
        Path as TPath, State,
    };

    // -- Manual State + StateSpec impl for testing --

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
    struct Counter {
        value: i64,
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

    #[derive(Debug, Serialize, Deserialize)]
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

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    struct ToolScopedCounter {
        value: i64,
    }

    struct ToolScopedCounterRef;

    impl State for ToolScopedCounter {
        type Ref<'a> = ToolScopedCounterRef;
        const PATH: &'static str = "tool_counter";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            ToolScopedCounterRef
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

    impl StateSpec for ToolScopedCounter {
        type Action = CounterAction;
        const SCOPE: StateScope = StateScope::ToolCall;

        fn reduce(&mut self, action: Self::Action) {
            match action {
                CounterAction::Increment(n) => self.value += n,
                CounterAction::Reset => self.value = 0,
            }
        }
    }

    // -- Tests --

    #[test]
    fn any_state_action_increment() {
        let doc = json!({"counters": {"main": {"value": 5}}});
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(3));
        let patch = reduce_state_actions(vec![action], &doc, "test", &ScopeContext::run()).unwrap();
        let result = apply_patch(&doc, patch[0].patch()).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 8);
    }

    #[test]
    fn any_state_action_reset() {
        let doc = json!({"counters": {"main": {"value": 42}}});
        let action = AnyStateAction::new::<Counter>(CounterAction::Reset);
        let patch = reduce_state_actions(vec![action], &doc, "test", &ScopeContext::run()).unwrap();
        let result = apply_patch(&doc, patch[0].patch()).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 0);
    }

    #[test]
    fn any_state_action_missing_path_defaults() {
        let doc = json!({});
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        let patch = reduce_state_actions(vec![action], &doc, "test", &ScopeContext::run()).unwrap();
        let result = apply_patch(&doc, patch[0].patch()).unwrap();
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
    fn any_state_action_scope_defaults_to_thread() {
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(1));
        assert_eq!(action.scope(), StateScope::Thread);
    }

    #[test]
    fn any_state_action_scope_tool_call() {
        let action = AnyStateAction::new_for_call::<ToolScopedCounter>(
            CounterAction::Increment(1),
            "call_1",
        );
        assert_eq!(action.scope(), StateScope::ToolCall);
    }

    #[test]
    fn reduce_state_actions_uses_rolling_snapshot() {
        let base = json!({"counters": {"main": {"value": 1}}});
        let actions = vec![
            AnyStateAction::new::<Counter>(CounterAction::Increment(1)),
            AnyStateAction::new::<Counter>(CounterAction::Increment(1)),
        ];
        let tracked = reduce_state_actions(actions, &base, "agent", &ScopeContext::run()).unwrap();
        assert_eq!(tracked.len(), 2);

        let mut state = base.clone();
        for patch in tracked {
            state = apply_patch(&state, patch.patch()).unwrap();
        }
        assert_eq!(state["counters"]["main"]["value"], 3);
    }

    #[test]
    fn any_state_action_allows_root_path() {
        let tracked = reduce_state_actions(
            vec![AnyStateAction::new_at::<Counter>(
                "",
                CounterAction::Increment(1),
            )],
            &Value::Null,
            "test",
            &ScopeContext::run(),
        )
        .expect("root path action should reduce");
        assert_eq!(tracked.len(), 1);
        let result = apply_patch(&Value::Null, tracked[0].patch()).expect("patch should apply");
        assert_eq!(result["value"], 1);
    }

    #[test]
    fn reduce_tool_call_scoped_action_routes_to_call_namespace() {
        let base = json!({});
        let actions = vec![AnyStateAction::new_for_call::<ToolScopedCounter>(
            CounterAction::Increment(5),
            "call_42",
        )];
        let tracked = reduce_state_actions(actions, &base, "test", &ScopeContext::run()).unwrap();
        assert_eq!(tracked.len(), 1);

        let result = apply_patch(&base, tracked[0].patch()).unwrap();
        assert_eq!(
            result["__tool_call_scope"]["call_42"]["tool_counter"]["value"],
            5
        );
    }

    #[test]
    fn reduce_run_scoped_action_ignores_call_context() {
        let base = json!({});
        let scope_ctx = ScopeContext::for_call("call_42");
        let actions = vec![AnyStateAction::new::<Counter>(CounterAction::Increment(7))];
        let tracked = reduce_state_actions(actions, &base, "test", &scope_ctx).unwrap();

        let result = apply_patch(&base, tracked[0].patch()).unwrap();
        assert_eq!(result["counters"]["main"]["value"], 7);
        assert!(result.get("__tool_call_scope").is_none());
    }

    #[test]
    fn new_for_call_overrides_scope_ctx() {
        let base = json!({});
        let scope_ctx = ScopeContext::for_call("ambient_call");
        let actions = vec![AnyStateAction::new_for_call::<ToolScopedCounter>(
            CounterAction::Increment(3),
            "override_call",
        )];
        let tracked = reduce_state_actions(actions, &base, "test", &scope_ctx).unwrap();

        let result = apply_patch(&base, tracked[0].patch()).unwrap();
        // Should use the override call_id, not the ambient one
        assert_eq!(
            result["__tool_call_scope"]["override_call"]["tool_counter"]["value"],
            3
        );
        assert!(result["__tool_call_scope"].get("ambient_call").is_none());
    }

    #[test]
    #[should_panic(expected = "requires ToolCall-scoped state")]
    fn new_for_call_panics_on_non_tool_call_scope() {
        let _ = AnyStateAction::new_for_call::<Unbound>((), "call_1");
    }

    // -- CRDT (lattice) field test types --

    #[derive(Debug, Clone, Serialize, Deserialize, Default)]
    struct TokenStats {
        #[serde(default)]
        total_input: GCounter,
        #[serde(default)]
        total_output: GCounter,
        #[serde(default)]
        label: String,
    }

    struct TokenStatsRef;

    impl State for TokenStats {
        type Ref<'a> = TokenStatsRef;
        const PATH: &'static str = "token_stats";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            TokenStatsRef
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

        fn lattice_keys() -> &'static [&'static str] {
            &["total_input", "total_output"]
        }

        fn register_lattice(registry: &mut LatticeRegistry) {
            registry.register::<GCounter>(parse_path("token_stats.total_input"));
            registry.register::<GCounter>(parse_path("token_stats.total_output"));
        }
    }

    #[derive(Serialize, Deserialize)]
    #[allow(dead_code)]
    enum TokenStatsAction {
        AddInput(u64),
        AddOutput(u64),
    }

    impl StateSpec for TokenStats {
        type Action = TokenStatsAction;

        fn reduce(&mut self, action: TokenStatsAction) {
            match action {
                TokenStatsAction::AddInput(n) => self.total_input.increment("_", n),
                TokenStatsAction::AddOutput(n) => self.total_output.increment("_", n),
            }
        }
    }

    #[test]
    fn reducer_emits_op_set_for_crdt_fields_causing_false_conflict() {
        // Two plugins independently record tokens → parallel patches
        let base = json!({});

        let patches_a = reduce_state_actions(
            vec![AnyStateAction::new::<TokenStats>(
                TokenStatsAction::AddInput(100),
            )],
            &base,
            "plugin_a",
            &ScopeContext::run(),
        )
        .unwrap();
        let patches_b = reduce_state_actions(
            vec![AnyStateAction::new::<TokenStats>(
                TokenStatsAction::AddInput(200),
            )],
            &base,
            "plugin_b",
            &ScopeContext::run(),
        )
        .unwrap();

        // Register GCounter at field paths
        let mut registry = LatticeRegistry::new();
        registry.register::<GCounter>(parse_path("token_stats.total_input"));
        registry.register::<GCounter>(parse_path("token_stats.total_output"));

        let conflicts =
            conflicts_with_registry(patches_a[0].patch(), patches_b[0].patch(), &registry);

        // After fix: CRDT fields should use Op::LatticeMerge → no conflict
        assert!(
            conflicts.is_empty(),
            "CRDT fields should not conflict; reducer should emit Op::LatticeMerge for lattice fields"
        );
    }

    #[test]
    fn reducer_emits_lattice_merge_ops_for_crdt_fields() {
        let base = json!({});
        let patches = reduce_state_actions(
            vec![AnyStateAction::new::<TokenStats>(
                TokenStatsAction::AddInput(100),
            )],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();

        let ops = patches[0].patch().ops();
        // Should have per-field ops, not a single whole-state Op::set
        let has_lattice_merge = ops.iter().any(|op| matches!(op, Op::LatticeMerge { .. }));
        assert!(
            has_lattice_merge,
            "reducer should emit Op::LatticeMerge for CRDT fields, got: {ops:?}"
        );
    }

    #[test]
    fn reducer_mixed_fields_emits_correct_op_types() {
        // Custom action that modifies both CRDT and non-CRDT fields
        #[derive(Debug, Clone, Serialize, Deserialize, Default)]
        struct MixedState {
            #[serde(default)]
            counter: GCounter,
            #[serde(default)]
            name: String,
        }

        struct MixedStateRef;

        impl State for MixedState {
            type Ref<'a> = MixedStateRef;
            const PATH: &'static str = "mixed";

            fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
                MixedStateRef
            }

            fn from_value(value: &Value) -> TireaResult<Self> {
                if value.is_null() {
                    return Ok(Self::default());
                }
                serde_json::from_value(value.clone())
                    .map_err(tirea_state::TireaError::Serialization)
            }

            fn to_value(&self) -> TireaResult<Value> {
                serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
            }

            fn lattice_keys() -> &'static [&'static str] {
                &["counter"]
            }
        }

        #[derive(Serialize, Deserialize)]
        enum MixedAction {
            IncrementAndRename(u64, String),
        }

        impl StateSpec for MixedState {
            type Action = MixedAction;

            fn reduce(&mut self, action: MixedAction) {
                match action {
                    MixedAction::IncrementAndRename(n, name) => {
                        self.counter.increment("_", n);
                        self.name = name;
                    }
                }
            }
        }

        let base = json!({});
        let patches = reduce_state_actions(
            vec![AnyStateAction::new::<MixedState>(
                MixedAction::IncrementAndRename(5, "new".to_string()),
            )],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();

        let ops = patches[0].patch().ops();
        let lattice_ops: Vec<_> = ops
            .iter()
            .filter(|op| matches!(op, Op::LatticeMerge { .. }))
            .collect();
        let set_ops: Vec<_> = ops
            .iter()
            .filter(|op| matches!(op, Op::Set { .. }))
            .collect();

        assert!(
            !lattice_ops.is_empty(),
            "should have LatticeMerge for CRDT field 'counter'"
        );
        assert!(
            !set_ops.is_empty(),
            "should have Op::set for non-CRDT field 'name'"
        );
    }

    #[test]
    fn diff_ops_skips_unchanged_fields() {
        // Only modify one CRDT field; the other fields should not appear in ops.
        let base = json!({"token_stats": {"total_input": {}, "total_output": {}, "label": ""}});
        let patches = reduce_state_actions(
            vec![AnyStateAction::new::<TokenStats>(
                TokenStatsAction::AddInput(42),
            )],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();

        let ops = patches[0].patch().ops();
        // Only total_input changed → exactly one op
        assert_eq!(
            ops.len(),
            1,
            "should only emit op for the changed field, got: {ops:?}"
        );
        assert!(
            matches!(&ops[0], Op::LatticeMerge { .. }),
            "changed CRDT field should use LatticeMerge"
        );
    }

    #[test]
    fn diff_ops_empty_when_no_changes() {
        // A reset on an already-zero counter produces no state change.
        let base = json!({"counters": {"main": {"value": 0}}});
        let patches = reduce_state_actions(
            vec![AnyStateAction::new::<Counter>(CounterAction::Reset)],
            &base,
            "test",
            &ScopeContext::run(),
        )
        .unwrap();

        // No change → no patches emitted
        assert!(
            patches.is_empty(),
            "no-op reduce should produce no patches, got: {patches:?}"
        );
    }

    #[test]
    fn serialized_payload_is_captured() {
        let action = AnyStateAction::new::<Counter>(CounterAction::Increment(42));
        let payload = action.serialized_payload();
        assert_eq!(*payload, json!({"Increment": 42}));
    }

    #[test]
    fn serialized_payload_captured_for_call_scoped() {
        let action =
            AnyStateAction::new_for_call::<ToolScopedCounter>(CounterAction::Reset, "call_1");
        let payload = action.serialized_payload();
        assert_eq!(*payload, json!("Reset"));
    }
}
