// Some items (MailboxDispatcher, AgentReceiver, etc.) are only used by #[cfg(test)] tests.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tirea_agentos::contracts::storage::{
    MailboxEntry, MailboxEntryOrigin, MailboxEntryStatus, MailboxQuery, MailboxReader,
    MailboxStore, MailboxStoreError, RunStatus, ThreadReader,
};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::{AgentOs, AgentOsRunError, RunStream};
use tirea_contract::storage::{MailboxReceiver, ReceiveOutcome, RunRecord};

use super::ApiError;

const DEFAULT_MAILBOX_POLL_INTERVAL_MS: u64 = 100;
pub(crate) const DEFAULT_MAILBOX_LEASE_MS: u64 = 30_000;
const DEFAULT_MAILBOX_RETRY_MS: u64 = 250;
const DEFAULT_MAILBOX_BATCH_SIZE: usize = 16;
const DEFAULT_MAILBOX_MAX_ATTEMPTS: u32 = 10;
const DEFAULT_MAILBOX_GC_INTERVAL_SECS: u64 = 60;
const DEFAULT_MAILBOX_GC_TTL_MS: u64 = 24 * 60 * 60 * 1000; // 24 hours
pub(crate) const INLINE_MAILBOX_AVAILABLE_AT: u64 = i64::MAX as u64;

pub(crate) fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

// ---------------------------------------------------------------------------
// Agent-specific helpers
// ---------------------------------------------------------------------------

pub(crate) fn normalize_background_run_request(
    agent_id: &str,
    mut request: RunRequest,
) -> RunRequest {
    request.agent_id = agent_id.to_string();
    if request.thread_id.is_none() {
        request.thread_id = Some(new_id());
    }
    if request.run_id.is_none() {
        request.run_id = Some(new_id());
    }
    request
}

/// Optional envelope fields for enqueued mailbox entries.
#[derive(Debug, Clone, Default)]
pub struct EnqueueOptions {
    /// Identity of the sender for audit and reply routing.
    pub sender_id: Option<String>,
    /// Dispatch priority (higher = dispatched first). Default 0.
    pub priority: u8,
    /// Deduplication key — rejected if another entry with the same key exists in the mailbox.
    pub dedupe_key: Option<String>,
}

pub(crate) fn mailbox_entry_from_request(
    request: &RunRequest,
    generation: u64,
    options: &EnqueueOptions,
    available_at: u64,
) -> MailboxEntry {
    let now = now_unix_millis();
    let mailbox_id = request
        .thread_id
        .clone()
        .expect("background mailbox request should have thread_id");
    let payload = serde_json::to_value(request).expect("RunRequest should be serializable");
    MailboxEntry {
        entry_id: new_id(),
        mailbox_id,
        origin: MailboxEntryOrigin::from_run_origin(request.origin),
        sender_id: options.sender_id.clone(),
        payload,
        priority: options.priority,
        dedupe_key: options.dedupe_key.clone(),
        generation,
        status: MailboxEntryStatus::Queued,
        available_at,
        attempt_count: 0,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        created_at: now,
        updated_at: now,
    }
}

pub(crate) fn mailbox_error(err: MailboxStoreError) -> ApiError {
    ApiError::Internal(err.to_string())
}

pub(crate) fn is_generation_mismatch(err: &MailboxStoreError) -> bool {
    matches!(err, MailboxStoreError::GenerationMismatch { .. })
}

pub(crate) fn is_permanent_dispatch_error(err: &AgentOsRunError) -> bool {
    matches!(err, AgentOsRunError::Resolve(_))
}

pub(crate) async fn drain_background_run(mut run: RunStream) {
    while run.events.next().await.is_some() {}
}

pub(crate) fn parent_completion_notification_dedupe_key(
    parent_run_id: &str,
    child_run_id: &str,
) -> String {
    format!("parent-task-completion:{parent_run_id}:{child_run_id}")
}

fn parent_completion_notification_status(record: &RunRecord) -> Option<&'static str> {
    if record.status != RunStatus::Done {
        return None;
    }

    match record.termination_code.as_deref() {
        Some("error") => Some("failed"),
        Some("cancelled") => Some("cancelled"),
        Some(code) if code.starts_with("stopped:") => Some("stopped"),
        Some("behavior_requested" | "natural") | None => Some("completed"),
        Some(_) => Some("completed"),
    }
}

pub(crate) fn build_parent_completion_notification_message(record: &RunRecord) -> Option<Message> {
    let recipient_task_id = record.parent_run_id.clone()?;
    let status = parent_completion_notification_status(record)?;
    let task_id = record
        .source_mailbox_entry_id
        .as_deref()
        .unwrap_or(record.run_id.as_str());
    let content = serde_json::json!({
        "type": "background_task_notification",
        "recipient_task_id": recipient_task_id,
        "child_task_id": task_id,
        "child_run_id": record.run_id,
        "child_thread_id": record.thread_id,
        "status": status,
        "termination_code": record.termination_code,
        "termination_detail": record.termination_detail,
        "result_ref": {
            "task_id": task_id,
            "run_id": record.run_id,
            "thread_id": record.thread_id,
        },
    });

    Some(Message::internal_system(content.to_string()))
}

// ---------------------------------------------------------------------------
// Entry lifecycle helpers
// ---------------------------------------------------------------------------

pub(crate) async fn ack_claimed_entry(
    mailbox_store: &Arc<dyn MailboxStore>,
    entry_id: &str,
    claim_token: &str,
) -> Result<(), ApiError> {
    match mailbox_store
        .ack_mailbox_entry(entry_id, claim_token, now_unix_millis())
        .await
    {
        Ok(()) => Ok(()),
        Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
        Err(err) => Err(mailbox_error(err)),
    }
}

pub(crate) async fn nack_claimed_entry(
    mailbox_store: &Arc<dyn MailboxStore>,
    entry_id: &str,
    claim_token: &str,
    retry_delay_ms: u64,
    error: &str,
) -> Result<(), ApiError> {
    let now = now_unix_millis();
    match mailbox_store
        .nack_mailbox_entry(
            entry_id,
            claim_token,
            now.saturating_add(retry_delay_ms),
            error,
            now,
        )
        .await
    {
        Ok(()) => Ok(()),
        Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
        Err(err) => Err(mailbox_error(err)),
    }
}

pub(crate) async fn dead_letter_claimed_entry(
    mailbox_store: &Arc<dyn MailboxStore>,
    entry_id: &str,
    claim_token: &str,
    error: &str,
) -> Result<(), ApiError> {
    match mailbox_store
        .dead_letter_mailbox_entry(entry_id, claim_token, error, now_unix_millis())
        .await
    {
        Ok(()) => Ok(()),
        Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
        Err(err) => Err(mailbox_error(err)),
    }
}

// ---------------------------------------------------------------------------
// Agent receiver (implements MailboxReceiver for agent runs)
// ---------------------------------------------------------------------------

pub struct AgentReceiver {
    os: Arc<AgentOs>,
}

impl AgentReceiver {
    pub fn new(os: Arc<AgentOs>) -> Self {
        Self { os }
    }
}

#[async_trait]
impl MailboxReceiver for AgentReceiver {
    async fn receive(&self, entry: &MailboxEntry) -> ReceiveOutcome {
        let mut request: RunRequest = match serde_json::from_value(entry.payload.clone()) {
            Ok(r) => r,
            Err(e) => return ReceiveOutcome::Reject(format!("invalid payload: {e}")),
        };

        request.source_mailbox_entry_id = Some(entry.entry_id.clone());

        let agent_id = request.agent_id.clone();
        let thread_id = request
            .thread_id
            .clone()
            .unwrap_or_else(|| entry.mailbox_id.clone());

        match self
            .os
            .current_run_id_for_thread(&agent_id, &thread_id)
            .await
        {
            Ok(Some(_)) => return ReceiveOutcome::Retry("thread has active run".into()),
            Ok(None) => {}
            Err(e) => return ReceiveOutcome::Retry(e.to_string()),
        }

        let resolved = match self.os.resolve(&agent_id) {
            Ok(r) => r,
            Err(e) => return ReceiveOutcome::Reject(e.to_string()),
        };

        match self
            .os
            .start_active_run_with_persistence(&agent_id, request, resolved, true, false)
            .await
        {
            Ok(run) => {
                tokio::spawn(drain_background_run(run));
                ReceiveOutcome::Accepted
            }
            Err(e) if is_permanent_dispatch_error(&e) => ReceiveOutcome::Reject(e.to_string()),
            Err(e) => ReceiveOutcome::Retry(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent-specific inline run start (for streaming/synchronous runs)
// ---------------------------------------------------------------------------

pub(crate) enum MailboxRunStartError {
    Busy(String),
    Superseded(String),
    Permanent(String),
    Retryable(String),
    Internal(ApiError),
}

pub(crate) async fn start_agent_run_for_entry(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    entry: &MailboxEntry,
    persist_run: bool,
) -> Result<RunStream, MailboxRunStartError> {
    // Verify entry still claimed with our token
    if let Some(current) = mailbox_store
        .load_mailbox_entry(&entry.entry_id)
        .await
        .map_err(mailbox_error)
        .map_err(MailboxRunStartError::Internal)?
    {
        if current.status != MailboxEntryStatus::Claimed || current.claim_token != entry.claim_token
        {
            return Err(MailboxRunStartError::Superseded(
                current
                    .last_error
                    .unwrap_or_else(|| "mailbox entry is no longer active".to_string()),
            ));
        }
    }

    // Check generation
    if mailbox_store
        .load_mailbox_state(&entry.mailbox_id)
        .await
        .map_err(mailbox_error)
        .map_err(MailboxRunStartError::Internal)?
        .is_some_and(|state| state.current_generation != entry.generation)
    {
        return Err(MailboxRunStartError::Superseded(
            "mailbox entry superseded by interrupt".to_string(),
        ));
    }

    // Deserialize payload
    let mut request: RunRequest = serde_json::from_value(entry.payload.clone())
        .map_err(|e| MailboxRunStartError::Permanent(format!("invalid payload: {e}")))?;
    request.source_mailbox_entry_id = Some(entry.entry_id.clone());

    let agent_id = request.agent_id.clone();
    let thread_id = request
        .thread_id
        .clone()
        .unwrap_or_else(|| entry.mailbox_id.clone());

    // Check for active run on thread
    match os.current_run_id_for_thread(&agent_id, &thread_id).await {
        Ok(Some(_)) => {
            return Err(MailboxRunStartError::Busy(
                "thread already has an active run".to_string(),
            ));
        }
        Ok(None) => {}
        Err(err) => return Err(MailboxRunStartError::Internal(ApiError::from(err))),
    }

    let resolved = os
        .resolve(&agent_id)
        .map_err(|err| MailboxRunStartError::Permanent(err.to_string()))?;

    os.start_active_run_with_persistence(&agent_id, request, resolved, persist_run, !persist_run)
        .await
        .map_err(|err| {
            if is_permanent_dispatch_error(&err) {
                MailboxRunStartError::Permanent(err.to_string())
            } else {
                MailboxRunStartError::Retryable(err.to_string())
            }
        })
}

// ---------------------------------------------------------------------------
// Enqueue / background run
// ---------------------------------------------------------------------------

/// Enqueue a mailbox entry from a RunRequest. Returns (mailbox_id, entry_id, run_id).
pub(crate) async fn enqueue_mailbox_run(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
    available_at: u64,
    options: EnqueueOptions,
) -> Result<(String, String, String), ApiError> {
    os.resolve(agent_id).map_err(AgentOsRunError::from)?;

    let request = normalize_background_run_request(agent_id, request);
    let mailbox_id = request
        .thread_id
        .clone()
        .expect("normalized mailbox run request should have thread_id");
    let run_id = request
        .run_id
        .clone()
        .expect("normalized mailbox run request should have run_id");

    for _ in 0..2 {
        let now = now_unix_millis();
        let state = mailbox_store
            .ensure_mailbox_state(&mailbox_id, now)
            .await
            .map_err(mailbox_error)?;
        let entry =
            mailbox_entry_from_request(&request, state.current_generation, &options, available_at);
        let entry_id = entry.entry_id.clone();

        match mailbox_store.enqueue_mailbox_entry(&entry).await {
            Ok(()) => return Ok((mailbox_id, entry_id, run_id)),
            Err(err) if is_generation_mismatch(&err) => continue,
            Err(err) => return Err(mailbox_error(err)),
        }
    }

    Err(ApiError::Internal(format!(
        "mailbox enqueue raced with interrupt for mailbox '{mailbox_id}'"
    )))
}

/// Enqueue a background run. Returns (thread_id, run_id, entry_id).
pub async fn enqueue_background_run(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
    options: EnqueueOptions,
) -> Result<(String, String, String), ApiError> {
    let (mailbox_id, entry_id, run_id) = enqueue_mailbox_run(
        os,
        mailbox_store,
        agent_id,
        request,
        now_unix_millis(),
        options,
    )
    .await?;
    Ok((mailbox_id, run_id, entry_id))
}

pub async fn start_streaming_run_via_mailbox(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
    consumer_id: &str,
    options: EnqueueOptions,
) -> Result<RunStream, ApiError> {
    let (_mailbox_id, entry_id, _run_id) = enqueue_mailbox_run(
        os,
        mailbox_store,
        agent_id,
        request,
        INLINE_MAILBOX_AVAILABLE_AT,
        options,
    )
    .await?;

    let Some(entry) = mailbox_store
        .claim_mailbox_entry(
            &entry_id,
            consumer_id,
            now_unix_millis(),
            DEFAULT_MAILBOX_LEASE_MS,
        )
        .await
        .map_err(mailbox_error)?
    else {
        let existing = mailbox_store
            .load_mailbox_entry(&entry_id)
            .await
            .map_err(mailbox_error)?;
        return Err(match existing {
            Some(entry) if entry.status == MailboxEntryStatus::Accepted => {
                ApiError::BadRequest("run has already been accepted".to_string())
            }
            Some(entry) if entry.status == MailboxEntryStatus::Superseded => {
                ApiError::BadRequest("run has been superseded".to_string())
            }
            Some(entry) if entry.status == MailboxEntryStatus::Cancelled => {
                ApiError::BadRequest("run has already been cancelled".to_string())
            }
            Some(entry) if entry.status == MailboxEntryStatus::DeadLetter => ApiError::Internal(
                entry
                    .last_error
                    .unwrap_or_else(|| "mailbox entry moved to dead letter".to_string()),
            ),
            Some(_) => ApiError::BadRequest("entry is already claimed".to_string()),
            None => ApiError::Internal(format!(
                "mailbox entry '{entry_id}' disappeared before inline dispatch"
            )),
        });
    };

    let claim_token = entry.claim_token.clone().ok_or_else(|| {
        ApiError::Internal(format!(
            "mailbox entry '{}' was claimed without claim_token",
            entry.entry_id
        ))
    })?;

    match start_agent_run_for_entry(os, mailbox_store, &entry, false).await {
        Ok(run) => {
            ack_claimed_entry(mailbox_store, &entry.entry_id, &claim_token).await?;
            Ok(run)
        }
        Err(MailboxRunStartError::Superseded(error)) => {
            let _ = mailbox_store
                .supersede_mailbox_entry(&entry.entry_id, now_unix_millis(), &error)
                .await
                .map_err(mailbox_error)?;
            Err(ApiError::BadRequest(error))
        }
        Err(MailboxRunStartError::Busy(error)) => {
            mailbox_store
                .cancel_mailbox_entry(&entry.entry_id, now_unix_millis())
                .await
                .map_err(mailbox_error)?;
            Err(ApiError::BadRequest(error))
        }
        Err(MailboxRunStartError::Permanent(error))
        | Err(MailboxRunStartError::Retryable(error)) => {
            dead_letter_claimed_entry(mailbox_store, &entry.entry_id, &claim_token, &error).await?;
            Err(ApiError::Internal(error))
        }
        Err(MailboxRunStartError::Internal(error)) => {
            dead_letter_claimed_entry(
                mailbox_store,
                &entry.entry_id,
                &claim_token,
                &error.to_string(),
            )
            .await?;
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Thread / mailbox lifecycle
// ---------------------------------------------------------------------------

pub async fn cancel_pending_for_mailbox(
    mailbox_store: &Arc<dyn MailboxStore>,
    mailbox_id: &str,
    exclude_entry_id: Option<&str>,
) -> Result<Vec<MailboxEntry>, ApiError> {
    mailbox_store
        .cancel_pending_for_mailbox(mailbox_id, now_unix_millis(), exclude_entry_id)
        .await
        .map_err(mailbox_error)
}

pub struct ThreadInterruptResult {
    pub cancelled_run_id: Option<String>,
    pub generation: u64,
    pub superseded_entries: Vec<MailboxEntry>,
}

pub async fn interrupt_thread(
    os: &Arc<AgentOs>,
    read_store: &dyn ThreadReader,
    mailbox_store: &Arc<dyn MailboxStore>,
    thread_id: &str,
) -> Result<ThreadInterruptResult, ApiError> {
    let interrupted = mailbox_store
        .interrupt_mailbox(thread_id, now_unix_millis())
        .await
        .map_err(mailbox_error)?;
    let cancelled_run_id = os.cancel_active_run_by_thread(thread_id).await;

    if cancelled_run_id.is_none() && interrupted.superseded_entries.is_empty() {
        let thread_exists = read_store
            .load_thread(thread_id)
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))?
            .is_some();
        if !thread_exists {
            let mailbox_page = mailbox_store
                .list_mailbox_entries(&MailboxQuery {
                    mailbox_id: Some(thread_id.to_string()),
                    limit: 1,
                    ..Default::default()
                })
                .await
                .map_err(mailbox_error)?;
            if mailbox_page.total == 0 {
                return Err(ApiError::ThreadNotFound(thread_id.to_string()));
            }
        }
    }

    Ok(ThreadInterruptResult {
        cancelled_run_id,
        generation: interrupted.mailbox_state.current_generation,
        superseded_entries: interrupted.superseded_entries,
    })
}

// ---------------------------------------------------------------------------
// Background task lookup / cancel
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum BackgroundTaskLookup {
    Run(RunRecord),
    Mailbox(MailboxEntry),
}

/// Look up a background task by run_id (RunStore) or entry_id (MailboxStore).
///
/// If the entry has been accepted, tries to find the corresponding RunRecord
/// by extracting the run_id from the payload.
pub async fn load_background_task(
    read_store: &dyn ThreadReader,
    mailbox_store: &dyn MailboxReader,
    id: &str,
) -> Result<Option<BackgroundTaskLookup>, ApiError> {
    // Try run store first (id interpreted as run_id).
    if let Some(record) = read_store
        .load_run(id)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
    {
        return Ok(Some(BackgroundTaskLookup::Run(record)));
    }

    // Try mailbox store (id interpreted as entry_id).
    let Some(entry) = mailbox_store
        .load_mailbox_entry(id)
        .await
        .map_err(mailbox_error)?
    else {
        return Ok(None);
    };

    // If the entry was accepted, look up the RunRecord via run_id from payload.
    if entry.status == MailboxEntryStatus::Accepted {
        if let Some(run_id) = entry.payload.get("run_id").and_then(|v| v.as_str()) {
            if let Some(record) = read_store
                .load_run(run_id)
                .await
                .map_err(|err| ApiError::Internal(err.to_string()))?
            {
                return Ok(Some(BackgroundTaskLookup::Run(record)));
            }
        }
    }

    Ok(Some(BackgroundTaskLookup::Mailbox(entry)))
}

pub enum CancelBackgroundRunResult {
    Active,
    Pending,
}

/// Cancel by run_id (active run) or entry_id (queued mailbox entry).
///
/// If `id` is a run_id, cancels the active run directly.
/// If `id` is an entry_id for a queued entry, cancels the mailbox entry.
/// If `id` is an entry_id for an accepted entry, extracts the run_id from
/// the payload and cancels the active run.
pub async fn try_cancel_active_or_queued_run_by_id(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    id: &str,
) -> Result<Option<CancelBackgroundRunResult>, ApiError> {
    // Try cancelling active run directly (id interpreted as run_id).
    if os.cancel_active_run_by_id(id).await {
        return Ok(Some(CancelBackgroundRunResult::Active));
    }

    // Try cancelling a queued mailbox entry (id interpreted as entry_id).
    let cancelled = mailbox_store
        .cancel_mailbox_entry(id, now_unix_millis())
        .await
        .map_err(mailbox_error)?;
    if cancelled
        .as_ref()
        .is_some_and(|entry| entry.status == MailboxEntryStatus::Cancelled)
    {
        return Ok(Some(CancelBackgroundRunResult::Pending));
    }

    // If the entry exists but is already accepted, try extracting run_id from
    // the payload to cancel the active run.
    let entry = match cancelled {
        Some(e) => Some(e),
        None => mailbox_store
            .load_mailbox_entry(id)
            .await
            .map_err(mailbox_error)?,
    };
    if let Some(entry) = entry {
        if entry.status == MailboxEntryStatus::Accepted {
            if let Some(run_id) = entry
                .payload
                .get("run_id")
                .and_then(|v| v.as_str())
                .filter(|rid| *rid != id)
            {
                if os.cancel_active_run_by_id(run_id).await {
                    return Ok(Some(CancelBackgroundRunResult::Active));
                }
            }
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Generic mailbox dispatcher
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MailboxDispatcher {
    mailbox_store: Arc<dyn MailboxStore>,
    receiver: Arc<dyn MailboxReceiver>,
    consumer_id: String,
    poll_interval: Duration,
    lease_duration_ms: u64,
    retry_delay_ms: u64,
    batch_size: usize,
    max_attempts: u32,
}

impl MailboxDispatcher {
    pub fn new(mailbox_store: Arc<dyn MailboxStore>, receiver: Arc<dyn MailboxReceiver>) -> Self {
        Self {
            mailbox_store,
            receiver,
            consumer_id: format!("mailbox-{}", new_id()),
            poll_interval: Duration::from_millis(DEFAULT_MAILBOX_POLL_INTERVAL_MS),
            lease_duration_ms: DEFAULT_MAILBOX_LEASE_MS,
            retry_delay_ms: DEFAULT_MAILBOX_RETRY_MS,
            batch_size: DEFAULT_MAILBOX_BATCH_SIZE,
            max_attempts: DEFAULT_MAILBOX_MAX_ATTEMPTS,
        }
    }

    #[must_use]
    pub fn with_consumer_id(mut self, consumer_id: impl Into<String>) -> Self {
        self.consumer_id = consumer_id.into();
        self
    }

    async fn dispatch_claimed_entry(&self, entry: MailboxEntry) -> Result<(), ApiError> {
        let claim_token = entry.claim_token.clone().ok_or_else(|| {
            ApiError::Internal(format!(
                "mailbox entry '{}' was claimed without claim_token",
                entry.entry_id
            ))
        })?;

        let entry_id = entry.entry_id.as_str();

        // Check if superseded since claim (generation mismatch)
        if self
            .mailbox_store
            .load_mailbox_state(&entry.mailbox_id)
            .await
            .map_err(mailbox_error)?
            .is_some_and(|state| state.current_generation != entry.generation)
        {
            let _ = self
                .mailbox_store
                .supersede_mailbox_entry(entry_id, now_unix_millis(), "superseded by interrupt")
                .await;
            tracing::debug!(entry_id, "mailbox entry superseded by generation mismatch");
            return Ok(());
        }

        match self.receiver.receive(&entry).await {
            ReceiveOutcome::Accepted => {
                ack_claimed_entry(&self.mailbox_store, entry_id, &claim_token).await?;
                tracing::debug!(entry_id, "mailbox entry accepted");
            }
            ReceiveOutcome::Retry(reason) => {
                if entry.attempt_count >= self.max_attempts {
                    dead_letter_claimed_entry(
                        &self.mailbox_store,
                        entry_id,
                        &claim_token,
                        &format!("max attempts ({}) exceeded: {reason}", self.max_attempts),
                    )
                    .await?;
                    tracing::warn!(
                        entry_id,
                        attempts = entry.attempt_count,
                        "mailbox entry dead-lettered after max attempts"
                    );
                } else {
                    nack_claimed_entry(
                        &self.mailbox_store,
                        entry_id,
                        &claim_token,
                        self.retry_delay_ms,
                        &reason,
                    )
                    .await?;
                    tracing::debug!(entry_id, attempts = entry.attempt_count, %reason, "mailbox entry nacked for retry");
                }
            }
            ReceiveOutcome::Reject(reason) => {
                dead_letter_claimed_entry(&self.mailbox_store, entry_id, &claim_token, &reason)
                    .await?;
                tracing::warn!(entry_id, %reason, "mailbox entry rejected");
            }
        }
        Ok(())
    }

    pub async fn dispatch_ready_once(&self) -> Result<usize, ApiError> {
        let claimed = self
            .mailbox_store
            .claim_mailbox_entries(
                None,
                self.batch_size,
                &self.consumer_id,
                now_unix_millis(),
                self.lease_duration_ms,
            )
            .await
            .map_err(mailbox_error)?;

        let count = claimed.len();
        if count == 0 {
            return Ok(0);
        }

        let mut futures: FuturesUnordered<_> = claimed
            .into_iter()
            .map(|entry| self.dispatch_claimed_entry(entry))
            .collect();

        let mut first_error: Option<ApiError> = None;
        while let Some(result) = futures.next().await {
            if let Err(err) = result {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }

        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(count)
    }

    pub async fn run_forever(self) {
        let gc_interval = Duration::from_secs(DEFAULT_MAILBOX_GC_INTERVAL_SECS);
        let mut last_gc = std::time::Instant::now();

        loop {
            if let Err(err) = self.dispatch_ready_once().await {
                tracing::error!("mailbox dispatcher failed: {err}");
            }

            if last_gc.elapsed() >= gc_interval {
                let cutoff = now_unix_millis().saturating_sub(DEFAULT_MAILBOX_GC_TTL_MS);
                match self
                    .mailbox_store
                    .purge_terminal_mailbox_entries(cutoff)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(purged = n, "mailbox GC purged terminal entries"),
                    Err(err) => tracing::warn!("mailbox GC failed: {err}"),
                }
                last_gc = std::time::Instant::now();
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
    use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
    use tirea_agentos::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
    use tirea_agentos::contracts::{AgentBehavior, TerminationReason};
    use tirea_contract::storage::{
        MailboxEntryOrigin, MailboxReader, MailboxWriter, RunOrigin, RunReader, ThreadReader,
    };
    use tirea_contract::testing::MailboxEntryBuilder;
    use tirea_store_adapters::MemoryStore;

    struct TerminatePlugin;

    #[async_trait]
    impl AgentBehavior for TerminatePlugin {
        fn id(&self) -> &str {
            "mailbox_terminate"
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    fn make_os(store: Arc<MemoryStore>) -> Arc<AgentOs> {
        Arc::new(
            AgentOsBuilder::new()
                .with_registered_behavior("mailbox_terminate", Arc::new(TerminatePlugin))
                .with_agent_spec(AgentDefinitionSpec::local_with_id(
                    "test",
                    AgentDefinition {
                        id: "test".to_string(),
                        behavior_ids: vec!["mailbox_terminate".to_string()],
                        ..Default::default()
                    },
                ))
                .with_agent_state_store(store)
                .build()
                .expect("build AgentOs"),
        )
    }

    #[test]
    fn mailbox_entry_origin_follows_run_origin() {
        let mut request = RunRequest {
            agent_id: "test".to_string(),
            thread_id: Some("mailbox-origin-thread".to_string()),
            run_id: Some("mailbox-origin-run".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: RunOrigin::Internal,
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        };

        let internal = mailbox_entry_from_request(&request, 0, &EnqueueOptions::default(), 1);
        assert_eq!(internal.origin, MailboxEntryOrigin::Internal);

        request.origin = RunOrigin::AgUi;
        let external = mailbox_entry_from_request(&request, 0, &EnqueueOptions::default(), 1);
        assert_eq!(external.origin, MailboxEntryOrigin::External);
    }

    #[tokio::test]
    async fn dispatcher_accepts_enqueued_background_run() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (thread_id, run_id, _entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("mailbox-thread".to_string()),
                run_id: Some("mailbox-run".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue background run");
        assert_eq!(thread_id, "mailbox-thread");
        assert_eq!(run_id, "mailbox-run");

        let receiver: Arc<dyn MailboxReceiver> = Arc::new(AgentReceiver::new(os.clone()));
        MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch mailbox run");

        // The entry should be accepted
        let page = mailbox_store
            .list_mailbox_entries(&MailboxQuery {
                mailbox_id: Some("mailbox-thread".to_string()),
                status: Some(MailboxEntryStatus::Accepted),
                ..Default::default()
            })
            .await
            .expect("list mailbox entries");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].status, MailboxEntryStatus::Accepted);

        let run_record = RunReader::load_run(store.as_ref(), &run_id)
            .await
            .expect("load run record")
            .expect("run record should be persisted");
        assert_eq!(run_record.thread_id, thread_id);

        let thread = ThreadReader::load_thread(store.as_ref(), &thread_id)
            .await
            .expect("load thread")
            .expect("thread should exist");
        assert_eq!(thread.id, thread_id);
    }

    #[tokio::test]
    async fn dispatcher_skips_claimed_entry_after_interrupt_supersedes_it() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (_thread_id, _run_id, _entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("mailbox-supersede-thread".to_string()),
                run_id: Some("mailbox-supersede-run".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue background run");

        let claimed = mailbox_store
            .claim_mailbox_entries(None, 1, "test-dispatcher", now_unix_millis(), 5_000)
            .await
            .expect("claim mailbox entry");
        assert_eq!(claimed.len(), 1);

        mailbox_store
            .interrupt_mailbox("mailbox-supersede-thread", now_unix_millis())
            .await
            .expect("interrupt mailbox");

        let receiver: Arc<dyn MailboxReceiver> = Arc::new(AgentReceiver::new(os.clone()));
        let dispatcher = MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher");
        dispatcher
            .dispatch_claimed_entry(claimed.into_iter().next().expect("claimed entry"))
            .await
            .expect("dispatch after supersede");

        // The entry should be superseded
        let page = mailbox_store
            .list_mailbox_entries(&MailboxQuery {
                mailbox_id: Some("mailbox-supersede-thread".to_string()),
                ..Default::default()
            })
            .await
            .expect("list mailbox entries");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].status, MailboxEntryStatus::Superseded);
    }

    // -----------------------------------------------------------------------
    // AgentReceiver tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn agent_receiver_rejects_invalid_payload() {
        let store = Arc::new(MemoryStore::new());
        let os = make_os(store.clone());
        let receiver = AgentReceiver::new(os);

        let entry = MailboxEntryBuilder::queued("entry-bad-json", "mailbox-bad")
            .with_payload(serde_json::json!("not a valid RunRequest"))
            .claimed("token", "test", u64::MAX)
            .build();

        match receiver.receive(&entry).await {
            ReceiveOutcome::Reject(reason) => {
                assert!(reason.contains("invalid payload"), "got: {reason}");
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_receiver_rejects_unknown_agent() {
        let store = Arc::new(MemoryStore::new());
        let os = make_os(store.clone());
        let receiver = AgentReceiver::new(os);

        let request = RunRequest {
            agent_id: "nonexistent-agent".to_string(),
            thread_id: Some("thread-unknown".to_string()),
            run_id: Some("run-unknown".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: Default::default(),
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        };

        let entry = MailboxEntryBuilder::queued("entry-unknown-agent", "mailbox-unknown")
            .with_payload(serde_json::to_value(&request).unwrap())
            .claimed("token", "test", u64::MAX)
            .build();

        match receiver.receive(&entry).await {
            ReceiveOutcome::Reject(reason) => {
                assert!(
                    reason.contains("nonexistent-agent")
                        || reason.contains("not found")
                        || reason.contains("resolve"),
                    "expected resolve error, got: {reason}"
                );
            }
            other => panic!("expected Reject for unknown agent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_receiver_accepts_valid_request() {
        let store = Arc::new(MemoryStore::new());
        let os = make_os(store.clone());
        let receiver = AgentReceiver::new(os);

        let request = RunRequest {
            agent_id: "test".to_string(),
            thread_id: Some("thread-valid".to_string()),
            run_id: Some("run-valid".to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: Default::default(),
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        };

        let entry = MailboxEntryBuilder::queued("entry-valid", "thread-valid")
            .with_payload(serde_json::to_value(&request).unwrap())
            .claimed("token", "test", u64::MAX)
            .build();

        match receiver.receive(&entry).await {
            ReceiveOutcome::Accepted => {} // success
            other => panic!("expected Accepted, got {other:?}"),
        }

        // Give the spawned drain task a moment to finish
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Run should be persisted
        let run = RunReader::load_run(store.as_ref(), "run-valid")
            .await
            .expect("load run")
            .expect("run should be persisted");
        assert_eq!(run.thread_id, "thread-valid");
    }

    // -----------------------------------------------------------------------
    // MailboxDispatcher with mock receiver
    // -----------------------------------------------------------------------

    struct MockReceiver {
        outcome: std::sync::Mutex<Option<ReceiveOutcome>>,
    }

    impl MockReceiver {
        fn always(outcome: ReceiveOutcome) -> Self {
            Self {
                outcome: std::sync::Mutex::new(Some(outcome)),
            }
        }
    }

    #[async_trait]
    impl MailboxReceiver for MockReceiver {
        async fn receive(&self, _entry: &MailboxEntry) -> ReceiveOutcome {
            self.outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(ReceiveOutcome::Accepted)
        }
    }

    #[tokio::test]
    async fn dispatcher_reject_outcome_dead_letters_entry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        store.ensure_mailbox_state("mbx-reject", 1).await.unwrap();
        let entry = MailboxEntryBuilder::queued("entry-reject", "mbx-reject")
            .with_payload(serde_json::json!({"test": true}))
            .build();
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        let receiver: Arc<dyn MailboxReceiver> = Arc::new(MockReceiver::always(
            ReceiveOutcome::Reject("bad message".to_string()),
        ));
        MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");

        let loaded = store
            .load_mailbox_entry("entry-reject")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, MailboxEntryStatus::DeadLetter);
        assert_eq!(loaded.last_error.as_deref(), Some("bad message"));
    }

    #[tokio::test]
    async fn dispatcher_retry_outcome_nacks_entry_for_retry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        store.ensure_mailbox_state("mbx-retry", 1).await.unwrap();
        let entry = MailboxEntryBuilder::queued("entry-retry", "mbx-retry")
            .with_payload(serde_json::json!({"test": true}))
            .build();
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        let receiver: Arc<dyn MailboxReceiver> = Arc::new(MockReceiver::always(
            ReceiveOutcome::Retry("try later".to_string()),
        ));
        MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");

        let loaded = store
            .load_mailbox_entry("entry-retry")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, MailboxEntryStatus::Queued);
        assert_eq!(loaded.last_error.as_deref(), Some("try later"));
        assert_eq!(loaded.attempt_count, 1);
    }

    #[tokio::test]
    async fn dispatcher_retry_at_max_attempts_dead_letters() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        store.ensure_mailbox_state("mbx-max", 1).await.unwrap();
        let entry = MailboxEntryBuilder::queued("entry-max", "mbx-max")
            .with_payload(serde_json::json!({"test": true}))
            .with_attempt_count(10)
            .build(); // Already at max (DEFAULT_MAILBOX_MAX_ATTEMPTS = 10)
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        let receiver: Arc<dyn MailboxReceiver> = Arc::new(MockReceiver::always(
            ReceiveOutcome::Retry("still failing".to_string()),
        ));
        let dispatcher = MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher");
        dispatcher
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");

        let loaded = store
            .load_mailbox_entry("entry-max")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, MailboxEntryStatus::DeadLetter);
        assert!(
            loaded
                .last_error
                .as_deref()
                .unwrap()
                .contains("max attempts"),
            "error should mention max attempts: {:?}",
            loaded.last_error
        );
    }

    #[tokio::test]
    async fn dispatcher_generation_mismatch_supersedes_entry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        store.ensure_mailbox_state("mbx-genm", 1).await.unwrap();
        let entry = MailboxEntryBuilder::queued("entry-genm", "mbx-genm")
            .with_payload(serde_json::json!({"test": true}))
            .build();
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        // Claim the entry
        let claimed = store
            .claim_mailbox_entries(None, 1, "test-dispatcher", now_unix_millis(), 30_000)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        let claimed_entry = claimed.into_iter().next().unwrap();

        // Interrupt the mailbox to bump generation
        store
            .interrupt_mailbox("mbx-genm", now_unix_millis())
            .await
            .unwrap();

        // Dispatch the claimed entry — dispatcher should detect generation mismatch
        let receiver: Arc<dyn MailboxReceiver> =
            Arc::new(MockReceiver::always(ReceiveOutcome::Accepted));
        let dispatcher = MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher");
        dispatcher
            .dispatch_claimed_entry(claimed_entry)
            .await
            .expect("dispatch should succeed");

        let loaded = store
            .load_mailbox_entry("entry-genm")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, MailboxEntryStatus::Superseded);
    }

    #[tokio::test]
    async fn dispatcher_accept_outcome_acks_entry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        store.ensure_mailbox_state("mbx-ack", 1).await.unwrap();
        let entry = MailboxEntryBuilder::queued("entry-ack", "mbx-ack")
            .with_payload(serde_json::json!({"test": true}))
            .build();
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        let receiver: Arc<dyn MailboxReceiver> =
            Arc::new(MockReceiver::always(ReceiveOutcome::Accepted));
        MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");

        let loaded = store
            .load_mailbox_entry("entry-ack")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, MailboxEntryStatus::Accepted);
    }

    #[tokio::test]
    async fn dispatcher_empty_mailbox_returns_zero() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        let receiver: Arc<dyn MailboxReceiver> =
            Arc::new(MockReceiver::always(ReceiveOutcome::Accepted));
        let count = MailboxDispatcher::new(mailbox_store, receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn dispatcher_processes_multiple_entries_in_batch() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();

        // Use distinct mailboxes so each entry can be claimed concurrently
        // (exclusive claim limits one active claim per mailbox).
        for i in 0..5 {
            let mid = format!("mbx-batch-{i}");
            store.ensure_mailbox_state(&mid, 1).await.unwrap();
            let entry = MailboxEntryBuilder::queued(format!("entry-batch-{i}"), &mid)
                .with_payload(serde_json::json!({"test": true}))
                .build();
            store.enqueue_mailbox_entry(&entry).await.unwrap();
        }

        let receiver: Arc<dyn MailboxReceiver> =
            Arc::new(MockReceiver::always(ReceiveOutcome::Accepted));
        let count = MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch should succeed");
        assert_eq!(count, 5);

        // All entries should be accepted
        let page = mailbox_store
            .list_mailbox_entries(&MailboxQuery {
                status: Some(MailboxEntryStatus::Accepted),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 5);
    }

    // -----------------------------------------------------------------------
    // Service function tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn enqueue_background_run_returns_three_tuple() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (thread_id, run_id, entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-3tuple".to_string()),
                run_id: Some("run-3tuple".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue");

        assert_eq!(thread_id, "thread-3tuple");
        assert_eq!(run_id, "run-3tuple");
        assert!(!entry_id.is_empty());

        // Verify the entry exists in the store
        let entry = mailbox_store
            .load_mailbox_entry(&entry_id)
            .await
            .unwrap()
            .expect("entry should exist");
        assert_eq!(entry.mailbox_id, "thread-3tuple");
        assert_eq!(entry.status, MailboxEntryStatus::Queued);

        // Verify payload contains run_id
        assert_eq!(
            entry.payload.get("run_id").and_then(|v| v.as_str()),
            Some("run-3tuple")
        );
    }

    #[tokio::test]
    async fn enqueue_background_run_rejects_unknown_agent() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let result = enqueue_background_run(
            &os,
            &mailbox_store,
            "nonexistent",
            RunRequest {
                agent_id: "nonexistent".to_string(),
                thread_id: Some("thread-no-agent".to_string()),
                run_id: Some("run-no-agent".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn load_background_task_finds_by_entry_id() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (_thread_id, _run_id, entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-lookup".to_string()),
                run_id: Some("run-lookup".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue");

        // Lookup by entry_id returns mailbox entry (queued, no run record yet)
        let task = load_background_task(store.as_ref(), store.as_ref(), &entry_id)
            .await
            .expect("load background task");
        assert!(matches!(task, Some(BackgroundTaskLookup::Mailbox(_))));
    }

    #[tokio::test]
    async fn load_background_task_cross_references_accepted_entry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (thread_id, run_id, entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-xref".to_string()),
                run_id: Some("run-xref".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue");

        // Dispatch to accept and create run record
        let receiver: Arc<dyn MailboxReceiver> = Arc::new(AgentReceiver::new(os.clone()));
        MailboxDispatcher::new(mailbox_store.clone(), receiver)
            .with_consumer_id("test-xref")
            .dispatch_ready_once()
            .await
            .expect("dispatch");

        // Give drain task time to finish
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Look up by entry_id — should cross-reference to RunRecord
        let task = load_background_task(store.as_ref(), store.as_ref(), &entry_id)
            .await
            .expect("load background task");
        match task {
            Some(BackgroundTaskLookup::Run(record)) => {
                assert_eq!(record.run_id, run_id);
                assert_eq!(record.thread_id, thread_id);
            }
            other => panic!("expected Run variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_background_task_returns_none_for_unknown_id() {
        let store = Arc::new(MemoryStore::new());
        let task = load_background_task(store.as_ref(), store.as_ref(), "nonexistent")
            .await
            .expect("load background task");
        assert!(task.is_none());
    }

    #[tokio::test]
    async fn try_cancel_queued_entry_by_entry_id() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (_thread_id, _run_id, entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-cancel-q".to_string()),
                run_id: Some("run-cancel-q".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue");

        let result = try_cancel_active_or_queued_run_by_id(&os, &mailbox_store, &entry_id).await;
        assert!(matches!(
            result,
            Ok(Some(CancelBackgroundRunResult::Pending))
        ));

        let entry = store.load_mailbox_entry(&entry_id).await.unwrap().unwrap();
        assert_eq!(entry.status, MailboxEntryStatus::Cancelled);
    }

    #[tokio::test]
    async fn try_cancel_returns_none_for_unknown_id() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let result =
            try_cancel_active_or_queued_run_by_id(&os, &mailbox_store, "nonexistent").await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn cancel_pending_for_mailbox_excludes_specified_entry() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (_t1, _r1, entry1) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-cancel-pen".to_string()),
                run_id: Some("run-cancel-pen-1".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue 1");

        let (_t2, _r2, entry2) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: Some("thread-cancel-pen".to_string()),
                run_id: Some("run-cancel-pen-2".to_string()),
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue 2");

        // Cancel all pending except entry1
        let cancelled =
            cancel_pending_for_mailbox(&mailbox_store, "thread-cancel-pen", Some(&entry1))
                .await
                .expect("cancel pending");
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].entry_id, entry2);

        // entry1 still queued
        let e1 = store.load_mailbox_entry(&entry1).await.unwrap().unwrap();
        assert_eq!(e1.status, MailboxEntryStatus::Queued);

        // entry2 cancelled
        let e2 = store.load_mailbox_entry(&entry2).await.unwrap().unwrap();
        assert_eq!(e2.status, MailboxEntryStatus::Cancelled);
    }

    #[tokio::test]
    async fn enqueue_generates_ids_when_not_provided() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (thread_id, run_id, entry_id) = enqueue_background_run(
            &os,
            &mailbox_store,
            "test",
            RunRequest {
                agent_id: "test".to_string(),
                thread_id: None,
                run_id: None,
                parent_run_id: None,
                parent_thread_id: None,
                resource_id: None,
                origin: Default::default(),
                state: None,
                messages: vec![],
                initial_decisions: vec![],
                source_mailbox_entry_id: None,
            },
            EnqueueOptions::default(),
        )
        .await
        .expect("enqueue");

        assert!(!thread_id.is_empty());
        assert!(!run_id.is_empty());
        assert!(!entry_id.is_empty());
        // All three should be distinct
        assert_ne!(thread_id, run_id);
        assert_ne!(run_id, entry_id);
    }
}
