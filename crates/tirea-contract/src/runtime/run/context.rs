use crate::runtime::activity::ActivityManager;
use crate::runtime::run::delta::RunDelta;
use crate::runtime::state::SerializedAction;
use crate::runtime::tool_call::ToolCallContext;
use crate::runtime::tool_call::SuspendedCall;
use crate::runtime::suspended_calls_from_state;
use crate::thread::Message;
use crate::RunConfig;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tirea_state::{
    apply_patches_with_registry, get_at_path, parse_path, DeltaTracked, DocCell, LatticeRegistry,
    Op, State, TireaResult, TrackedPatch,
};

/// Run-scoped workspace that holds mutable state for a single agent run.
///
/// `RunContext` is constructed from a `Thread`'s persisted data at the start
/// of a run and accumulates messages, patches, and overlay ops as the run
/// progresses. It owns the `DocCell` (live document) and provides delta
/// extraction via `take_delta()`.
///
/// It does **not** hold the `Thread` itself — only the data needed for
/// execution. The thread identity is carried as `thread_id`.
pub struct RunContext {
    thread_id: String,
    thread_base: Value,
    messages: DeltaTracked<Arc<Message>>,
    thread_patches: DeltaTracked<TrackedPatch>,
    serialized_actions: DeltaTracked<SerializedAction>,
    pub run_config: RunConfig,
    doc: DocCell,
    version: Option<u64>,
    version_timestamp: Option<u64>,
    lattice_registry: Arc<LatticeRegistry>,
}

impl RunContext {
    /// Build a run workspace from thread data.
    ///
    /// - `thread_id`: thread identifier (owned)
    /// - `state`: already-rebuilt state (base + patches)
    /// - `messages`: initial messages (cursor set to end — no delta)
    /// - `run_config`: per-run sealed configuration
    pub fn new(
        thread_id: impl Into<String>,
        state: Value,
        messages: Vec<Arc<Message>>,
        run_config: RunConfig,
    ) -> Self {
        Self::with_registry(
            thread_id,
            state,
            messages,
            run_config,
            Arc::new(LatticeRegistry::new()),
        )
    }

    /// Build a run workspace with a pre-populated lattice registry.
    pub fn with_registry(
        thread_id: impl Into<String>,
        state: Value,
        messages: Vec<Arc<Message>>,
        run_config: RunConfig,
        lattice_registry: Arc<LatticeRegistry>,
    ) -> Self {
        let doc = DocCell::new(state.clone());
        Self {
            thread_id: thread_id.into(),
            thread_base: state,
            messages: DeltaTracked::new(messages),
            thread_patches: DeltaTracked::empty(),
            serialized_actions: DeltaTracked::empty(),
            run_config,
            doc,
            version: None,
            version_timestamp: None,
            lattice_registry,
        }
    }

    // =========================================================================
    // Identity
    // =========================================================================

    /// Thread identifier.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    // =========================================================================
    // Version
    // =========================================================================

    /// Current committed version (0 if never committed).
    pub fn version(&self) -> u64 {
        self.version.unwrap_or(0)
    }

    /// Update version after a successful state commit.
    pub fn set_version(&mut self, version: u64, timestamp: Option<u64>) {
        self.version = Some(version);
        if let Some(ts) = timestamp {
            self.version_timestamp = Some(ts);
        }
    }

    /// Timestamp of the last committed version.
    pub fn version_timestamp(&self) -> Option<u64> {
        self.version_timestamp
    }

    // =========================================================================
    // Suspended calls
    // =========================================================================

    /// Read all suspended calls from durable control state.
    pub fn suspended_calls(&self) -> std::collections::HashMap<String, SuspendedCall> {
        self.snapshot()
            .map(|s| suspended_calls_from_state(&s))
            .unwrap_or_default()
    }

    // =========================================================================
    // Messages
    // =========================================================================

    /// All messages (initial + accumulated during run).
    pub fn messages(&self) -> &[Arc<Message>] {
        self.messages.as_slice()
    }

    /// Add a single message to the run.
    pub fn add_message(&mut self, msg: Arc<Message>) {
        self.messages.push(msg);
    }

    /// Add multiple messages to the run.
    pub fn add_messages(&mut self, msgs: Vec<Arc<Message>>) {
        self.messages.extend(msgs);
    }

    // =========================================================================
    // State / Patches
    // =========================================================================

    /// The initial rebuilt state (base + thread patches).
    pub fn thread_base(&self) -> &Value {
        &self.thread_base
    }

    /// Add a tracked patch from this run.
    pub fn add_thread_patch(&mut self, patch: TrackedPatch) {
        self.thread_patches.push(patch);
    }

    /// Add multiple tracked patches from this run.
    pub fn add_thread_patches(&mut self, patches: Vec<TrackedPatch>) {
        self.thread_patches.extend(patches);
    }

    /// All patches accumulated during this run.
    pub fn thread_patches(&self) -> &[TrackedPatch] {
        self.thread_patches.as_slice()
    }

    // =========================================================================
    // Serialized Actions (intent log)
    // =========================================================================

    /// Add serialized actions captured during tool/phase execution.
    pub fn add_serialized_actions(&mut self, actions: Vec<SerializedAction>) {
        self.serialized_actions.extend(actions);
    }

    // =========================================================================
    // Doc (live document)
    // =========================================================================

    /// Rebuild the current run-visible state (thread_base + thread_patches).
    ///
    /// This is a pure computation that returns a new `Value` without
    /// touching the `DocCell`.
    pub fn snapshot(&self) -> TireaResult<Value> {
        let patches = self.thread_patches.as_slice();
        if patches.is_empty() {
            Ok(self.thread_base.clone())
        } else {
            apply_patches_with_registry(
                &self.thread_base,
                patches.iter().map(|p| p.patch()),
                &self.lattice_registry,
            )
        }
    }

    /// Typed snapshot at the type's canonical path.
    ///
    /// Rebuilds state and deserializes the value at `T::PATH`.
    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        let val = self.snapshot()?;
        let at = get_at_path(&val, &parse_path(T::PATH)).unwrap_or(&Value::Null);
        T::from_value(at)
    }

    /// Typed snapshot at an explicit path.
    ///
    /// Rebuilds state and deserializes the value at the given path.
    pub fn snapshot_at<T: State>(&self, path: &str) -> TireaResult<T> {
        let val = self.snapshot()?;
        let at = get_at_path(&val, &parse_path(path)).unwrap_or(&Value::Null);
        T::from_value(at)
    }

    // =========================================================================
    // Delta output
    // =========================================================================

    /// Extract the incremental delta (new messages + patches + actions) since
    /// the last `take_delta()` call.
    pub fn take_delta(&mut self) -> RunDelta {
        RunDelta {
            messages: self.messages.take_delta(),
            patches: self.thread_patches.take_delta(),
            actions: self.serialized_actions.take_delta(),
        }
    }

    /// Whether there are un-consumed messages, patches, or actions.
    pub fn has_delta(&self) -> bool {
        self.messages.has_delta()
            || self.thread_patches.has_delta()
            || self.serialized_actions.has_delta()
    }

    // =========================================================================
    // ToolCallContext derivation
    // =========================================================================

    /// Create a `ToolCallContext` scoped to a specific tool call.
    pub fn tool_call_context<'ctx>(
        &'ctx self,
        ops: &'ctx Mutex<Vec<Op>>,
        call_id: impl Into<String>,
        source: impl Into<String>,
        pending_messages: &'ctx Mutex<Vec<Arc<Message>>>,
        activity_manager: Arc<dyn ActivityManager>,
    ) -> ToolCallContext<'ctx> {
        ToolCallContext::new(
            &self.doc,
            ops,
            call_id,
            source,
            &self.run_config,
            pending_messages,
            activity_manager,
        )
    }
}

impl RunContext {
    /// Convenience constructor from a `Thread`.
    ///
    /// Rebuilds state from the thread's base state + patches, then wraps
    /// the thread's messages and the given `run_config` into a `RunContext`.
    /// Version metadata is carried over from thread metadata.
    pub fn from_thread(
        thread: &crate::thread::Thread,
        run_config: RunConfig,
    ) -> Result<Self, tirea_state::TireaError> {
        Self::from_thread_with_registry(
            thread,
            run_config,
            Arc::new(LatticeRegistry::new()),
        )
    }

    /// Convenience constructor from a `Thread` with a lattice registry.
    pub fn from_thread_with_registry(
        thread: &crate::thread::Thread,
        run_config: RunConfig,
        lattice_registry: Arc<LatticeRegistry>,
    ) -> Result<Self, tirea_state::TireaError> {
        let state = thread.rebuild_state()?;
        let messages: Vec<Arc<Message>> = thread.messages.clone();
        let mut ctx =
            Self::with_registry(thread.id.clone(), state, messages, run_config, lattice_registry);
        if let Some(v) = thread.metadata.version {
            ctx.set_version(v, thread.metadata.version_timestamp);
        }
        Ok(ctx)
    }

    /// The lattice registry used by this context for CRDT-aware operations.
    pub fn lattice_registry(&self) -> &Arc<LatticeRegistry> {
        &self.lattice_registry
    }
}

impl std::fmt::Debug for RunContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunContext")
            .field("thread_id", &self.thread_id)
            .field("messages", &self.messages.len())
            .field("thread_patches", &self.thread_patches.len())
            .field("has_delta", &self.has_delta())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_state::{path, Patch};

    #[test]
    fn new_context_has_no_delta() {
        let msgs = vec![Arc::new(Message::user("hi"))];
        let mut ctx = RunContext::new("t-1", json!({}), msgs, RunConfig::default());
        assert!(!ctx.has_delta());
        let delta = ctx.take_delta();
        assert!(delta.is_empty());
        assert_eq!(ctx.messages().len(), 1);
    }

    #[test]
    fn add_message_creates_delta() {
        let mut ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());
        ctx.add_message(Arc::new(Message::user("hello")));
        ctx.add_message(Arc::new(Message::assistant("hi")));
        assert!(ctx.has_delta());
        let delta = ctx.take_delta();
        assert_eq!(delta.messages.len(), 2);
        assert!(delta.patches.is_empty());
        assert!(!ctx.has_delta());
        assert_eq!(ctx.messages().len(), 2);
    }

    #[test]
    fn add_patch_creates_delta() {
        let mut ctx = RunContext::new("t-1", json!({"a": 1}), vec![], RunConfig::default());
        let patch = TrackedPatch::new(Patch::new().with_op(Op::set(path!("a"), json!(2))));
        ctx.add_thread_patch(patch);
        assert!(ctx.has_delta());
        let delta = ctx.take_delta();
        assert_eq!(delta.patches.len(), 1);
        assert!(!ctx.has_delta());
    }

    #[test]
    fn multiple_deltas() {
        let mut ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());
        ctx.add_message(Arc::new(Message::user("a")));
        let d1 = ctx.take_delta();
        assert_eq!(d1.messages.len(), 1);

        ctx.add_message(Arc::new(Message::user("b")));
        ctx.add_message(Arc::new(Message::user("c")));
        let d2 = ctx.take_delta();
        assert_eq!(d2.messages.len(), 2);

        let d3 = ctx.take_delta();
        assert!(d3.is_empty());
    }

    // =========================================================================
    // Category 1: Delta extraction incremental semantics
    // =========================================================================

    /// Initial messages passed to `new()` are NOT part of the delta.
    /// Only run-added messages appear in `take_delta()`.
    #[test]
    fn initial_messages_excluded_from_delta() {
        let initial = vec![
            Arc::new(Message::user("pre-existing-1")),
            Arc::new(Message::assistant("pre-existing-2")),
        ];
        let mut ctx = RunContext::new("t-1", json!({}), initial, RunConfig::default());

        // No delta despite having 2 messages
        assert!(!ctx.has_delta());
        let delta = ctx.take_delta();
        assert!(delta.messages.is_empty());
        assert_eq!(ctx.messages().len(), 2);

        // Now add a run message — only that one appears
        ctx.add_message(Arc::new(Message::user("run-added")));
        let delta = ctx.take_delta();
        assert_eq!(delta.messages.len(), 1);
        assert_eq!(delta.messages[0].content, "run-added");
        // Total messages still include initial
        assert_eq!(ctx.messages().len(), 3);
    }

    /// All patches are delta (cursor starts at 0) — every patch added during
    /// a run is considered new.
    #[test]
    fn all_patches_are_delta() {
        let mut ctx = RunContext::new("t-1", json!({"a": 0}), vec![], RunConfig::default());
        ctx.add_thread_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("a"), json!(1))),
        ));
        ctx.add_thread_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("a"), json!(2))),
        ));
        let delta = ctx.take_delta();
        assert_eq!(delta.patches.len(), 2, "all run patches should be in delta");
    }

    /// Multiple take_delta calls produce non-overlapping results.
    #[test]
    fn consecutive_take_delta_non_overlapping() {
        let mut ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());

        // Round 1: 1 message + 1 patch
        ctx.add_message(Arc::new(Message::user("m1")));
        ctx.add_thread_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("x"), json!(1))),
        ));
        let d1 = ctx.take_delta();
        assert_eq!(d1.messages.len(), 1);
        assert_eq!(d1.patches.len(), 1);

        // Round 2: 2 messages + 1 patch (no overlap with d1)
        ctx.add_message(Arc::new(Message::user("m2")));
        ctx.add_message(Arc::new(Message::user("m3")));
        ctx.add_thread_patch(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("y"), json!(2))),
        ));
        let d2 = ctx.take_delta();
        assert_eq!(d2.messages.len(), 2);
        assert_eq!(d2.patches.len(), 1);

        // Round 3: nothing added
        let d3 = ctx.take_delta();
        assert!(d3.is_empty());

        // Total accumulated
        assert_eq!(ctx.messages().len(), 3);
        assert_eq!(ctx.thread_patches().len(), 2);
    }

    // =========================================================================
    // Category 6: Typed snapshot (snapshot_of / snapshot_at)
    // =========================================================================

    #[test]
    fn snapshot_of_deserializes_at_canonical_path() {
        use crate::testing::TestFixtureState;

        let ctx = RunContext::new(
            "t-1",
            json!({"__test_fixture": {"label": null}}),
            vec![],
            RunConfig::default(),
        );
        let ctrl: TestFixtureState = ctx.snapshot_of().unwrap();
        assert!(ctrl.label.is_none());
    }

    #[test]
    fn snapshot_at_deserializes_at_explicit_path() {
        use crate::testing::TestFixtureState;

        let ctx = RunContext::new(
            "t-1",
            json!({"custom": {"label": null}}),
            vec![],
            RunConfig::default(),
        );
        let ctrl: TestFixtureState = ctx.snapshot_at("custom").unwrap();
        assert!(ctrl.label.is_none());
    }

    #[test]
    fn snapshot_of_returns_error_for_missing_path() {
        use crate::testing::TestFixtureState;

        let ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());
        assert!(ctx.snapshot_of::<TestFixtureState>().is_err());
    }

    // =========================================================================
    // Category 5: from_thread boundary conditions
    // =========================================================================

    #[test]
    fn from_thread_rebuilds_existing_patches() {
        use crate::thread::Thread;

        let mut thread = Thread::with_initial_state("t-1", json!({"counter": 0}));
        thread.patches.push(TrackedPatch::new(
            Patch::new().with_op(Op::set(path!("counter"), json!(5))),
        ));

        let ctx = RunContext::from_thread(&thread, RunConfig::default()).unwrap();
        // thread_base is pre-rebuilt (includes thread patches)
        assert_eq!(ctx.thread_base()["counter"], 5);
        // No run patches yet
        assert!(ctx.thread_patches().is_empty());
        // snapshot() is consistent with thread_base()
        assert_eq!(ctx.snapshot().unwrap()["counter"], 5);
    }

    #[test]
    fn from_thread_carries_version_metadata() {
        use crate::thread::Thread;

        let mut thread = Thread::new("t-1");
        thread.metadata.version = Some(42);
        thread.metadata.version_timestamp = Some(1700000000);

        let ctx = RunContext::from_thread(&thread, RunConfig::default()).unwrap();
        assert_eq!(ctx.version(), 42);
        assert_eq!(ctx.version_timestamp(), Some(1700000000));
    }

    #[test]
    fn from_thread_broken_patch_returns_error() {
        use crate::thread::Thread;

        let mut thread = Thread::with_initial_state("t-1", json!({"x": 1}));
        // Append to a non-array path — this will fail during rebuild_state
        thread.patches.push(TrackedPatch::new(Patch::with_ops(vec![
            tirea_state::Op::Append {
                path: path!("x"),
                value: json!(999),
            },
        ])));

        let result = RunContext::from_thread(&thread, RunConfig::default());
        assert!(
            result.is_err(),
            "broken patch should cause from_thread to fail"
        );
    }

    // =========================================================================
    // Version tracking
    // =========================================================================

    #[test]
    fn version_defaults_to_zero() {
        let ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());
        assert_eq!(ctx.version(), 0);
        assert_eq!(ctx.version_timestamp(), None);
    }

    #[test]
    fn set_version_updates_correctly() {
        let mut ctx = RunContext::new("t-1", json!({}), vec![], RunConfig::default());
        ctx.set_version(5, Some(1700000000));
        assert_eq!(ctx.version(), 5);
        assert_eq!(ctx.version_timestamp(), Some(1700000000));

        // Update again
        ctx.set_version(6, None);
        assert_eq!(ctx.version(), 6);
        // Timestamp unchanged when None passed
        assert_eq!(ctx.version_timestamp(), Some(1700000000));
    }
}
