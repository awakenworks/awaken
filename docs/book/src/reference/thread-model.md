# Thread Model

Threads represent persistent conversations. A thread owns metadata and a small
run projection; messages, runs, and dispatch attempts are stored separately.

The durable model is:

```text
Thread 1 -> * MessageRecord
Thread 1 -> * RunRecord
RunRecord 1 -> * RunDispatch

RunRecord reads MessageRecord by range or explicit ids.
RunRecord produces MessageRecord through checkpointed assistant/tool output.
```

## Thread

```rust,ignore
pub struct Thread {
    /// Unique thread identifier (UUID v7).
    pub id: String,
    /// External resource or tenant grouping for this thread.
    pub resource_id: Option<String>,
    /// Parent thread id when this thread was created by a sub-agent run.
    pub parent_thread_id: Option<String>,
    /// Thread metadata (timestamps, title, custom data).
    pub metadata: ThreadMetadata,
    /// Run currently executing on a worker for this thread.
    pub active_run_id: Option<String>,
    /// Current unfinished user intent for this thread.
    pub open_run_id: Option<String>,
    /// Most recently known run for this thread.
    pub latest_run_id: Option<String>,
}
```

**Crate path:** `awaken::contract::thread::Thread` (re-exported from `awaken-contract`)

`parent_thread_id` is normalized on assignment: leading/trailing whitespace is
trimmed and empty strings deserialize to `None`. The same trimming is applied
to `resource_id`. Thread hierarchy participates in the run lifecycle: when a
sub-agent run begins, `RunRequestSnapshot.parent_thread_id` carries the parent
thread, and the checkpoint projection populates `Thread.parent_thread_id` on
the child thread the first time it is materialized.

### Constructors

```rust,ignore
/// Create with a generated UUID v7 identifier.
fn new() -> Self

/// Create with a specific identifier.
fn with_id(id: impl Into<String>) -> Self
```

### Builder methods

```rust,ignore
fn with_title(self, title: impl Into<String>) -> Self
fn with_resource_id(self, resource_id: impl Into<String>) -> Self
fn with_parent_thread_id(self, parent_thread_id: impl Into<String>) -> Self
```

`Thread` implements `Default` (delegates to `Thread::new()`), `Clone`,
`Serialize`, and `Deserialize`.

## ThreadMetadata

```rust,ignore
pub struct ThreadMetadata {
    /// Creation timestamp (unix millis).
    pub created_at: Option<u64>,
    /// Last update timestamp (unix millis).
    pub updated_at: Option<u64>,
    /// Optional thread title.
    pub title: Option<String>,
    /// Custom metadata key-value pairs.
    pub custom: HashMap<String, Value>,
}
```

All `Option` fields are omitted from JSON when `None`. The `custom` map is
omitted when empty.

`ThreadMetadata` implements `Default`, `Clone`, `Serialize`, and `Deserialize`.

## Storage

Messages are **not** stored inside the `Thread` struct. They are managed through
the `ThreadStore` trait:

```rust,ignore
#[async_trait]
pub trait ThreadStore: Send + Sync {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;
    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError>;
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError>;
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;
    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError>;
    async fn list_child_threads(&self, parent_thread_id: &str) -> Result<Vec<Thread>, StorageError>;
    async fn validate_thread_hierarchy(
        &self,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> Result<(), StorageError>;
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;
    async fn load_message_records(&self, thread_id: &str) -> Result<Option<Vec<MessageRecord>>, StorageError>;
    async fn save_messages(&self, thread_id: &str, messages: &[Message]) -> Result<(), StorageError>;
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn update_thread_metadata(&self, id: &str, metadata: ThreadMetadata) -> Result<(), StorageError>;
}
```

The default helpers on `ThreadStore` cover first-class lineage filtering,
parent existence / cycle validation, and child-thread delete strategies
without requiring every backend to reimplement that logic.

```rust,ignore
pub enum ChildThreadDeleteStrategy {
    /// Reject deletion when at least one direct child exists.
    Reject,
    /// Preserve child threads and clear their `parent_thread_id`. Default.
    Detach,
    /// Recursively delete all descendants before deleting the target thread.
    Cascade,
}
```

The default `delete_thread_with_strategy` implementation issues multiple
low-level writes and is **not atomic** across child updates and the final
delete. Production backends with concurrent writers should override the
method with a transactional or otherwise fenced implementation. The file,
PostgreSQL, and NATS-buffered backends ship with backend-native overrides.

The default `list_threads_query` walks `list_threads` in fixed-size chunks
and filters in memory; the file, PostgreSQL, and NATS-buffered backends each
override it with a backend-native pushdown. Cursors returned by
`ThreadQuery::encode_cursor` are validated against the original query shape
on decode, so a paged sequence cannot drift onto a different filter.

`Message` is the protocol payload sent to agents and protocol adapters.
`MessageRecord` is the durable thread-log projection:

```rust,ignore
pub struct MessageRecord {
    pub message_id: String,
    pub thread_id: String,
    pub seq: u64,
    pub message: Message,
    pub produced_by_run_id: Option<String>,
    pub step_index: Option<u32>,
    pub tool_call_id: Option<String>,
    pub created_at: Option<u64>,
}
```

The default `load_message_records` implementation derives records from
`load_messages`, assigning 1-based `seq` values in append order and projecting
producer metadata from each `Message`.

User and system messages normally have `produced_by_run_id = None`. Assistant,
tool, and internal messages produced by a run should set `produced_by_run_id`
through `Message.metadata.run_id`. This lets a child or sub-agent result be
found from the child run's produced message range. The result is the final
non-tool assistant message in that range.

## ThreadRunStore

Extends `ThreadStore` + `RunStore` with an atomic checkpoint operation.

```rust,ignore
#[async_trait]
pub trait ThreadRunStore: ThreadStore + RunStore + Send + Sync {
    /// Persist thread messages and run record atomically.
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;
}
```

## RunStore

Run record persistence for tracking run history and enabling resume.

```rust,ignore
#[async_trait]
pub trait RunStore: Send + Sync {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError>;
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError>;
}
```

## RunRecord

```rust,ignore
pub struct RunRecord {
    pub run_id: String,
    pub thread_id: String,
    pub agent_id: String,
    pub parent_run_id: Option<String>,
    pub request: Option<RunRequestSnapshot>,
    pub input: Option<RunMessageInput>,
    pub output: Option<RunMessageOutput>,
    pub status: RunStatus,
    pub termination_reason: Option<TerminationReason>,
    pub final_output: Option<String>,
    pub error_payload: Option<Value>,
    pub dispatch_id: Option<String>,
    pub session_id: Option<String>,
    pub transport_request_id: Option<String>,
    pub waiting: Option<RunWaitingState>,
    pub outcome: Option<RunOutcome>,
    pub created_at: u64,        // unix seconds
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub updated_at: u64,        // unix seconds
    pub steps: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub state: Option<PersistedState>,
}
```

`RunRecord` is the source of truth for one user intent. It stores request
metadata, lifecycle state, waiting state, output/error outcome, and trace IDs.
It does not own message bodies.

### RunRequestSnapshot

`RunRequestSnapshot` preserves the request that created or resumed the run:

```rust,ignore
pub struct RunRequestSnapshot {
    pub origin: RunRequestOrigin,
    pub sender_id: Option<String>,
    pub input_message_ids: Vec<String>,
    pub input_message_count: u64,
    pub request_extras: Option<Value>,
    pub decisions: Vec<RunResumeDecision>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub parent_thread_id: Option<String>,
    pub transport_request_id: Option<String>,
}
```

`input_message_ids` plus `input_message_count` point at thread-owned message
records. Request snapshots do not own message bodies.

### RunMessageInput and RunMessageOutput

`RunMessageInput` describes the thread-log range or explicit message selection
read by a run. `RunMessageOutput` describes messages produced by the run. Both
types reference thread-owned message records:

```rust,ignore
pub struct RunMessageInput {
    pub thread_id: String,
    pub range: Option<MessageSeqRange>,
    pub trigger_message_ids: Vec<String>,
    pub selected_message_ids: Vec<String>,
    pub context_policy: Option<String>,
    pub compacted_snapshot_id: Option<String>,
}
```

## RunDispatch

`RunDispatch` owns delivery, lease, retry, and queue-audit state. It is not the
source of truth for agent success.

```text
Queued -> Claimed -> Acked | Cancelled | Superseded | DeadLetter
```

`Acked` means the dispatch was consumed and will not be retried. To decide
whether the agent succeeded, read `RunRecord.status`, `RunRecord.waiting`, and
`RunRecord.outcome`.

## Related

- [Use File Store](../how-to/use-file-store.md)
- [Use Postgres Store](../how-to/use-postgres-store.md)
- ADR-0022: Run Dispatch Data Model
