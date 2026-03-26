# Thread Model

Threads represent persistent conversations. A thread holds metadata only;
messages are stored and loaded separately through the `ThreadStore` trait.

## Thread

```rust,ignore
pub struct Thread {
    /// Unique thread identifier (UUID v7).
    pub id: String,
    /// Thread metadata.
    pub metadata: ThreadMetadata,
}
```

**Crate path:** `awaken::contract::thread::Thread` (re-exported from `awaken-contract`)

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
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;
    async fn save_messages(&self, thread_id: &str, messages: &[Message]) -> Result<(), StorageError>;
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError>;
    async fn update_thread_metadata(&self, id: &str, metadata: ThreadMetadata) -> Result<(), StorageError>;
}
```

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
    pub status: RunStatus,
    pub termination_code: Option<String>,
    pub created_at: u64,        // unix seconds
    pub updated_at: u64,        // unix seconds
    pub steps: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub state: Option<PersistedState>,
}
```

## Related

- [Use File Store](../how-to/use-file-store.md)
- [Use Postgres Store](../how-to/use-postgres-store.md)
