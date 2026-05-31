//! Server/store thread-run storage: the full CRUD query/page/pagination
//! vocabulary, store-trait re-exports, and scoped wrappers.
//!
//! The run-record data model and the runtime checkpoint read port stay in
//! runtime-contract (the engine consumes them) and are re-exported below by
//! name. The query/page/filter/pagination types — used only by the
//! `ThreadStore`/`RunStore` CRUD surface — are defined here.

// Run-record data + checkpoint read port shared with the runtime; defined in
// runtime-contract.
pub use awaken_runtime_contract::contract::storage::{
    CheckpointSnapshot, MessageSeqRange, RunMessageInput, RunMessageOutput, RunOutcome, RunRecord,
    RunRequestOrigin, RunRequestSnapshot, RunResumeDecision, RunWaitingState, RunWaitingTicket,
    RuntimeCheckpointStore, StorageError, WaitingReason, message_append,
};
// Thread/run store traits + checkpoint adapter (server/store concern).
pub use super::store_traits::{RunStore, ThreadRunCheckpointStore, ThreadRunStore, ThreadStore};

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::contract::lifecycle::RunStatus;
use awaken_runtime_contract::contract::message::{Message, MessageRecord, Visibility};
use awaken_runtime_contract::thread::{Thread, ThreadMetadata, normalize_lineage_id};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

const MESSAGE_CURSOR_PREFIX: &str = "msg_";
const THREAD_CURSOR_PREFIX: &str = "thr_";

// ── query types ─────────────────────────────────────────────────────

/// Pagination/filter query for listing messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageQuery {
    /// Number of items to skip.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Return records with sequence numbers greater than this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// Return records with sequence numbers less than this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Sort order for message sequence numbers.
    #[serde(default)]
    pub order: MessageOrder,
    /// Visibility filter applied before pagination.
    #[serde(default)]
    pub visibility: MessageVisibilityFilter,
    /// Filter by producing run ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            after: None,
            before: None,
            order: MessageOrder::Asc,
            visibility: MessageVisibilityFilter::Any,
            run_id: None,
        }
    }
}

impl MessageQuery {
    /// Return a copy with contract-level pagination limits applied.
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.min(200),
            after: self.after,
            before: self.before,
            order: self.order,
            visibility: self.visibility,
            run_id: self.run_id.clone(),
        }
    }

    /// Encode an opaque cursor for continuing this exact query.
    #[must_use]
    pub fn encode_cursor(&self, offset: usize) -> String {
        let normalized = self.normalized();
        encode_cursor_token(
            MESSAGE_CURSOR_PREFIX,
            &MessageCursorToken {
                offset,
                after: normalized.after,
                before: normalized.before,
                order: normalized.order,
                visibility: normalized.visibility,
                run_id: normalized.run_id,
            },
        )
    }

    /// Decode a cursor and verify it belongs to this exact query shape.
    pub fn decode_cursor(&self, cursor: &str) -> Result<usize, String> {
        if let Ok(offset) = cursor.parse::<usize>() {
            return Ok(offset);
        }

        let normalized = self.normalized();
        let token: MessageCursorToken = decode_cursor_token(MESSAGE_CURSOR_PREFIX, cursor)?;
        if token.after != normalized.after
            || token.before != normalized.before
            || token.order != normalized.order
            || token.visibility != normalized.visibility
            || token.run_id != normalized.run_id
        {
            return Err("cursor does not match message query filters".to_string());
        }
        Ok(token.offset)
    }

    /// Return true when a record passes the query filters.
    #[must_use]
    pub fn matches_record(&self, record: &MessageRecord) -> bool {
        if self.after.is_some_and(|after| record.seq <= after) {
            return false;
        }
        if self.before.is_some_and(|before| record.seq >= before) {
            return false;
        }
        if self
            .run_id
            .as_deref()
            .is_some_and(|run_id| record.produced_by_run_id.as_deref() != Some(run_id))
        {
            return false;
        }
        match self.visibility {
            MessageVisibilityFilter::Any => true,
            MessageVisibilityFilter::External => record.message.visibility != Visibility::Internal,
            MessageVisibilityFilter::Internal => record.message.visibility == Visibility::Internal,
        }
    }
}

/// Message sequence ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrder {
    /// Oldest message first.
    #[default]
    Asc,
    /// Newest message first.
    Desc,
}

/// Message visibility filter for storage queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageVisibilityFilter {
    /// Include all stored messages.
    #[default]
    Any,
    /// Include externally visible messages.
    External,
    /// Include internal-only messages.
    Internal,
}

/// Paginated message record response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePage {
    pub records: Vec<MessageRecord>,
    pub total: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<String>,
}

impl MessagePage {
    /// Empty page for a missing thread or message log.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
            total: 0,
            has_more: false,
            next_cursor: None,
            prev_cursor: None,
        }
    }
}

/// Parent/root lineage filter for thread queries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadParentFilter {
    /// Do not filter by parent linkage.
    #[default]
    Any,
    /// Restrict results to root threads with no parent.
    Root,
    /// Restrict results to direct children of the specified parent thread.
    Parent(String),
}

impl ThreadParentFilter {
    #[must_use]
    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    #[must_use]
    pub fn normalized(&self) -> Self {
        match self {
            Self::Any => Self::Any,
            Self::Root => Self::Root,
            Self::Parent(parent_thread_id) => normalize_lineage_id(Some(parent_thread_id))
                .map(Self::Parent)
                .unwrap_or(Self::Any),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MessageCursorToken {
    offset: usize,
    after: Option<u64>,
    before: Option<u64>,
    order: MessageOrder,
    visibility: MessageVisibilityFilter,
    run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ThreadCursorToken {
    offset: usize,
    resource_id: Option<String>,
    parent_filter: ThreadParentFilter,
    id_prefix: Option<String>,
}

/// Pagination/filter query for listing threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadQuery {
    /// Number of items to skip after filtering.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Filter by external resource grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// Filter by parent/root lineage.
    #[serde(default, skip_serializing_if = "ThreadParentFilter::is_any")]
    pub parent_filter: ThreadParentFilter,
    /// Backend-internal scope filter: keep only thread IDs that start with this
    /// prefix. Pushed down so a scoped listing never scans the full thread set
    /// of a shared backend (ADR-0042 scope boundary). Server routes must not
    /// expose this as a user-controlled HTTP filter; scoped wrappers inject it
    /// from trusted `ScopeContext`. `None` means no backend scope filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_prefix: Option<String>,
}

impl Default for ThreadQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            resource_id: None,
            parent_filter: ThreadParentFilter::Any,
            id_prefix: None,
        }
    }
}

impl ThreadQuery {
    /// Return true when the query carries any non-pagination filter.
    #[must_use]
    pub fn has_filters(&self) -> bool {
        normalize_lineage_id(self.resource_id.as_deref()).is_some()
            || !self.parent_filter.is_any()
            || self.id_prefix.is_some()
    }

    /// Return a copy with normalized lineage filters.
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.min(200),
            resource_id: normalize_lineage_id(self.resource_id.as_deref()),
            parent_filter: self.parent_filter.normalized(),
            id_prefix: self.id_prefix.clone(),
        }
    }

    /// Encode an opaque cursor for continuing this exact query.
    #[must_use]
    pub fn encode_cursor(&self, offset: usize) -> String {
        let normalized = self.normalized();
        encode_cursor_token(
            THREAD_CURSOR_PREFIX,
            &ThreadCursorToken {
                offset,
                resource_id: normalized.resource_id,
                parent_filter: normalized.parent_filter,
                id_prefix: normalized.id_prefix,
            },
        )
    }

    /// Decode a cursor and verify it belongs to this exact query shape.
    pub fn decode_cursor(&self, cursor: &str) -> Result<usize, String> {
        let normalized = self.normalized();
        if let Ok(offset) = cursor.parse::<usize>() {
            return if normalized.has_filters() {
                Err("cursor does not match thread query filters".to_string())
            } else {
                Ok(offset)
            };
        }

        let token: ThreadCursorToken = decode_cursor_token(THREAD_CURSOR_PREFIX, cursor)?;
        if token.resource_id != normalized.resource_id
            || token.parent_filter != normalized.parent_filter
            || token.id_prefix != normalized.id_prefix
        {
            return Err("cursor does not match thread query filters".to_string());
        }
        Ok(token.offset)
    }

    /// Return true when a thread passes the query filters.
    #[must_use]
    pub fn matches_thread(&self, thread: &Thread) -> bool {
        let normalized = self.normalized();
        if normalized
            .id_prefix
            .as_deref()
            .is_some_and(|prefix| !thread.id.starts_with(prefix))
        {
            return false;
        }
        if normalized
            .resource_id
            .as_deref()
            .is_some_and(|resource_id| {
                normalize_lineage_id(thread.resource_id.as_deref()).as_deref() != Some(resource_id)
            })
        {
            return false;
        }
        match &normalized.parent_filter {
            ThreadParentFilter::Any => {}
            ThreadParentFilter::Root => {
                if normalize_lineage_id(thread.parent_thread_id.as_deref()).is_some() {
                    return false;
                }
            }
            ThreadParentFilter::Parent(parent_thread_id) => {
                if normalize_lineage_id(thread.parent_thread_id.as_deref()).as_deref()
                    != Some(parent_thread_id.as_str())
                {
                    return false;
                }
            }
        }
        true
    }
}

/// Paginated thread ID response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub items: Vec<String>,
    pub total: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<String>,
}

impl ThreadPage {
    /// Empty thread page.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            has_more: false,
            next_cursor: None,
            prev_cursor: None,
        }
    }
}

/// How deleting a thread should treat direct and transitive child threads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildThreadDeleteStrategy {
    /// Reject deletion when at least one direct child exists.
    Reject,
    /// Preserve child threads and clear their `parent_thread_id`.
    #[default]
    Detach,
    /// Recursively delete all descendants before deleting the target thread.
    Cascade,
}

/// Parent thread that should be materialized by a checkpoint projection.
#[must_use]
pub fn checkpoint_parent_thread_id<'a>(
    existing_thread: Option<&'a Thread>,
    run: &'a RunRecord,
) -> Option<&'a str> {
    existing_thread
        .and_then(|thread| thread.parent_thread_id.as_deref())
        .or_else(|| {
            run.request
                .as_ref()
                .and_then(|request| request.parent_thread_id.as_deref())
        })
}

/// Sort threads by recent activity, then ID for deterministic ties.
pub fn sort_threads_by_recent_activity(threads: &mut [Thread]) {
    threads.sort_by(|a, b| {
        let a_updated = a.metadata.updated_at.or(a.metadata.created_at).unwrap_or(0);
        let b_updated = b.metadata.updated_at.or(b.metadata.created_at).unwrap_or(0);
        b_updated.cmp(&a_updated).then_with(|| a.id.cmp(&b.id))
    });
}

/// Apply thread filters and offset pagination to an in-memory thread set.
#[must_use]
pub fn paginate_threads(mut threads: Vec<Thread>, query: &ThreadQuery) -> ThreadPage {
    let query = query.normalized();
    sort_threads_by_recent_activity(&mut threads);
    let filtered: Vec<Thread> = threads
        .into_iter()
        .filter(|thread| query.matches_thread(thread))
        .collect();
    let total = filtered.len();
    let start = query.offset.min(total);
    let items: Vec<String> = filtered
        .into_iter()
        .skip(start)
        .take(query.limit)
        .map(|thread| thread.id)
        .collect();
    let next_offset = start + items.len();
    let has_more = query.limit > 0 && next_offset < total;
    ThreadPage {
        items,
        total,
        has_more,
        next_cursor: has_more.then(|| query.encode_cursor(next_offset)),
        prev_cursor: (query.limit > 0 && start > 0)
            .then(|| query.encode_cursor(start.saturating_sub(query.limit))),
    }
}

/// Apply message filters and offset pagination to an in-memory record set.
#[must_use]
pub fn paginate_message_records(
    mut records: Vec<MessageRecord>,
    query: &MessageQuery,
) -> MessagePage {
    let query = query.normalized();
    records.retain(|record| query.matches_record(record));
    match query.order {
        MessageOrder::Asc => records.sort_by_key(|record| record.seq),
        MessageOrder::Desc => records.sort_by(|a, b| b.seq.cmp(&a.seq)),
    }
    let total = records.len();
    let start = query.offset.min(total);
    let page_records: Vec<MessageRecord> =
        records.into_iter().skip(start).take(query.limit).collect();
    let next_offset = start + page_records.len();
    let has_more = query.limit > 0 && next_offset < total;
    MessagePage {
        records: page_records,
        total,
        has_more,
        next_cursor: has_more.then(|| query.encode_cursor(next_offset)),
        prev_cursor: (query.limit > 0 && start > 0)
            .then(|| query.encode_cursor(start.saturating_sub(query.limit))),
    }
}

fn encode_cursor_token<T: Serialize>(prefix: &str, token: &T) -> String {
    let bytes = serde_json::to_vec(token).expect("cursor token serialization should succeed");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor_token<T: DeserializeOwned>(prefix: &str, cursor: &str) -> Result<T, String> {
    let payload = cursor
        .strip_prefix(prefix)
        .ok_or_else(|| "cursor must be a valid pagination token".to_string())?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "cursor must be a valid pagination token".to_string())?;
    serde_json::from_slice(&decoded)
        .map_err(|_| "cursor must be a valid pagination token".to_string())
}

/// Pagination/filter query for listing runs.
#[derive(Debug, Clone)]
pub struct RunQuery {
    /// Number of items to skip.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Filter by thread ID.
    pub thread_id: Option<String>,
    /// Filter by run status.
    pub status: Option<RunStatus>,
    /// Backend-level scope filter: keep only runs whose `thread_id` starts with
    /// this prefix. Pushed down so a scoped listing never scans the full run
    /// table of a shared backend (ADR-0042 scope boundary). `None` means no
    /// prefix filter.
    pub id_prefix: Option<String>,
}

impl RunQuery {
    /// True when `thread_id` passes the optional `id_prefix` filter.
    #[must_use]
    pub fn matches_id_prefix(&self, thread_id: &str) -> bool {
        self.id_prefix
            .as_deref()
            .is_none_or(|prefix| thread_id.starts_with(prefix))
    }
}

impl Default for RunQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            thread_id: None,
            status: None,
            id_prefix: None,
        }
    }
}

/// Paginated run list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    pub items: Vec<RunRecord>,
    pub total: usize,
    pub has_more: bool,
}

#[derive(Clone)]
pub struct ScopedThreadRunStore {
    inner: Arc<dyn ThreadRunStore>,
    scope_id: ScopeId,
}

impl ScopedThreadRunStore {
    pub fn new(inner: Arc<dyn ThreadRunStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn ThreadRunStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    /// Prefix shared by every key in this scope (`scope:<len>:<scope>:`). Pushed
    /// to the backend as a filter so scoped listings never scan other scopes.
    fn scope_prefix(&self) -> String {
        scoped_key(&self.scope_id, "")
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_thread(&self, thread: &Thread) -> Thread {
        let mut thread = thread.clone();
        thread.id = self.scoped(&thread.id);
        thread.parent_thread_id = thread.parent_thread_id.as_deref().map(|id| self.scoped(id));
        thread
    }

    fn decode_thread(&self, mut thread: Thread) -> Option<Thread> {
        thread.id = self.unscoped(&thread.id)?.to_string();
        thread.parent_thread_id = match thread.parent_thread_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        Some(thread)
    }

    fn encode_run(&self, run: &RunRecord) -> RunRecord {
        let mut run = run.clone();
        run.run_id = self.scoped(&run.run_id);
        run.thread_id = self.scoped(&run.thread_id);
        run.parent_run_id = run.parent_run_id.as_deref().map(|id| self.scoped(id));
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.scoped(&input.thread_id);
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.scoped(&output.thread_id);
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = request
                .parent_thread_id
                .as_deref()
                .map(|id| self.scoped(id));
        }
        run
    }

    fn decode_run(&self, mut run: RunRecord) -> Option<RunRecord> {
        run.run_id = self.unscoped(&run.run_id)?.to_string();
        run.thread_id = self.unscoped(&run.thread_id)?.to_string();
        run.parent_run_id = match run.parent_run_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.unscoped(&input.thread_id)?.to_string();
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.unscoped(&output.thread_id)?.to_string();
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = match request.parent_thread_id.as_deref() {
                Some(id) => Some(self.unscoped(id)?.to_string()),
                None => None,
            };
        }
        Some(run)
    }

    fn decode_message_record(&self, mut record: MessageRecord) -> Option<MessageRecord> {
        record.thread_id = self.unscoped(&record.thread_id)?.to_string();
        if let Some(run_id) = record.produced_by_run_id.as_deref()
            && let Some(unscoped) = self.unscoped(run_id)
        {
            record.produced_by_run_id = Some(unscoped.to_string());
        }
        Some(record)
    }

    fn encode_message_query(&self, query: &MessageQuery) -> MessageQuery {
        let mut query = query.clone();
        query.run_id = query.run_id.as_deref().map(|id| self.scoped(id));
        query
    }
}

#[async_trait]
impl ThreadStore for ScopedThreadRunStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        Ok(self
            .inner
            .load_thread(&self.scoped(thread_id))
            .await?
            .and_then(|thread| self.decode_thread(thread)))
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(&self.encode_thread(thread)).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(&self.scoped(thread_id)).await
    }

    async fn save_thread_state(
        &self,
        thread_id: &str,
        state: &awaken_runtime_contract::state::PersistedState,
    ) -> Result<(), StorageError> {
        self.inner
            .save_thread_state(&self.scoped(thread_id), state)
            .await
    }

    async fn load_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<awaken_runtime_contract::state::PersistedState>, StorageError> {
        self.inner.load_thread_state(&self.scoped(thread_id)).await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        // Push the scope prefix to the backend so it filters and paginates at
        // the source instead of streaming every scope's threads into memory.
        // ThreadQuery caps one page at 200; loop internally so this legacy
        // vector API preserves the caller-requested `limit`.
        let scope_prefix = self.scope_prefix();
        let mut next_offset = offset;
        let mut items = Vec::with_capacity(limit.min(200));
        while items.len() < limit {
            let page_limit = (limit - items.len()).min(200);
            let page = self
                .inner
                .list_threads_query(&ThreadQuery {
                    offset: next_offset,
                    limit: page_limit,
                    resource_id: None,
                    parent_filter: ThreadParentFilter::Any,
                    id_prefix: Some(scope_prefix.clone()),
                })
                .await?;
            let page_len = page.items.len();
            items.extend(
                page.items
                    .into_iter()
                    .filter_map(|id| self.unscoped(&id).map(str::to_string)),
            );
            if !page.has_more || page_len == 0 {
                break;
            }
            next_offset = next_offset.saturating_add(page_len);
        }
        Ok(items)
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(&self.scoped(thread_id)).await
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner
            .load_committed_messages(&self.scoped(thread_id))
            .await
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        Ok(self
            .inner
            .load_message_records(&self.scoped(thread_id))
            .await?
            .map(|records| {
                records
                    .into_iter()
                    .filter_map(|record| self.decode_message_record(record))
                    .collect()
            }))
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let query = self.encode_message_query(query);
        let mut page = self
            .inner
            .list_message_records(&self.scoped(thread_id), &query)
            .await?;
        page.records = page
            .records
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect();
        Ok(page)
    }

    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        Ok(self
            .inner
            .append_message_records(&self.scoped(thread_id), messages)
            .await?
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner
            .save_messages(&self.scoped(thread_id), messages)
            .await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(&self.scoped(thread_id)).await
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner
            .update_thread_metadata(&self.scoped(id), metadata)
            .await
    }
}

#[async_trait]
impl RunStore for ScopedThreadRunStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(&self.encode_run(record)).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .load_run(&self.scoped(run_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .latest_run(&self.scoped(thread_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        // Single-thread queries are bounded to one (in-scope) thread, so push
        // the caller's offset/limit straight to the backend — no over-fetch and
        // no in-memory pagination. Every row of a scoped thread is in scope, so
        // `decode_run` never drops one and the backend's page totals are exact.
        if let Some(thread_id) = query.thread_id.as_deref() {
            let inner_page = self
                .inner
                .list_runs(&RunQuery {
                    offset: query.offset,
                    limit: query.limit,
                    thread_id: Some(self.scoped(thread_id)),
                    status: query.status,
                    id_prefix: None,
                })
                .await?;
            let items = inner_page
                .items
                .into_iter()
                .filter_map(|record| self.decode_run(record))
                .collect();
            return Ok(RunPage {
                items,
                total: inner_page.total,
                has_more: inner_page.has_more,
            });
        }

        // Cross-scope listing: push the scope prefix to the backend so it
        // filters and paginates at the source. Every returned row is in scope,
        // so `decode_run` never drops one and the page totals are exact — no
        // full-table scan, no in-memory windowing.
        let inner_page = self
            .inner
            .list_runs(&RunQuery {
                offset: query.offset,
                limit: query.limit,
                thread_id: None,
                status: query.status,
                id_prefix: Some(self.scope_prefix()),
            })
            .await?;
        let items = inner_page
            .items
            .into_iter()
            .filter_map(|record| self.decode_run(record))
            .collect();
        Ok(RunPage {
            items,
            total: inner_page.total,
            has_more: inner_page.has_more,
        })
    }
}

#[async_trait]
impl ThreadRunStore for ScopedThreadRunStore {
    #[allow(deprecated)]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.inner
            .checkpoint(&self.scoped(thread_id), messages, &self.encode_run(run))
            .await
    }

    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        self.inner
            .checkpoint_append(
                &self.scoped(thread_id),
                messages,
                expected_version,
                &self.encode_run(run),
            )
            .await
    }
}

#[cfg(test)]
mod query_tests {
    use super::*;
    use awaken_runtime_contract::contract::lifecycle::RunStatus;

    #[test]
    fn run_page_with_multiple_records_roundtrips() {
        let record = |run_id: &str, status: RunStatus, parent: Option<&str>| RunRecord {
            run_id: run_id.into(),
            thread_id: "t-1".into(),
            agent_id: "a-1".into(),
            parent_run_id: parent.map(str::to_string),
            resolution_id: None,
            activation: None,
            request: None,
            input: None,
            output: None,
            status,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 100,
            started_at: None,
            finished_at: None,
            updated_at: 200,
            steps: 1,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        };
        let page = RunPage {
            items: vec![
                record("r-1", RunStatus::Done, None),
                record("r-2", RunStatus::Running, Some("r-1")),
            ],
            total: 5,
            has_more: true,
        };

        let json = serde_json::to_string(&page).unwrap();
        let parsed: RunPage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.total, 5);
        assert!(parsed.has_more);
    }

    #[test]
    fn query_defaults_are_sensible() {
        let mq = MessageQuery::default();
        assert_eq!(mq.offset, 0);
        assert_eq!(mq.limit, 50);

        let rq = RunQuery::default();
        assert_eq!(rq.offset, 0);
        assert_eq!(rq.limit, 50);
        assert!(rq.thread_id.is_none());
        assert!(rq.status.is_none());
    }
}
