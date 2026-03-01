//! Execution context types for tools and plugins.
//!
//! `ToolCallContext` provides state access, run config, and identity for tool execution.
//! It replaces direct `&Thread` usage in tool signatures, keeping the persistent
//! entity (`Thread`) invisible to tools and plugins.

use crate::runtime::activity::ActivityManager;
use crate::runtime::{ToolCallResume, ToolCallState, ToolCallStatesMap};
use crate::thread::Message;
use crate::RunConfig;
use futures::future::pending;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tirea_state::{
    get_at_path, parse_path, DocCell, Op, Patch, PatchSink, Path, State, TireaError, TireaResult,
    TrackedPatch,
};
use tokio_util::sync::CancellationToken;

type PatchHook<'a> = Arc<dyn Fn(&Op) -> TireaResult<()> + Send + Sync + 'a>;
const TOOL_PROGRESS_STREAM_PREFIX: &str = "tool_call:";
/// Default activity type used for tool progress updates.
pub const TOOL_PROGRESS_ACTIVITY_TYPE: &str = "progress";

/// Canonical activity state shape for tool progress updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
pub struct ToolProgressState {
    /// Normalized progress value.
    pub progress: f64,
    /// Optional absolute total if the source has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Optional human-readable progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Execution context for tool invocations.
///
/// Provides typed state access (read/write), run config access, identity,
/// message queuing, and activity tracking. Tools receive `&ToolCallContext`
/// instead of `&Thread`.
pub struct ToolCallContext<'a> {
    doc: &'a DocCell,
    ops: &'a Mutex<Vec<Op>>,
    call_id: String,
    source: String,
    run_config: &'a RunConfig,
    pending_messages: &'a Mutex<Vec<Arc<Message>>>,
    activity_manager: Arc<dyn ActivityManager>,
    cancellation_token: Option<&'a CancellationToken>,
}

impl<'a> ToolCallContext<'a> {
    fn tool_call_state_path(call_id: &str) -> Path {
        Path::root()
            .key(ToolCallStatesMap::PATH)
            .key("calls")
            .key(call_id)
    }

    fn apply_op(&self, op: Op) -> TireaResult<()> {
        self.doc.apply(&op)?;
        self.ops.lock().unwrap().push(op);
        Ok(())
    }

    /// Create a new tool call context.
    pub fn new(
        doc: &'a DocCell,
        ops: &'a Mutex<Vec<Op>>,
        call_id: impl Into<String>,
        source: impl Into<String>,
        run_config: &'a RunConfig,
        pending_messages: &'a Mutex<Vec<Arc<Message>>>,
        activity_manager: Arc<dyn ActivityManager>,
    ) -> Self {
        Self {
            doc,
            ops,
            call_id: call_id.into(),
            source: source.into(),
            run_config,
            pending_messages,
            activity_manager,
            cancellation_token: None,
        }
    }

    /// Attach cancellation token.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: &'a CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    // =========================================================================
    // Identity
    // =========================================================================

    /// Borrow the underlying document cell.
    pub fn doc(&self) -> &DocCell {
        self.doc
    }

    /// Current call id (typically the `tool_call_id`).
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// Stable idempotency key for the current tool invocation.
    ///
    /// Tools should use this value when implementing idempotent side effects.
    pub fn idempotency_key(&self) -> &str {
        self.call_id()
    }

    /// Source identifier used for tracked patches.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the run cancellation token has already been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation_token
            .is_some_and(CancellationToken::is_cancelled)
    }

    /// Await cancellation for this context.
    ///
    /// If no cancellation token is available, this future never resolves.
    pub async fn cancelled(&self) {
        if let Some(token) = self.cancellation_token {
            token.cancelled().await;
        } else {
            pending::<()>().await;
        }
    }

    /// Borrow the cancellation token when present.
    pub fn cancellation_token(&self) -> Option<&CancellationToken> {
        self.cancellation_token
    }

    // =========================================================================
    // Run Config
    // =========================================================================

    /// Borrow the run config.
    pub fn run_config(&self) -> &RunConfig {
        self.run_config
    }

    /// Typed run config accessor.
    pub fn config_state<T: State>(&self) -> TireaResult<T::Ref<'_>> {
        Ok(self.run_config.get::<T>())
    }

    /// Read a run config value by key.
    pub fn config_value(&self, key: &str) -> Option<&Value> {
        self.run_config.value(key)
    }

    // =========================================================================
    // State access
    // =========================================================================

    /// Typed state reference at path.
    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        let base = parse_path(path);
        let doc = self.doc;
        let hook: PatchHook<'_> = Arc::new(|op: &Op| {
            doc.apply(op)?;
            Ok(())
        });
        T::state_ref(doc, base, PatchSink::new_with_hook(self.ops, hook))
    }

    /// Typed state reference at the type's canonical path.
    ///
    /// Panics if `T::PATH` is empty (no bound path via `#[tirea(path = "...")]`).
    pub fn state_of<T: State>(&self) -> T::Ref<'_> {
        assert!(
            !T::PATH.is_empty(),
            "State type has no bound path; use state::<T>(path) instead"
        );
        self.state::<T>(T::PATH)
    }

    /// Typed state reference for current call (`tool_calls.<call_id>`).
    pub fn call_state<T: State>(&self) -> T::Ref<'_> {
        let path = format!("tool_calls.{}", self.call_id);
        self.state::<T>(&path)
    }

    /// Read persisted runtime state for a specific tool call.
    pub fn tool_call_state_for(&self, call_id: &str) -> TireaResult<Option<ToolCallState>> {
        if call_id.trim().is_empty() {
            return Ok(None);
        }
        let runtime = self.state_of::<ToolCallStatesMap>();
        let calls = runtime.calls()?;
        Ok(calls.get(call_id).cloned())
    }

    /// Read persisted runtime state for current `call_id`.
    pub fn tool_call_state(&self) -> TireaResult<Option<ToolCallState>> {
        self.tool_call_state_for(self.call_id())
    }

    /// Upsert persisted runtime state for a specific tool call.
    pub fn set_tool_call_state_for(&self, call_id: &str, state: ToolCallState) -> TireaResult<()> {
        if call_id.trim().is_empty() {
            return Err(TireaError::invalid_operation(
                "tool_call_state requires non-empty call_id",
            ));
        }
        let value = serde_json::to_value(state)?;
        self.apply_op(Op::set(Self::tool_call_state_path(call_id), value))
    }

    /// Upsert persisted runtime state for current `call_id`.
    pub fn set_tool_call_state(&self, state: ToolCallState) -> TireaResult<()> {
        self.set_tool_call_state_for(self.call_id(), state)
    }

    /// Remove persisted runtime state for a specific tool call.
    pub fn clear_tool_call_state_for(&self, call_id: &str) -> TireaResult<()> {
        if call_id.trim().is_empty() {
            return Ok(());
        }
        if self.tool_call_state_for(call_id)?.is_some() {
            self.apply_op(Op::delete(Self::tool_call_state_path(call_id)))?;
        }
        Ok(())
    }

    /// Remove persisted runtime state for current `call_id`.
    pub fn clear_tool_call_state(&self) -> TireaResult<()> {
        self.clear_tool_call_state_for(self.call_id())
    }

    /// Read resume payload for a specific tool call.
    pub fn resume_input_for(&self, call_id: &str) -> TireaResult<Option<ToolCallResume>> {
        Ok(self
            .tool_call_state_for(call_id)?
            .and_then(|state| state.resume))
    }

    /// Read resume payload for current `call_id`.
    pub fn resume_input(&self) -> TireaResult<Option<ToolCallResume>> {
        self.resume_input_for(self.call_id())
    }

    // =========================================================================
    // Messages
    // =========================================================================

    /// Queue a message addition in this operation.
    pub fn add_message(&self, message: Message) {
        self.pending_messages
            .lock()
            .unwrap()
            .push(Arc::new(message));
    }

    /// Queue multiple messages in this operation.
    pub fn add_messages(&self, messages: impl IntoIterator<Item = Message>) {
        self.pending_messages
            .lock()
            .unwrap()
            .extend(messages.into_iter().map(Arc::new));
    }

    // =========================================================================
    // Activity
    // =========================================================================

    /// Create an activity context for a stream/type pair.
    pub fn activity(
        &self,
        stream_id: impl Into<String>,
        activity_type: impl Into<String>,
    ) -> ActivityContext {
        let stream_id = stream_id.into();
        let activity_type = activity_type.into();
        let snapshot = self.activity_manager.snapshot(&stream_id);

        ActivityContext::new(
            snapshot,
            stream_id,
            activity_type,
            self.activity_manager.clone(),
        )
    }

    /// Stable stream id used by default for this tool call's progress activity.
    pub fn progress_stream_id(&self) -> String {
        format!("{TOOL_PROGRESS_STREAM_PREFIX}{}", self.call_id)
    }

    /// Publish a progress update for this tool call.
    ///
    /// The update is written to `activity(progress_stream_id(), "progress")`.
    pub fn report_progress(
        &self,
        progress: f64,
        total: Option<f64>,
        message: Option<String>,
    ) -> TireaResult<()> {
        if !progress.is_finite() {
            return Err(TireaError::invalid_operation(
                "progress value must be a finite number",
            ));
        }
        if let Some(total) = total {
            if !total.is_finite() {
                return Err(TireaError::invalid_operation(
                    "progress total must be a finite number",
                ));
            }
            if total < 0.0 {
                return Err(TireaError::invalid_operation(
                    "progress total must be non-negative",
                ));
            }
        }

        let activity = self.activity(self.progress_stream_id(), TOOL_PROGRESS_ACTIVITY_TYPE);
        let state = activity.state::<ToolProgressState>("");
        state.set_progress(progress)?;
        state.set_total(total)?;
        state.set_message(message)?;
        Ok(())
    }

    // =========================================================================
    // State snapshot
    // =========================================================================

    /// Snapshot the current document state.
    ///
    /// Returns the current state including all write-through updates.
    /// Equivalent to `Thread::rebuild_state()` in transient contexts.
    pub fn snapshot(&self) -> Value {
        self.doc.snapshot()
    }

    /// Typed snapshot at the type's canonical path.
    ///
    /// Reads current doc state and deserializes the value at `T::PATH`.
    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        let val = self.doc.snapshot();
        let at = get_at_path(&val, &parse_path(T::PATH)).unwrap_or(&Value::Null);
        T::from_value(at)
    }

    /// Typed snapshot at an explicit path.
    ///
    /// Reads current doc state and deserializes the value at the given path.
    pub fn snapshot_at<T: State>(&self, path: &str) -> TireaResult<T> {
        let val = self.doc.snapshot();
        let at = get_at_path(&val, &parse_path(path)).unwrap_or(&Value::Null);
        T::from_value(at)
    }

    // =========================================================================
    // Patch extraction
    // =========================================================================

    /// Extract accumulated patch with context source metadata.
    pub fn take_patch(&self) -> TrackedPatch {
        let ops = std::mem::take(&mut *self.ops.lock().unwrap());
        TrackedPatch::new(Patch::with_ops(ops)).with_source(self.source.clone())
    }

    /// Whether state has pending transient changes.
    pub fn has_changes(&self) -> bool {
        !self.ops.lock().unwrap().is_empty()
    }

    /// Number of queued transient operations.
    pub fn ops_count(&self) -> usize {
        self.ops.lock().unwrap().len()
    }
}

/// Activity-scoped state context.
pub struct ActivityContext {
    doc: DocCell,
    stream_id: String,
    activity_type: String,
    ops: Mutex<Vec<Op>>,
    manager: Arc<dyn ActivityManager>,
}

impl ActivityContext {
    pub(crate) fn new(
        doc: Value,
        stream_id: String,
        activity_type: String,
        manager: Arc<dyn ActivityManager>,
    ) -> Self {
        Self {
            doc: DocCell::new(doc),
            stream_id,
            activity_type,
            ops: Mutex::new(Vec::new()),
            manager,
        }
    }

    /// Typed activity state reference at the type's canonical path.
    ///
    /// Panics if `T::PATH` is empty.
    pub fn state_of<T: State>(&self) -> T::Ref<'_> {
        assert!(
            !T::PATH.is_empty(),
            "State type has no bound path; use state::<T>(path) instead"
        );
        self.state::<T>(T::PATH)
    }

    /// Get a typed activity state reference at the specified path.
    ///
    /// All modifications are automatically collected and immediately reported
    /// to the activity manager. Writes are applied to the shared doc for
    /// immediate read-back.
    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        let base = parse_path(path);
        let manager = self.manager.clone();
        let stream_id = self.stream_id.clone();
        let activity_type = self.activity_type.clone();
        let doc = &self.doc;
        let hook: PatchHook<'_> = Arc::new(move |op: &Op| {
            doc.apply(op)?;
            manager.on_activity_op(&stream_id, &activity_type, op);
            Ok(())
        });
        T::state_ref(&self.doc, base, PatchSink::new_with_hook(&self.ops, hook))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ResumeDecisionAction;
    use crate::runtime::activity::{ActivityManager, NoOpActivityManager};
    use crate::runtime::run::InferenceErrorState;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::time::{timeout, Duration};
    use tokio_util::sync::CancellationToken;

    fn make_ctx<'a>(
        doc: &'a DocCell,
        ops: &'a Mutex<Vec<Op>>,
        run_config: &'a RunConfig,
        pending: &'a Mutex<Vec<Arc<Message>>>,
    ) -> ToolCallContext<'a> {
        ToolCallContext::new(
            doc,
            ops,
            "call-1",
            "test",
            run_config,
            pending,
            NoOpActivityManager::arc(),
        )
    }

    #[test]
    fn test_identity() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);
        assert_eq!(ctx.call_id(), "call-1");
        assert_eq!(ctx.idempotency_key(), "call-1");
        assert_eq!(ctx.source(), "test");
    }

    #[test]
    fn test_scope_access() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let mut scope = RunConfig::new();
        scope.set("user_id", "u1").unwrap();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);
        assert_eq!(ctx.config_value("user_id"), Some(&json!("u1")));
        assert_eq!(ctx.config_value("missing"), None);
    }

    #[test]
    fn test_state_of_read_write() {
        let doc = DocCell::new(json!({"__inference_error": {"error": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        // Write
        let ctrl = ctx.state_of::<InferenceErrorState>();
        ctrl.set_error(Some(crate::runtime::run::InferenceError {
            error_type: "rate_limit".into(),
            message: "too many requests".into(),
        }))
        .expect("failed to set inference_error");

        // Read back from same ref
        let err = ctrl.error().unwrap();
        assert!(err.is_some());
        assert_eq!(err.unwrap().error_type, "rate_limit");

        // Ops captured in thread ops
        assert!(!ops.lock().unwrap().is_empty());
    }

    #[test]
    fn test_write_through_read_cross_ref() {
        let doc = DocCell::new(json!({"__inference_error": {"error": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        // Write via first ref
        ctx.state_of::<InferenceErrorState>()
            .set_error(Some(crate::runtime::run::InferenceError {
                error_type: "timeout".into(),
                message: "timed out".into(),
            }))
            .expect("failed to set inference_error");

        // Read via second ref
        let err = ctx.state_of::<InferenceErrorState>().error().unwrap();
        assert_eq!(err.unwrap().error_type, "timeout");
    }

    #[test]
    fn test_take_patch() {
        let doc = DocCell::new(json!({"__inference_error": {"error": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        ctx.state_of::<InferenceErrorState>()
            .set_error(Some(crate::runtime::run::InferenceError {
                error_type: "test".into(),
                message: "test".into(),
            }))
            .expect("failed to set inference_error");

        assert!(ctx.has_changes());
        assert!(ctx.ops_count() > 0);

        let patch = ctx.take_patch();
        assert!(!patch.patch().is_empty());
        assert_eq!(patch.source.as_deref(), Some("test"));
        assert!(!ctx.has_changes());
        assert_eq!(ctx.ops_count(), 0);
    }

    #[test]
    fn test_add_messages() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        ctx.add_message(Message::user("hello"));
        ctx.add_messages(vec![Message::assistant("hi"), Message::user("bye")]);

        assert_eq!(pending.lock().unwrap().len(), 3);
    }

    #[test]
    fn test_call_state() {
        let doc = DocCell::new(json!({"tool_calls": {}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        let ctrl = ctx.call_state::<InferenceErrorState>();
        ctrl.set_error(Some(crate::runtime::run::InferenceError {
            error_type: "call_scoped".into(),
            message: "test".into(),
        }))
        .expect("failed to set inference_error");

        assert!(ctx.has_changes());
    }

    #[test]
    fn test_tool_call_state_roundtrip_and_resume_input() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        let state = ToolCallState {
            call_id: "call.1".to_string(),
            tool_name: "confirm".to_string(),
            arguments: json!({"value": 1}),
            status: crate::runtime::ToolCallStatus::Resuming,
            resume_token: Some("resume.1".to_string()),
            resume: Some(crate::runtime::ToolCallResume {
                decision_id: "decision_1".to_string(),
                action: ResumeDecisionAction::Resume,
                result: json!({"approved": true}),
                reason: None,
                updated_at: 123,
            }),
            scratch: json!({"k": "v"}),
            updated_at: 124,
        };

        ctx.set_tool_call_state_for("call.1", state.clone())
            .expect("state should be persisted");

        let loaded = ctx
            .tool_call_state_for("call.1")
            .expect("state read should succeed");
        assert_eq!(loaded, Some(state.clone()));

        let resume = ctx
            .resume_input_for("call.1")
            .expect("resume read should succeed");
        assert_eq!(resume, state.resume);
    }

    #[test]
    fn test_clear_tool_call_state_for_removes_entry() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        ctx.set_tool_call_state_for(
            "call-1",
            ToolCallState {
                call_id: "call-1".to_string(),
                tool_name: "echo".to_string(),
                arguments: json!({"x": 1}),
                status: crate::runtime::ToolCallStatus::Running,
                resume_token: None,
                resume: None,
                scratch: Value::Null,
                updated_at: 1,
            },
        )
        .expect("state should be set");

        ctx.clear_tool_call_state_for("call-1")
            .expect("clear should succeed");
        assert_eq!(
            ctx.tool_call_state_for("call-1")
                .expect("state read should succeed"),
            None
        );
    }

    #[test]
    fn test_cancellation_token_absent_by_default() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        assert!(!ctx.is_cancelled());
        assert!(ctx.cancellation_token().is_none());
    }

    #[tokio::test]
    async fn test_cancelled_waits_for_attached_token() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let token = CancellationToken::new();

        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "test",
            &scope,
            &pending,
            NoOpActivityManager::arc(),
        )
        .with_cancellation_token(&token);

        let token_for_task = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token_for_task.cancel();
        });

        timeout(Duration::from_millis(300), ctx.cancelled())
            .await
            .expect("cancelled() should resolve after token cancellation");
    }

    #[tokio::test]
    async fn test_cancelled_without_token_never_resolves() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        let timed_out = timeout(Duration::from_millis(30), ctx.cancelled())
            .await
            .is_err();
        assert!(timed_out, "cancelled() without token should remain pending");
    }

    #[derive(Default)]
    struct RecordingActivityManager {
        events: Mutex<Vec<(String, String, Op)>>,
    }

    impl ActivityManager for RecordingActivityManager {
        fn snapshot(&self, _stream_id: &str) -> Value {
            json!({})
        }

        fn on_activity_op(&self, stream_id: &str, activity_type: &str, op: &Op) {
            self.events.lock().unwrap().push((
                stream_id.to_string(),
                activity_type.to_string(),
                op.clone(),
            ));
        }
    }

    #[test]
    fn test_report_progress_emits_progress_activity() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let activity_manager = Arc::new(RecordingActivityManager::default());

        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "test",
            &scope,
            &pending,
            activity_manager.clone(),
        );

        ctx.report_progress(0.5, Some(10.0), Some("half way".to_string()))
            .expect("progress should be emitted");

        let events = activity_manager.events.lock().unwrap();
        assert!(!events.is_empty());
        assert!(events.iter().all(|(stream_id, activity_type, _)| {
            stream_id == "tool_call:call-1" && activity_type == "progress"
        }));
        assert!(
            events
                .iter()
                .any(|(_, _, op)| op.path().to_string() == "$.progress"),
            "progress op should be emitted"
        );
    }

    #[test]
    fn test_report_progress_rejects_non_finite_values() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunConfig::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        assert!(ctx.report_progress(f64::NAN, None, None).is_err());
        assert!(ctx.report_progress(0.5, Some(f64::INFINITY), None).is_err());
        assert!(ctx.report_progress(0.5, Some(-1.0), None).is_err());
    }
}
