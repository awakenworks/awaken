//! Execution context types for tools and plugins.
//!
//! `ToolCallContext` provides state access, run policy, and identity for tool execution.
//! It replaces direct `&Thread` usage in tool signatures, keeping the persistent
//! entity (`Thread`) invisible to tools and plugins.

use crate::runtime::activity::ActivityManager;
use crate::runtime::run::RunIdentity;
use crate::runtime::{ToolCallResume, ToolCallState};
use crate::thread::Message;
use crate::RunPolicy;
use futures::future::pending;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tirea_state::{
    get_at_path, parse_path, DocCell, Op, Patch, PatchSink, Path, State, TireaError, TireaResult,
    TrackedPatch,
};
use tokio_util::sync::CancellationToken;

type PatchHook<'a> = Arc<dyn Fn(&Op) -> TireaResult<()> + Send + Sync + 'a>;
const TOOL_PROGRESS_STREAM_PREFIX: &str = "tool_call:";
/// Activity type used for tool-call progress updates.
pub const TOOL_CALL_PROGRESS_ACTIVITY_TYPE: &str = "tool-call-progress";
/// Legacy public alias kept for backward compatibility.
pub const TOOL_PROGRESS_ACTIVITY_TYPE: &str = TOOL_CALL_PROGRESS_ACTIVITY_TYPE;
/// Legacy activity type accepted by consumers.
pub const TOOL_PROGRESS_ACTIVITY_TYPE_LEGACY: &str = "progress";
/// Canonical payload `type` value for tool-call progress events.
pub const TOOL_CALL_PROGRESS_TYPE: &str = "tool-call-progress";
/// Canonical payload schema version for tool-call progress events.
pub const TOOL_CALL_PROGRESS_SCHEMA: &str = "tool-call-progress.v1";

/// Status marker for a tool-call progress node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ToolCallProgressStatus {
    Pending,
    #[default]
    Running,
    Done,
    Failed,
    Cancelled,
}

/// Canonical tree-node payload for tool-call progress updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
pub struct ToolCallProgressState {
    /// Payload type identifier.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Payload schema version.
    pub schema: String,
    /// Stable node id.
    pub node_id: String,
    /// Optional parent node id in the progress tree.
    #[serde(default)]
    pub parent_node_id: Option<String>,
    /// Optional parent tool call id when this node belongs to a nested run.
    #[serde(default)]
    pub parent_call_id: Option<String>,
    /// Tool call id that owns this node.
    pub call_id: String,
    /// Optional tool name.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Current status.
    pub status: ToolCallProgressStatus,
    /// Normalized progress ratio when available.
    #[serde(default)]
    pub progress: Option<f64>,
    /// Optional absolute loaded counter.
    #[serde(default)]
    pub loaded: Option<f64>,
    /// Optional absolute total counter.
    #[serde(default)]
    pub total: Option<f64>,
    /// Optional human-readable message.
    #[serde(default)]
    pub message: Option<String>,
    /// Current run id.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Parent run id.
    #[serde(default)]
    pub parent_run_id: Option<String>,
    /// Current thread id when available.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Last update timestamp in unix milliseconds.
    pub updated_at_ms: u64,
}

/// Input shape for publishing tool-call progress updates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCallProgressUpdate {
    #[serde(default)]
    pub status: ToolCallProgressStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

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

/// Sink interface for tool-call progress events.
///
/// Tools report progress through [`ToolCallContext::report_tool_call_progress`], and
/// the context forwards canonical payloads into this sink. The sink implementation
/// decides how payloads are emitted/transported.
pub trait ToolCallProgressSink: Send + Sync {
    /// Consume a canonical tool-call progress payload.
    fn report(
        &self,
        stream_id: &str,
        activity_type: &str,
        payload: &ToolCallProgressState,
    ) -> TireaResult<()>;
}

#[derive(Clone)]
struct ActivityManagerProgressSink {
    manager: Arc<dyn ActivityManager>,
}

impl ActivityManagerProgressSink {
    fn new(manager: Arc<dyn ActivityManager>) -> Self {
        Self { manager }
    }
}

/// Typed caller metadata exposed to tool executions.
#[derive(Clone, Debug, Default)]
pub struct CallerContext {
    thread_id: Option<String>,
    run_id: Option<String>,
    agent_id: Option<String>,
    messages: Arc<[Arc<Message>]>,
}

impl CallerContext {
    pub fn new(
        thread_id: Option<String>,
        run_id: Option<String>,
        agent_id: Option<String>,
        messages: Vec<Arc<Message>>,
    ) -> Self {
        Self {
            thread_id: thread_id
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            run_id: run_id
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            agent_id: agent_id
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            messages: Arc::<[Arc<Message>]>::from(messages),
        }
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    pub fn messages(&self) -> &[Arc<Message>] {
        self.messages.as_ref()
    }
}

impl ToolCallProgressSink for ActivityManagerProgressSink {
    fn report(
        &self,
        stream_id: &str,
        activity_type: &str,
        payload: &ToolCallProgressState,
    ) -> TireaResult<()> {
        let Value::Object(fields) = serde_json::to_value(payload)? else {
            return Err(TireaError::invalid_operation(
                "tool-call-progress payload must serialize as object",
            ));
        };
        for (key, value) in fields {
            let op = Op::set(Path::root().key(key), value);
            self.manager.on_activity_op(stream_id, activity_type, &op);
        }
        Ok(())
    }
}

/// Execution context for tool invocations.
///
/// Provides typed state access (read/write), run policy access, identity,
/// message queuing, and activity tracking. Tools receive `&ToolCallContext`
/// instead of `&Thread`.
pub struct ToolCallContext<'a> {
    doc: &'a DocCell,
    ops: &'a Mutex<Vec<Op>>,
    call_id: String,
    source: String,
    run_policy: &'a RunPolicy,
    run_identity: RunIdentity,
    caller_context: CallerContext,
    pending_messages: &'a Mutex<Vec<Arc<Message>>>,
    activity_manager: Arc<dyn ActivityManager>,
    tool_call_progress_sink: Arc<dyn ToolCallProgressSink>,
    cancellation_token: Option<&'a CancellationToken>,
    read_only: bool,
}

impl<'a> ToolCallContext<'a> {
    fn tool_call_state_path(call_id: &str) -> Path {
        Path::root()
            .key("__tool_call_scope")
            .key(call_id)
            .key("tool_call_state")
    }

    fn apply_op(&self, op: Op) -> TireaResult<()> {
        if self.read_only {
            return Err(TireaError::invalid_operation(
                "tool context is read-only; emit ToolExecutionEffect actions instead",
            ));
        }
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
        run_policy: &'a RunPolicy,
        pending_messages: &'a Mutex<Vec<Arc<Message>>>,
        activity_manager: Arc<dyn ActivityManager>,
    ) -> Self {
        let tool_call_progress_sink: Arc<dyn ToolCallProgressSink> =
            Arc::new(ActivityManagerProgressSink::new(activity_manager.clone()));
        Self {
            doc,
            ops,
            call_id: call_id.into(),
            source: source.into(),
            run_policy,
            run_identity: RunIdentity::default(),
            caller_context: CallerContext::default(),
            pending_messages,
            activity_manager,
            tool_call_progress_sink,
            cancellation_token: None,
            read_only: false,
        }
    }

    /// Mark this context as read-only; state writes via `apply_op` are rejected.
    #[must_use]
    pub fn as_read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Attach cancellation token.
    #[must_use]
    pub fn with_cancellation_token(mut self, token: &'a CancellationToken) -> Self {
        self.cancellation_token = Some(token);
        self
    }

    #[must_use]
    pub fn with_run_identity(mut self, run_identity: RunIdentity) -> Self {
        self.run_identity = run_identity;
        self
    }

    #[must_use]
    pub fn with_caller_context(mut self, caller_context: CallerContext) -> Self {
        self.caller_context = caller_context;
        self
    }

    /// Override the sink used for tool-call progress payload forwarding.
    ///
    /// This allows runtime integrations to decouple progress collection from
    /// activity transport details.
    #[must_use]
    pub fn with_tool_call_progress_sink(mut self, sink: Arc<dyn ToolCallProgressSink>) -> Self {
        self.tool_call_progress_sink = sink;
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
    // Run policy / identity
    // =========================================================================

    /// Borrow the run policy.
    pub fn run_policy(&self) -> &RunPolicy {
        self.run_policy
    }

    pub fn run_identity(&self) -> &RunIdentity {
        &self.run_identity
    }

    pub fn caller_context(&self) -> &CallerContext {
        &self.caller_context
    }

    // =========================================================================
    // State access
    // =========================================================================

    /// Typed state reference at path.
    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        let base = parse_path(path);
        let doc = self.doc;
        let read_only = self.read_only;
        let hook: PatchHook<'_> = Arc::new(move |op: &Op| {
            if read_only {
                return Err(TireaError::invalid_operation(
                    "tool context is read-only; emit ToolExecutionEffect actions instead",
                ));
            }
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
        let val = self.doc.snapshot();
        let path = Self::tool_call_state_path(call_id);
        let at = get_at_path(&val, &path);
        match at {
            Some(v) if !v.is_null() => {
                let state = ToolCallState::from_value(v)?;
                Ok(Some(state))
            }
            _ => Ok(None),
        }
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

    fn source_tool_name(&self) -> Option<String> {
        self.source
            .strip_prefix("tool:")
            .filter(|name| !name.trim().is_empty())
            .map(ToOwned::to_owned)
    }

    fn validate_progress_value(name: &str, value: Option<f64>) -> TireaResult<()> {
        let Some(value) = value else {
            return Ok(());
        };
        if !value.is_finite() {
            return Err(TireaError::invalid_operation(format!(
                "{name} must be a finite number"
            )));
        }
        if value < 0.0 {
            return Err(TireaError::invalid_operation(format!(
                "{name} must be non-negative"
            )));
        }
        Ok(())
    }

    /// Publish a typed tool-call progress node update.
    ///
    /// The update is written to `activity(progress_stream_id(), "tool-call-progress")`
    /// with payload schema `tool-call-progress.v1`.
    pub fn report_tool_call_progress(&self, update: ToolCallProgressUpdate) -> TireaResult<()> {
        Self::validate_progress_value("progress value", update.progress)?;
        Self::validate_progress_value("progress loaded", update.loaded)?;
        Self::validate_progress_value("progress total", update.total)?;

        let run_id = self.run_identity.run_id_opt().map(ToOwned::to_owned);
        let parent_run_id = self.run_identity.parent_run_id_opt().map(ToOwned::to_owned);
        let thread_id = self.caller_context.thread_id().map(ToOwned::to_owned);
        let parent_call_id = self.run_identity.parent_tool_call_id_opt().and_then(|id| {
            if id == self.call_id {
                None
            } else {
                Some(id.to_string())
            }
        });
        let parent_node_id = parent_call_id
            .as_ref()
            .map(|id| format!("{TOOL_PROGRESS_STREAM_PREFIX}{id}"))
            .or_else(|| run_id.as_ref().map(|id| format!("run:{id}")));
        let stream_id = self.progress_stream_id();
        let payload = ToolCallProgressState {
            event_type: TOOL_CALL_PROGRESS_TYPE.to_string(),
            schema: TOOL_CALL_PROGRESS_SCHEMA.to_string(),
            node_id: stream_id.clone(),
            parent_node_id,
            parent_call_id,
            call_id: self.call_id.clone(),
            tool_name: self.source_tool_name(),
            status: update.status,
            progress: update.progress,
            loaded: update.loaded,
            total: update.total,
            message: update.message,
            run_id,
            parent_run_id,
            thread_id,
            updated_at_ms: current_unix_millis(),
        };

        self.tool_call_progress_sink
            .report(&stream_id, TOOL_CALL_PROGRESS_ACTIVITY_TYPE, &payload)
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

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
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
    use crate::testing::TestFixtureState;
    use serde_json::json;
    use std::sync::Arc;
    use tirea_state::apply_patch;
    use tokio::time::{timeout, Duration};
    use tokio_util::sync::CancellationToken;

    fn make_ctx<'a>(
        doc: &'a DocCell,
        ops: &'a Mutex<Vec<Op>>,
        run_policy: &'a RunPolicy,
        pending: &'a Mutex<Vec<Arc<Message>>>,
    ) -> ToolCallContext<'a> {
        ToolCallContext::new(
            doc,
            ops,
            "call-1",
            "test",
            run_policy,
            pending,
            NoOpActivityManager::arc(),
        )
    }

    fn run_identity(run_id: &str) -> RunIdentity {
        RunIdentity::new(
            "thread-child".to_string(),
            None,
            run_id.to_string(),
            None,
            "agent".to_string(),
            crate::storage::RunOrigin::Internal,
        )
    }

    fn caller_context(thread_id: &str) -> CallerContext {
        CallerContext::new(
            Some(thread_id.to_string()),
            Some("run-parent".to_string()),
            Some("caller".to_string()),
            vec![Arc::new(Message::user("seed"))],
        )
    }

    #[test]
    fn test_identity() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);
        assert_eq!(ctx.call_id(), "call-1");
        assert_eq!(ctx.idempotency_key(), "call-1");
        assert_eq!(ctx.source(), "test");
    }

    #[test]
    fn test_typed_context_access() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::new();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending)
            .with_run_identity(run_identity("run-1").with_parent_tool_call_id("call-parent"))
            .with_caller_context(caller_context("thread-1"));

        assert_eq!(
            ctx.run_identity().parent_tool_call_id_opt(),
            Some("call-parent")
        );
        assert_eq!(ctx.run_identity().run_id_opt(), Some("run-1"));
        assert_eq!(ctx.caller_context().thread_id(), Some("thread-1"));
        assert_eq!(ctx.caller_context().agent_id(), Some("caller"));
        assert_eq!(ctx.caller_context().messages().len(), 1);
    }

    #[test]
    fn test_state_of_read_write() {
        let doc = DocCell::new(json!({"__test_fixture": {"label": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        // Write
        let ctrl = ctx.state_of::<TestFixtureState>();
        ctrl.set_label(Some("rate_limit".into()))
            .expect("failed to set label");

        // Read back from same ref
        let val = ctrl.label().unwrap();
        assert!(val.is_some());
        assert_eq!(val.unwrap(), "rate_limit");

        // Ops captured in thread ops
        assert!(!ops.lock().unwrap().is_empty());
    }

    #[test]
    fn test_write_through_read_cross_ref() {
        let doc = DocCell::new(json!({"__test_fixture": {"label": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        // Write via first ref
        ctx.state_of::<TestFixtureState>()
            .set_label(Some("timeout".into()))
            .expect("failed to set label");

        // Read via second ref
        let val = ctx.state_of::<TestFixtureState>().label().unwrap();
        assert_eq!(val.unwrap(), "timeout");
    }

    #[test]
    fn test_take_patch() {
        let doc = DocCell::new(json!({"__test_fixture": {"label": null}}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        ctx.state_of::<TestFixtureState>()
            .set_label(Some("test".into()))
            .expect("failed to set label");

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
        let scope = RunPolicy::default();
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
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());

        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        let ctrl = ctx.call_state::<TestFixtureState>();
        ctrl.set_label(Some("call_scoped".into()))
            .expect("failed to set label");

        assert!(ctx.has_changes());
    }

    #[test]
    fn test_tool_call_state_roundtrip_and_resume_input() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
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
        let scope = RunPolicy::default();
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
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        assert!(!ctx.is_cancelled());
        assert!(ctx.cancellation_token().is_none());
    }

    #[tokio::test]
    async fn test_cancelled_waits_for_attached_token() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
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
        let scope = RunPolicy::default();
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

    fn rebuild_activity_state(events: &[(String, String, Op)]) -> Value {
        let mut value = json!({});
        for (_, _, op) in events {
            value = apply_patch(&value, &Patch::with_ops(vec![op.clone()]))
                .expect("activity op should apply");
        }
        value
    }

    #[derive(Default)]
    struct RecordingProgressSink {
        events: Mutex<Vec<(String, String, ToolCallProgressState)>>,
    }

    impl ToolCallProgressSink for RecordingProgressSink {
        fn report(
            &self,
            stream_id: &str,
            activity_type: &str,
            payload: &ToolCallProgressState,
        ) -> TireaResult<()> {
            self.events.lock().unwrap().push((
                stream_id.to_string(),
                activity_type.to_string(),
                payload.clone(),
            ));
            Ok(())
        }
    }

    struct FailingProgressSink;

    impl ToolCallProgressSink for FailingProgressSink {
        fn report(
            &self,
            _stream_id: &str,
            _activity_type: &str,
            _payload: &ToolCallProgressState,
        ) -> TireaResult<()> {
            Err(TireaError::invalid_operation("sink failed"))
        }
    }

    #[test]
    fn test_report_tool_call_progress_emits_tool_call_progress_activity() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
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

        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Running,
            progress: Some(0.5),
            loaded: None,
            total: Some(10.0),
            message: Some("half way".to_string()),
        })
        .expect("progress should be emitted");

        let events = activity_manager.events.lock().unwrap();
        assert!(!events.is_empty());
        assert!(events.iter().all(|(stream_id, activity_type, _)| {
            stream_id == "tool_call:call-1" && activity_type == TOOL_CALL_PROGRESS_ACTIVITY_TYPE
        }));
        let state = rebuild_activity_state(&events);
        assert_eq!(state["type"], TOOL_CALL_PROGRESS_TYPE);
        assert_eq!(state["schema"], TOOL_CALL_PROGRESS_SCHEMA);
        assert_eq!(state["node_id"], "tool_call:call-1");
        assert_eq!(state["call_id"], "call-1");
        assert_eq!(state["status"], "running");
        assert_eq!(state["progress"], json!(0.5));
        assert_eq!(state["total"], json!(10.0));
        assert_eq!(state["message"], json!("half way"));
    }

    #[test]
    fn test_report_tool_call_progress_rejects_non_finite_values() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());
        let ctx = make_ctx(&doc, &ops, &scope, &pending);

        assert!(ctx
            .report_tool_call_progress(ToolCallProgressUpdate {
                status: ToolCallProgressStatus::Running,
                progress: Some(f64::NAN),
                loaded: None,
                total: None,
                message: None,
            })
            .is_err());
        assert!(ctx
            .report_tool_call_progress(ToolCallProgressUpdate {
                status: ToolCallProgressStatus::Running,
                progress: Some(0.5),
                loaded: None,
                total: Some(f64::INFINITY),
                message: None,
            })
            .is_err());
        assert!(ctx
            .report_tool_call_progress(ToolCallProgressUpdate {
                status: ToolCallProgressStatus::Running,
                progress: Some(0.5),
                loaded: Some(-1.0),
                total: None,
                message: None,
            })
            .is_err());
    }

    #[test]
    fn test_report_tool_call_progress_writes_lineage_and_metadata() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::new();
        let pending = Mutex::new(Vec::new());
        let activity_manager = Arc::new(RecordingActivityManager::default());
        let run_identity = RunIdentity::new(
            "thread-abc".to_string(),
            None,
            "run-123".to_string(),
            Some("run-parent".to_string()),
            "agent".to_string(),
            crate::storage::RunOrigin::Internal,
        )
        .with_parent_tool_call_id("call-parent");
        let caller_context = CallerContext::new(
            Some("thread-abc".to_string()),
            Some("run-parent".to_string()),
            Some("caller".to_string()),
            vec![],
        );

        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "tool:echo",
            &scope,
            &pending,
            activity_manager.clone(),
        )
        .with_run_identity(run_identity)
        .with_caller_context(caller_context);

        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Done,
            progress: Some(1.0),
            loaded: Some(5.0),
            total: Some(5.0),
            message: Some("done".to_string()),
        })
        .expect("tool call progress should be emitted");

        let events = activity_manager.events.lock().unwrap();
        let state = rebuild_activity_state(&events);
        assert_eq!(state["type"], TOOL_CALL_PROGRESS_TYPE);
        assert_eq!(state["schema"], TOOL_CALL_PROGRESS_SCHEMA);
        assert_eq!(state["node_id"], "tool_call:call-1");
        assert_eq!(state["parent_node_id"], "tool_call:call-parent");
        assert_eq!(state["parent_call_id"], "call-parent");
        assert_eq!(state["tool_name"], "echo");
        assert_eq!(state["status"], "done");
        assert_eq!(state["run_id"], "run-123");
        assert_eq!(state["parent_run_id"], "run-parent");
        assert_eq!(state["thread_id"], "thread-abc");
        assert!(state["updated_at_ms"].as_u64().unwrap_or_default() > 0);
    }

    #[test]
    fn test_report_tool_call_progress_without_parent_tool_call_anchors_to_run_node() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::new();
        let pending = Mutex::new(Vec::new());
        let activity_manager = Arc::new(RecordingActivityManager::default());
        let run_identity = run_identity("run-123");
        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "tool:echo",
            &scope,
            &pending,
            activity_manager.clone(),
        )
        .with_run_identity(run_identity);

        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Running,
            progress: Some(0.3),
            loaded: None,
            total: None,
            message: Some("working".to_string()),
        })
        .expect("tool call progress should be emitted");

        let events = activity_manager.events.lock().unwrap();
        let state = rebuild_activity_state(&events);
        assert_eq!(state["parent_node_id"], "run:run-123");
        assert!(state["parent_call_id"].is_null());
    }

    #[test]
    fn test_report_tool_call_progress_uses_injected_sink_instead_of_activity_manager() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());
        let activity_manager = Arc::new(RecordingActivityManager::default());
        let sink = Arc::new(RecordingProgressSink::default());
        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "tool:echo",
            &scope,
            &pending,
            activity_manager.clone(),
        )
        .with_tool_call_progress_sink(sink.clone());

        ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Running,
            progress: Some(0.2),
            loaded: None,
            total: Some(10.0),
            message: Some("working".to_string()),
        })
        .expect("tool call progress should be reported");

        let sink_events = sink.events.lock().unwrap();
        assert_eq!(sink_events.len(), 1);
        let (stream_id, activity_type, payload) = &sink_events[0];
        assert_eq!(stream_id, "tool_call:call-1");
        assert_eq!(activity_type, TOOL_CALL_PROGRESS_ACTIVITY_TYPE);
        assert_eq!(payload.call_id, "call-1");
        assert_eq!(payload.progress, Some(0.2));

        let activity_events = activity_manager.events.lock().unwrap();
        assert!(
            activity_events.is_empty(),
            "injected sink should bypass default activity manager sink"
        );
    }

    #[test]
    fn test_report_tool_call_progress_propagates_sink_error() {
        let doc = DocCell::new(json!({}));
        let ops = Mutex::new(Vec::new());
        let scope = RunPolicy::default();
        let pending = Mutex::new(Vec::new());
        let ctx = ToolCallContext::new(
            &doc,
            &ops,
            "call-1",
            "tool:echo",
            &scope,
            &pending,
            NoOpActivityManager::arc(),
        )
        .with_tool_call_progress_sink(Arc::new(FailingProgressSink));

        let result = ctx.report_tool_call_progress(ToolCallProgressUpdate {
            status: ToolCallProgressStatus::Running,
            progress: Some(0.1),
            loaded: None,
            total: None,
            message: None,
        });
        assert!(result.is_err());
    }
}
