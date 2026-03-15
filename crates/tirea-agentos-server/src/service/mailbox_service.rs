use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tirea_agentos::contracts::storage::{
    MailboxEntry, MailboxEntryStatus, MailboxQuery, MailboxStore,
};
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::runtime::{AgentOs, RunStream};
use tirea_contract::storage::RunOrigin;

use super::mailbox::{
    ack_claimed_entry, build_parent_completion_notification_message, dead_letter_claimed_entry,
    drain_background_run, is_generation_mismatch, mailbox_entry_from_request, mailbox_error,
    normalize_background_run_request, now_unix_millis, parent_completion_notification_dedupe_key,
    start_agent_run_for_entry, DEFAULT_MAILBOX_LEASE_MS, INLINE_MAILBOX_AVAILABLE_AT,
};
use super::mailbox::{EnqueueOptions, MailboxRunStartError};
use super::ApiError;

/// Interval between lease renewal heartbeats.
const LEASE_RENEWAL_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn a background task that periodically extends a mailbox entry's lease.
///
/// Returns a [`tokio::task::JoinHandle`] that should be aborted when the run
/// completes (the handle is cancel-safe).
fn spawn_lease_renewal(
    store: Arc<dyn MailboxStore>,
    entry_id: String,
    claim_token: String,
    lease_ms: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LEASE_RENEWAL_INTERVAL);
        // Skip the first immediate tick.
        interval.tick().await;
        loop {
            interval.tick().await;
            let now = now_unix_millis();
            match store
                .extend_lease(&entry_id, &claim_token, lease_ms, now)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(entry_id, "lease renewal: entry no longer claimed, stopping");
                    break;
                }
                Err(err) => {
                    tracing::warn!(entry_id, %err, "lease renewal failed");
                    break;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Control signals
// ---------------------------------------------------------------------------

/// Control signal for a thread's mailbox.
#[derive(Debug, Clone)]
pub enum ControlSignal {
    /// Cancel the active run but keep pending entries.
    Cancel,
    /// Cancel the active run, clear all pending entries, and bump generation.
    Interrupt,
}

// ---------------------------------------------------------------------------
// Per-thread state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct BufferedEntry {
    entry: MailboxEntry,
}

enum ThreadStatus {
    Idle,
    Running {
        run_id: String,
        entry_id: String,
        claim_token: String,
    },
}

struct MailboxInner {
    status: ThreadStatus,
    generation: u64,
    pending: VecDeque<BufferedEntry>,
    /// Background task that extends the mailbox entry lease while a run is active.
    lease_renewal: Option<tokio::task::JoinHandle<()>>,
}

struct ThreadMailbox {
    thread_id: String,
    inner: tokio::sync::Mutex<MailboxInner>,
}

impl ThreadMailbox {
    fn new(thread_id: String, generation: u64) -> Self {
        Self {
            thread_id,
            inner: tokio::sync::Mutex::new(MailboxInner {
                status: ThreadStatus::Idle,
                generation,
                pending: VecDeque::new(),
                lease_renewal: None,
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// MailboxService
// ---------------------------------------------------------------------------

/// Event-driven mailbox service that replaces polling-based dispatch.
///
/// Each thread gets a `ThreadMailbox` that tracks whether it's idle or running.
/// When a message arrives and the thread is idle, dispatch happens immediately
/// (zero-latency). When the thread is busy, messages are buffered and dispatched
/// automatically on run completion.
pub struct MailboxService {
    os: Arc<AgentOs>,
    mailbox_store: Arc<dyn MailboxStore>,
    consumer_id: String,
    mailboxes: tokio::sync::RwLock<HashMap<String, Arc<ThreadMailbox>>>,
}

impl MailboxService {
    pub fn new(
        os: Arc<AgentOs>,
        mailbox_store: Arc<dyn MailboxStore>,
        consumer_id: impl Into<String>,
    ) -> Self {
        Self {
            os,
            mailbox_store,
            consumer_id: consumer_id.into(),
            mailboxes: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Returns a reference to the underlying mailbox store.
    pub fn mailbox_store(&self) -> &Arc<dyn MailboxStore> {
        &self.mailbox_store
    }

    /// Get or create the per-thread mailbox.
    async fn get_or_create_mailbox(
        self: &Arc<Self>,
        thread_id: &str,
        generation: u64,
    ) -> Arc<ThreadMailbox> {
        // Fast path: read lock
        {
            let map = self.mailboxes.read().await;
            if let Some(mb) = map.get(thread_id) {
                return mb.clone();
            }
        }
        // Slow path: write lock
        let mut map = self.mailboxes.write().await;
        map.entry(thread_id.to_string())
            .or_insert_with(|| Arc::new(ThreadMailbox::new(thread_id.to_string(), generation)))
            .clone()
    }

    /// Persist a mailbox entry to the store, handling generation retries.
    async fn enqueue_to_store(
        &self,
        request: &RunRequest,
        options: &EnqueueOptions,
        available_at: u64,
    ) -> Result<MailboxEntry, ApiError> {
        let mailbox_id = request
            .thread_id
            .as_ref()
            .expect("normalized request should have thread_id");

        for _ in 0..2 {
            let now = now_unix_millis();
            let state = self
                .mailbox_store
                .ensure_mailbox_state(mailbox_id, now)
                .await
                .map_err(mailbox_error)?;
            let entry = mailbox_entry_from_request(
                request,
                state.current_generation,
                options,
                available_at,
            );
            match self.mailbox_store.enqueue_mailbox_entry(&entry).await {
                Ok(()) => return Ok(entry),
                Err(tirea_agentos::contracts::storage::MailboxStoreError::AlreadyExists(_))
                    if options.dedupe_key.is_some() =>
                {
                    let dedupe_key = options
                        .dedupe_key
                        .as_deref()
                        .expect("guarded by options.dedupe_key.is_some()");
                    if let Some(existing) = self
                        .find_mailbox_entry_by_dedupe_key(mailbox_id, dedupe_key)
                        .await?
                    {
                        return Ok(existing);
                    }
                    return Err(ApiError::Internal(format!(
                        "mailbox dedupe collision for '{mailbox_id}' and key '{dedupe_key}' but existing entry was not found"
                    )));
                }
                Err(err) if is_generation_mismatch(&err) => continue,
                Err(err) => return Err(mailbox_error(err)),
            }
        }

        Err(ApiError::Internal(format!(
            "mailbox enqueue raced with interrupt for mailbox '{mailbox_id}'"
        )))
    }

    async fn find_mailbox_entry_by_dedupe_key(
        &self,
        mailbox_id: &str,
        dedupe_key: &str,
    ) -> Result<Option<MailboxEntry>, ApiError> {
        let mut offset = 0;
        loop {
            let page = self
                .mailbox_store
                .list_mailbox_entries(&MailboxQuery {
                    mailbox_id: Some(mailbox_id.to_string()),
                    offset,
                    limit: 200,
                    ..Default::default()
                })
                .await
                .map_err(mailbox_error)?;
            if let Some(entry) = page
                .items
                .into_iter()
                .find(|entry| entry.dedupe_key.as_deref() == Some(dedupe_key))
            {
                return Ok(Some(entry));
            }
            if !page.has_more {
                return Ok(None);
            }
            offset += 200;
        }
    }

    /// Submit a background run request. Returns (thread_id, run_id, entry_id).
    ///
    /// If the thread is idle, the run is dispatched immediately.
    /// If the thread is busy, the entry is buffered for later dispatch.
    pub async fn submit(
        self: &Arc<Self>,
        agent_id: &str,
        request: RunRequest,
        options: EnqueueOptions,
    ) -> Result<(String, String, String), ApiError> {
        self.os
            .resolve(agent_id)
            .map_err(|e| ApiError::from(tirea_agentos::runtime::AgentOsRunError::from(e)))?;

        let request = normalize_background_run_request(agent_id, request);
        let thread_id = request
            .thread_id
            .clone()
            .expect("normalized request should have thread_id");
        let run_id = request
            .run_id
            .clone()
            .expect("normalized request should have run_id");

        // WAL: persist to store first
        let entry = self
            .enqueue_to_store(&request, &options, now_unix_millis())
            .await?;
        let entry_id = entry.entry_id.clone();
        let generation = entry.generation;

        let mailbox = self.get_or_create_mailbox(&thread_id, generation).await;
        let mut inner = mailbox.inner.lock().await;

        // Update generation if store is ahead
        if generation > inner.generation {
            inner.generation = generation;
        }

        match &inner.status {
            ThreadStatus::Idle => {
                drop(inner);
                self.dispatch_entry(mailbox, entry).await;
            }
            ThreadStatus::Running { .. } => {
                inner.pending.push_back(BufferedEntry { entry });
            }
        }

        Ok((thread_id, run_id, entry_id))
    }

    /// Submit a streaming run request. Returns a RunStream for SSE consumption.
    ///
    /// This bypasses the buffering path — the entry is claimed inline and
    /// the run stream is returned directly. The stream is wrapped so that
    /// on_run_complete fires when it exhausts.
    pub async fn submit_streaming(
        self: &Arc<Self>,
        agent_id: &str,
        request: RunRequest,
        options: EnqueueOptions,
    ) -> Result<RunStream, ApiError> {
        self.os
            .resolve(agent_id)
            .map_err(|e| ApiError::from(tirea_agentos::runtime::AgentOsRunError::from(e)))?;

        let request = normalize_background_run_request(agent_id, request);
        let thread_id = request
            .thread_id
            .clone()
            .expect("normalized request should have thread_id");

        // WAL: persist with sentinel available_at so the sweep doesn't grab it
        let entry = self
            .enqueue_to_store(&request, &options, INLINE_MAILBOX_AVAILABLE_AT)
            .await?;
        let entry_id = entry.entry_id.clone();
        let generation = entry.generation;

        // Claim the entry inline
        let Some(claimed) = self
            .mailbox_store
            .claim_mailbox_entry(
                &entry_id,
                &self.consumer_id,
                now_unix_millis(),
                DEFAULT_MAILBOX_LEASE_MS,
            )
            .await
            .map_err(mailbox_error)?
        else {
            return Err(ApiError::Internal(format!(
                "mailbox entry '{entry_id}' could not be claimed for streaming"
            )));
        };

        let claim_token = claimed.claim_token.clone().ok_or_else(|| {
            ApiError::Internal(format!(
                "mailbox entry '{entry_id}' was claimed without claim_token"
            ))
        })?;

        // Register the thread mailbox as Running before starting
        let mailbox = self.get_or_create_mailbox(&thread_id, generation).await;
        let renewal = spawn_lease_renewal(
            self.mailbox_store.clone(),
            entry_id.clone(),
            claim_token.clone(),
            DEFAULT_MAILBOX_LEASE_MS,
        );
        {
            let mut inner = mailbox.inner.lock().await;
            if generation > inner.generation {
                inner.generation = generation;
            }
            inner.status = ThreadStatus::Running {
                run_id: request.run_id.clone().unwrap_or_default(),
                entry_id: entry_id.clone(),
                claim_token: claim_token.clone(),
            };
            inner.lease_renewal = Some(renewal);
        }

        match start_agent_run_for_entry(&self.os, &self.mailbox_store, &claimed, false).await {
            Ok(run) => {
                // Defer ack until stream exhausts via wrap_with_completion.
                Ok(self.wrap_with_completion(thread_id, run))
            }
            Err(MailboxRunStartError::Superseded(error)) => {
                let _ = self
                    .mailbox_store
                    .supersede_mailbox_entry(&entry_id, now_unix_millis(), &error)
                    .await;
                // Reset to idle
                let mut inner = mailbox.inner.lock().await;
                inner.status = ThreadStatus::Idle;
                Err(ApiError::BadRequest(error))
            }
            Err(MailboxRunStartError::Busy(error)) => {
                self.mailbox_store
                    .cancel_mailbox_entry(&entry_id, now_unix_millis())
                    .await
                    .map_err(mailbox_error)?;
                let mut inner = mailbox.inner.lock().await;
                inner.status = ThreadStatus::Idle;
                Err(ApiError::BadRequest(error))
            }
            Err(MailboxRunStartError::Permanent(error))
            | Err(MailboxRunStartError::Retryable(error)) => {
                dead_letter_claimed_entry(&self.mailbox_store, &entry_id, &claim_token, &error)
                    .await?;
                let mut inner = mailbox.inner.lock().await;
                inner.status = ThreadStatus::Idle;
                Err(ApiError::Internal(error))
            }
            Err(MailboxRunStartError::Internal(error)) => {
                dead_letter_claimed_entry(
                    &self.mailbox_store,
                    &entry_id,
                    &claim_token,
                    &error.to_string(),
                )
                .await?;
                let mut inner = mailbox.inner.lock().await;
                inner.status = ThreadStatus::Idle;
                Err(error)
            }
        }
    }

    /// Dispatch a single entry on a thread. Claims, starts the run, and
    /// transitions the mailbox to Running.
    ///
    /// Returns a boxed future to break the recursive async type cycle
    /// (dispatch_entry → on_run_complete → dispatch_entry).
    fn dispatch_entry(
        self: &Arc<Self>,
        mailbox: Arc<ThreadMailbox>,
        entry: MailboxEntry,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let entry_id = entry.entry_id.clone();

            let claimed = match self
                .mailbox_store
                .claim_mailbox_entry(
                    &entry_id,
                    &self.consumer_id,
                    now_unix_millis(),
                    DEFAULT_MAILBOX_LEASE_MS,
                )
                .await
            {
                Ok(Some(c)) => c,
                Ok(None) => {
                    tracing::debug!(entry_id, "entry already claimed/consumed, skipping");
                    return;
                }
                Err(err) => {
                    tracing::error!(entry_id, %err, "failed to claim mailbox entry");
                    return;
                }
            };

            let claim_token = match claimed.claim_token.as_deref() {
                Some(t) => t.to_string(),
                None => {
                    tracing::error!(entry_id, "claimed entry missing claim_token");
                    return;
                }
            };

            // Check generation mismatch
            if self
                .mailbox_store
                .load_mailbox_state(&entry.mailbox_id)
                .await
                .ok()
                .flatten()
                .is_some_and(|state| state.current_generation != entry.generation)
            {
                let _ = self
                    .mailbox_store
                    .supersede_mailbox_entry(
                        &entry_id,
                        now_unix_millis(),
                        "superseded by interrupt",
                    )
                    .await;
                tracing::debug!(entry_id, "entry superseded by generation mismatch");
                return;
            }

            match start_agent_run_for_entry(&self.os, &self.mailbox_store, &claimed, true).await {
                Ok(run) => {
                    // Defer ack until run completes — the Claimed status with
                    // active lease serves as a distributed lock preventing
                    // concurrent runs on the same thread across nodes.
                    let renewal = spawn_lease_renewal(
                        self.mailbox_store.clone(),
                        entry_id.clone(),
                        claim_token.clone(),
                        DEFAULT_MAILBOX_LEASE_MS,
                    );

                    let run_id = run.run_id.clone();
                    let completed_run_id = run_id.clone();
                    let thread_id = mailbox.thread_id.clone();

                    {
                        let mut inner = mailbox.inner.lock().await;
                        inner.status = ThreadStatus::Running {
                            run_id,
                            entry_id: entry_id.clone(),
                            claim_token: claim_token.clone(),
                        };
                        inner.lease_renewal = Some(renewal);
                    }

                    // Spawn background drain with completion callback
                    let svc = self.clone();
                    tokio::spawn(async move {
                        drain_background_run(run).await;
                        svc.on_run_complete(&thread_id, &completed_run_id).await;
                    });
                }
                Err(MailboxRunStartError::Busy(reason)) => {
                    // Thread is busy (rare race). Buffer the entry for later.
                    let _ = super::mailbox::nack_claimed_entry(
                        &self.mailbox_store,
                        &entry_id,
                        &claim_token,
                        250,
                        &reason,
                    )
                    .await;
                    let mut inner = mailbox.inner.lock().await;
                    inner.pending.push_front(BufferedEntry { entry: claimed });
                    tracing::debug!(entry_id, %reason, "dispatch busy, buffered entry");
                }
                Err(MailboxRunStartError::Superseded(error)) => {
                    let _ = self
                        .mailbox_store
                        .supersede_mailbox_entry(&entry_id, now_unix_millis(), &error)
                        .await;
                    tracing::debug!(entry_id, %error, "entry superseded during dispatch");
                    // Try next pending
                    let svc = self.clone();
                    let tid = mailbox.thread_id.clone();
                    tokio::spawn(async move {
                        svc.try_dispatch_next(&tid).await;
                    });
                }
                Err(MailboxRunStartError::Permanent(error)) => {
                    let _ = dead_letter_claimed_entry(
                        &self.mailbox_store,
                        &entry_id,
                        &claim_token,
                        &error,
                    )
                    .await;
                    tracing::warn!(entry_id, %error, "permanent dispatch error");
                    let svc = self.clone();
                    let tid = mailbox.thread_id.clone();
                    tokio::spawn(async move {
                        svc.try_dispatch_next(&tid).await;
                    });
                }
                Err(MailboxRunStartError::Retryable(error)) => {
                    let _ = super::mailbox::nack_claimed_entry(
                        &self.mailbox_store,
                        &entry_id,
                        &claim_token,
                        250,
                        &error,
                    )
                    .await;
                    tracing::warn!(entry_id, %error, "retryable dispatch error");
                    // Re-buffer at front for next attempt
                    let mut inner = mailbox.inner.lock().await;
                    inner.pending.push_front(BufferedEntry { entry: claimed });
                }
                Err(MailboxRunStartError::Internal(error)) => {
                    let _ = dead_letter_claimed_entry(
                        &self.mailbox_store,
                        &entry_id,
                        &claim_token,
                        &error.to_string(),
                    )
                    .await;
                    tracing::error!(entry_id, %error, "internal dispatch error");
                    let svc = self.clone();
                    let tid = mailbox.thread_id.clone();
                    tokio::spawn(async move {
                        svc.try_dispatch_next(&tid).await;
                    });
                }
            }
        })
    }

    /// Wrap a RunStream so that `on_run_complete` fires when the stream exhausts.
    fn wrap_with_completion(self: &Arc<Self>, thread_id: String, run: RunStream) -> RunStream {
        let svc = self.clone();
        let tid = thread_id;
        let completed_run_id = run.run_id.clone();
        let wrapped = futures::stream::unfold(
            (run.events, Some(svc), Some(tid), Some(completed_run_id)),
            |(mut events, svc, tid, run_id)| async move {
                match events.next().await {
                    Some(event) => Some((event, (events, svc, tid, run_id))),
                    None => {
                        // Stream exhausted — fire completion
                        if let (Some(svc), Some(tid), Some(run_id)) = (svc, tid, run_id) {
                            svc.on_run_complete(&tid, &run_id).await;
                        }
                        None
                    }
                }
            },
        );
        RunStream {
            thread_id: run.thread_id,
            run_id: run.run_id,
            decision_tx: run.decision_tx,
            events: Box::pin(wrapped),
        }
    }

    /// Called when a run completes on a thread. Acks the mailbox entry,
    /// stops lease renewal, transitions to Idle, and dispatches the next
    /// pending entry.
    async fn on_run_complete(self: &Arc<Self>, thread_id: &str, run_id: &str) {
        if let Err(err) = self.submit_parent_completion_notification(run_id).await {
            tracing::warn!(thread_id, run_id, %err, "failed to submit parent completion notification");
        }

        let mailbox = {
            let map = self.mailboxes.read().await;
            match map.get(thread_id) {
                Some(mb) => mb.clone(),
                None => return,
            }
        };

        let next = {
            let mut inner = mailbox.inner.lock().await;

            // Abort lease renewal and ack the completed entry.
            if let Some(handle) = inner.lease_renewal.take() {
                handle.abort();
            }
            if let ThreadStatus::Running {
                entry_id,
                claim_token,
                ..
            } = &inner.status
            {
                if let Err(err) =
                    ack_claimed_entry(&self.mailbox_store, entry_id, claim_token).await
                {
                    tracing::error!(entry_id, %err, "failed to ack entry on run completion");
                }
            }

            inner.status = ThreadStatus::Idle;
            inner.pending.pop_front()
        };

        if let Some(buffered) = next {
            let svc = self.clone();
            tokio::spawn(async move {
                svc.dispatch_entry(mailbox, buffered.entry).await;
            });
        }
    }

    async fn submit_parent_completion_notification(
        self: &Arc<Self>,
        completed_run_id: &str,
    ) -> Result<(), ApiError> {
        let Some(store) = self.os.agent_state_store().cloned() else {
            return Ok(());
        };
        let Some(child_record) = store
            .load_run(completed_run_id)
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))?
        else {
            return Ok(());
        };
        if child_record.source_mailbox_entry_id.is_none() {
            return Ok(());
        }
        let Some(parent_run_id) = child_record.parent_run_id.clone() else {
            return Ok(());
        };
        let Some(message) = build_parent_completion_notification_message(&child_record) else {
            return Ok(());
        };
        let Some(parent_record) = store
            .load_run(&parent_run_id)
            .await
            .map_err(|err| ApiError::Internal(err.to_string()))?
        else {
            return Err(ApiError::Internal(format!(
                "parent run '{parent_run_id}' not found for completed child run '{completed_run_id}'"
            )));
        };
        let agent_id = parent_record.agent_id.trim();
        if agent_id.is_empty() {
            return Err(ApiError::Internal(format!(
                "parent run '{parent_run_id}' is missing agent_id"
            )));
        }

        let request = RunRequest {
            agent_id: agent_id.to_string(),
            thread_id: Some(parent_record.thread_id.clone()),
            run_id: None,
            parent_run_id: None,
            parent_thread_id: parent_record.parent_thread_id.clone(),
            resource_id: None,
            origin: RunOrigin::Internal,
            state: None,
            messages: vec![message],
            initial_decisions: Vec::new(),
            source_mailbox_entry_id: None,
        };
        let options = EnqueueOptions {
            sender_id: Some(format!("run:{completed_run_id}")),
            priority: 0,
            dedupe_key: Some(parent_completion_notification_dedupe_key(
                &parent_run_id,
                completed_run_id,
            )),
        };
        self.submit(agent_id, request, options).await?;
        Ok(())
    }

    /// Try to dispatch the next pending entry for a thread (if idle).
    async fn try_dispatch_next(self: &Arc<Self>, thread_id: &str) {
        let mailbox = {
            let map = self.mailboxes.read().await;
            match map.get(thread_id) {
                Some(mb) => mb.clone(),
                None => return,
            }
        };

        let next = {
            let mut inner = mailbox.inner.lock().await;
            if !matches!(inner.status, ThreadStatus::Idle) {
                return;
            }
            inner.pending.pop_front()
        };

        if let Some(buffered) = next {
            let svc = self.clone();
            tokio::spawn(async move {
                svc.dispatch_entry(mailbox, buffered.entry).await;
            });
        }
    }

    /// Send a control signal to a thread's mailbox.
    pub async fn control(
        self: &Arc<Self>,
        thread_id: &str,
        signal: ControlSignal,
    ) -> Result<ControlResult, ApiError> {
        match signal {
            ControlSignal::Cancel => {
                let cancelled_run_id = self.os.cancel_active_run_by_thread(thread_id).await;
                Ok(ControlResult {
                    cancelled_run_id,
                    generation: None,
                    superseded_entries: vec![],
                })
            }
            ControlSignal::Interrupt => {
                // Interrupt the store (bumps generation, supersedes pending)
                let interrupted = self
                    .mailbox_store
                    .interrupt_mailbox(thread_id, now_unix_millis())
                    .await
                    .map_err(mailbox_error)?;

                // Cancel active run
                let cancelled_run_id = self.os.cancel_active_run_by_thread(thread_id).await;

                let new_generation = interrupted.mailbox_state.current_generation;

                // Clear in-memory pending and update generation
                let mailbox = {
                    let map = self.mailboxes.read().await;
                    map.get(thread_id).cloned()
                };
                if let Some(mb) = mailbox {
                    let mut inner = mb.inner.lock().await;
                    inner.pending.clear();
                    inner.generation = new_generation;
                    // If there's no active run to complete, mark idle
                    if cancelled_run_id.is_none() {
                        if let Some(handle) = inner.lease_renewal.take() {
                            handle.abort();
                        }
                        inner.status = ThreadStatus::Idle;
                    }
                }

                Ok(ControlResult {
                    cancelled_run_id,
                    generation: Some(new_generation),
                    superseded_entries: interrupted.superseded_entries,
                })
            }
        }
    }

    /// Recover mailbox state from the persistent store on startup.
    ///
    /// Loads all queued entries and buffers them in the appropriate ThreadMailbox.
    pub async fn recover(self: &Arc<Self>) -> Result<usize, ApiError> {
        let page = self
            .mailbox_store
            .list_mailbox_entries(&MailboxQuery {
                status: Some(MailboxEntryStatus::Queued),
                limit: 10_000,
                ..Default::default()
            })
            .await
            .map_err(mailbox_error)?;

        let mut recovered = 0;
        for entry in page.items {
            let thread_id = entry.mailbox_id.clone();
            let generation = entry.generation;

            // Skip entries with sentinel available_at (inline streaming claims)
            if entry.available_at == INLINE_MAILBOX_AVAILABLE_AT {
                continue;
            }

            let mailbox = self.get_or_create_mailbox(&thread_id, generation).await;
            let mut inner = mailbox.inner.lock().await;

            if generation > inner.generation {
                inner.generation = generation;
            }

            match &inner.status {
                ThreadStatus::Idle if inner.pending.is_empty() => {
                    // Dispatch immediately
                    drop(inner);
                    self.dispatch_entry(mailbox, entry).await;
                }
                _ => {
                    inner.pending.push_back(BufferedEntry { entry });
                }
            }
            recovered += 1;
        }

        if recovered > 0 {
            tracing::info!(recovered, "recovered mailbox entries from store");
        }
        Ok(recovered)
    }

    /// Background sweep that picks up orphaned entries.
    ///
    /// Runs at a low frequency (30s) as a safety net. Most dispatch happens
    /// via the event-driven path.
    pub async fn run_sweep_forever(self: Arc<Self>) {
        let sweep_interval = Duration::from_secs(30);
        let gc_interval = Duration::from_secs(60);
        let gc_ttl_ms: u64 = 24 * 60 * 60 * 1000;
        let mut last_gc = std::time::Instant::now();

        loop {
            tokio::time::sleep(sweep_interval).await;

            // Sweep: claim and dispatch any orphaned queued entries
            match self
                .mailbox_store
                .claim_mailbox_entries(
                    None,
                    16,
                    &self.consumer_id,
                    now_unix_millis(),
                    DEFAULT_MAILBOX_LEASE_MS,
                )
                .await
            {
                Ok(claimed) if !claimed.is_empty() => {
                    tracing::info!(count = claimed.len(), "sweep picked up orphaned entries");
                    for entry in claimed {
                        let thread_id = entry.mailbox_id.clone();
                        let generation = entry.generation;
                        let mailbox = self.get_or_create_mailbox(&thread_id, generation).await;
                        let is_idle = {
                            let inner = mailbox.inner.lock().await;
                            matches!(inner.status, ThreadStatus::Idle)
                        };
                        if is_idle {
                            // Already claimed, dispatch directly
                            let claim_token = entry.claim_token.clone().unwrap_or_default();
                            match start_agent_run_for_entry(
                                &self.os,
                                &self.mailbox_store,
                                &entry,
                                true,
                            )
                            .await
                            {
                                Ok(run) => {
                                    let renewal = spawn_lease_renewal(
                                        self.mailbox_store.clone(),
                                        entry.entry_id.clone(),
                                        claim_token.clone(),
                                        DEFAULT_MAILBOX_LEASE_MS,
                                    );
                                    let run_id = run.run_id.clone();
                                    let completed_run_id = run_id.clone();
                                    let entry_id = entry.entry_id.clone();
                                    {
                                        let mut inner = mailbox.inner.lock().await;
                                        inner.status = ThreadStatus::Running {
                                            run_id,
                                            entry_id,
                                            claim_token,
                                        };
                                        inner.lease_renewal = Some(renewal);
                                    }
                                    let svc = self.clone();
                                    let tid = thread_id.clone();
                                    tokio::spawn(async move {
                                        drain_background_run(run).await;
                                        svc.on_run_complete(&tid, &completed_run_id).await;
                                    });
                                }
                                Err(MailboxRunStartError::Busy(_)) => {
                                    let _ = super::mailbox::nack_claimed_entry(
                                        &self.mailbox_store,
                                        &entry.entry_id,
                                        &claim_token,
                                        250,
                                        "thread busy during sweep",
                                    )
                                    .await;
                                }
                                Err(MailboxRunStartError::Permanent(error)) => {
                                    let _ = dead_letter_claimed_entry(
                                        &self.mailbox_store,
                                        &entry.entry_id,
                                        &claim_token,
                                        &error,
                                    )
                                    .await;
                                }
                                Err(_) => {
                                    let _ = super::mailbox::nack_claimed_entry(
                                        &self.mailbox_store,
                                        &entry.entry_id,
                                        &claim_token,
                                        1000,
                                        "sweep dispatch failed",
                                    )
                                    .await;
                                }
                            }
                        } else {
                            // Thread is busy, nack for later
                            let claim_token = entry.claim_token.clone().unwrap_or_default();
                            let _ = super::mailbox::nack_claimed_entry(
                                &self.mailbox_store,
                                &entry.entry_id,
                                &claim_token,
                                1000,
                                "thread busy during sweep",
                            )
                            .await;
                        }
                    }
                }
                Ok(_) => {} // nothing to sweep
                Err(err) => {
                    tracing::error!(%err, "sweep failed to claim entries");
                }
            }

            // Periodic GC of terminal entries
            if last_gc.elapsed() >= gc_interval {
                let cutoff = now_unix_millis().saturating_sub(gc_ttl_ms);
                match self
                    .mailbox_store
                    .purge_terminal_mailbox_entries(cutoff)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(purged = n, "mailbox GC purged terminal entries"),
                    Err(err) => tracing::warn!(%err, "mailbox GC failed"),
                }
                last_gc = std::time::Instant::now();
            }
        }
    }
}

/// Result of a control signal.
pub struct ControlResult {
    pub cancelled_run_id: Option<String>,
    pub generation: Option<u64>,
    pub superseded_entries: Vec<MailboxEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tirea_agentos::composition::{AgentDefinition, AgentDefinitionSpec, AgentOsBuilder};
    use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
    use tirea_agentos::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
    use tirea_agentos::contracts::storage::{MailboxStore, ThreadReader, ThreadWriter};
    use tirea_agentos::contracts::{AgentBehavior, TerminationReason};
    use tirea_contract::storage::{
        MailboxWriter, RunOrigin, RunQuery, RunReader, RunStatus, RunWriter,
    };
    use tirea_store_adapters::MemoryStore;

    struct TerminatePlugin;

    #[async_trait]
    impl AgentBehavior for TerminatePlugin {
        fn id(&self) -> &str {
            "svc_terminate"
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

    struct DelayedTerminatePlugin {
        id: &'static str,
        delay_ms: u64,
    }

    #[async_trait]
    impl AgentBehavior for DelayedTerminatePlugin {
        fn id(&self) -> &str {
            self.id
        }

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            ActionSet::single(BeforeInferenceAction::Terminate(
                TerminationReason::BehaviorRequested,
            ))
        }
    }

    fn make_os_with_agents(store: Arc<MemoryStore>, agent_ids: &[&str]) -> Arc<AgentOs> {
        let mut builder = AgentOsBuilder::new()
            .with_registered_behavior("svc_terminate", Arc::new(TerminatePlugin))
            .with_agent_state_store(store);
        for agent_id in agent_ids {
            builder = builder.with_agent_spec(AgentDefinitionSpec::local_with_id(
                *agent_id,
                AgentDefinition {
                    id: (*agent_id).to_string(),
                    behavior_ids: vec!["svc_terminate".to_string()],
                    ..Default::default()
                },
            ));
        }
        Arc::new(builder.build().expect("build AgentOs"))
    }

    fn make_os(store: Arc<MemoryStore>) -> Arc<AgentOs> {
        make_os_with_agents(store, &["test"])
    }

    fn make_request(thread_id: &str, run_id: &str) -> RunRequest {
        RunRequest {
            agent_id: "test".to_string(),
            thread_id: Some(thread_id.to_string()),
            run_id: Some(run_id.to_string()),
            parent_run_id: None,
            parent_thread_id: None,
            resource_id: None,
            origin: Default::default(),
            state: None,
            messages: vec![],
            initial_decisions: vec![],
            source_mailbox_entry_id: None,
        }
    }

    fn completion_message_for_parent(
        messages: &[std::sync::Arc<tirea_agentos::contracts::thread::Message>],
        parent_run_id: &str,
    ) -> Option<serde_json::Value> {
        completion_messages_for_parent(messages, parent_run_id)
            .into_iter()
            .next()
    }

    fn completion_messages_for_parent(
        messages: &[std::sync::Arc<tirea_agentos::contracts::thread::Message>],
        parent_run_id: &str,
    ) -> Vec<serde_json::Value> {
        messages
            .iter()
            .map(std::sync::Arc::as_ref)
            .filter_map(|message| {
                let parsed: serde_json::Value = serde_json::from_str(&message.content).ok()?;
                if parsed["type"].as_str() == Some("background_task_notification")
                    && parsed["recipient_task_id"].as_str() == Some(parent_run_id)
                {
                    Some(parsed)
                } else {
                    None
                }
            })
            .collect()
    }

    async fn seed_completed_run(
        store: &MemoryStore,
        run_id: &str,
        thread_id: &str,
        agent_id: &str,
        origin: RunOrigin,
        parent_thread_id: Option<&str>,
    ) {
        let now = now_unix_millis();
        let mut record = tirea_contract::storage::RunRecord::new(
            run_id.to_string(),
            thread_id.to_string(),
            agent_id.to_string(),
            origin,
            RunStatus::Done,
            now,
        );
        record.parent_thread_id = parent_thread_id.map(ToString::to_string);
        record.termination_code = Some("natural".to_string());
        record.updated_at = now;
        store.upsert_run(&record).await.expect("seed completed run");
    }

    async fn seed_child_terminal_run(
        store: &MemoryStore,
        run_id: &str,
        thread_id: &str,
        agent_id: &str,
        parent_run_id: &str,
        source_mailbox_entry_id: Option<&str>,
    ) {
        seed_child_terminal_run_with_status(
            store,
            run_id,
            thread_id,
            agent_id,
            parent_run_id,
            source_mailbox_entry_id,
            Some("natural"),
            None,
        )
        .await;
    }

    async fn seed_child_terminal_run_with_status(
        store: &MemoryStore,
        run_id: &str,
        thread_id: &str,
        agent_id: &str,
        parent_run_id: &str,
        source_mailbox_entry_id: Option<&str>,
        termination_code: Option<&str>,
        termination_detail: Option<&str>,
    ) {
        let now = now_unix_millis();
        let mut record = tirea_contract::storage::RunRecord::new(
            run_id.to_string(),
            thread_id.to_string(),
            agent_id.to_string(),
            RunOrigin::User,
            RunStatus::Done,
            now,
        );
        record.parent_run_id = Some(parent_run_id.to_string());
        record.source_mailbox_entry_id = source_mailbox_entry_id.map(ToString::to_string);
        record.termination_code = termination_code.map(ToString::to_string);
        record.termination_detail = termination_detail.map(ToString::to_string);
        record.updated_at = now;
        store.upsert_run(&record).await.expect("seed child run");
    }

    async fn wait_for_completion_messages(
        store: &MemoryStore,
        thread_id: &str,
        parent_run_id: &str,
        expected: usize,
    ) -> Vec<serde_json::Value> {
        for _ in 0..40 {
            if let Some(thread) = store
                .load_thread(thread_id)
                .await
                .expect("load thread should succeed")
            {
                let messages = completion_messages_for_parent(&thread.messages, parent_run_id);
                if messages.len() >= expected {
                    return messages;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        store
            .load_thread(thread_id)
            .await
            .expect("load thread should succeed")
            .map(|thread| completion_messages_for_parent(&thread.messages, parent_run_id))
            .unwrap_or_default()
    }

    async fn wait_for_run_record(
        store: &MemoryStore,
        run_id: &str,
    ) -> tirea_contract::storage::RunRecord {
        for _ in 0..40 {
            if let Some(record) = RunReader::load_run(store, run_id)
                .await
                .expect("load run should succeed")
            {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        RunReader::load_run(store, run_id)
            .await
            .expect("load run should succeed")
            .unwrap_or_else(|| panic!("timed out waiting for run '{run_id}'"))
    }

    #[tokio::test]
    async fn submit_dispatches_immediately_when_idle() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let (thread_id, run_id, entry_id) = svc
            .submit(
                "test",
                make_request("svc-thread-1", "svc-run-1"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit");

        assert_eq!(thread_id, "svc-thread-1");
        assert_eq!(run_id, "svc-run-1");
        assert!(!entry_id.is_empty());

        // Give the spawned drain task time to finish
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Entry should be accepted
        let loaded = mailbox_store
            .load_mailbox_entry(&entry_id)
            .await
            .unwrap()
            .expect("entry should exist");
        assert_eq!(loaded.status, MailboxEntryStatus::Accepted);
    }

    #[tokio::test]
    async fn submit_buffers_when_busy_then_dispatches_on_completion() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        // Submit first run
        let (_t1, _r1, entry1) = svc
            .submit(
                "test",
                make_request("svc-thread-2", "svc-run-2a"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit 1");

        // Submit second run while first is running (should buffer)
        let (_t2, _r2, entry2) = svc
            .submit(
                "test",
                make_request("svc-thread-2", "svc-run-2b"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit 2");

        // Give time for both runs to complete
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Both entries should be accepted
        let e1 = mailbox_store
            .load_mailbox_entry(&entry1)
            .await
            .unwrap()
            .expect("entry1");
        assert_eq!(e1.status, MailboxEntryStatus::Accepted);

        let e2 = mailbox_store
            .load_mailbox_entry(&entry2)
            .await
            .unwrap()
            .expect("entry2");
        assert_eq!(e2.status, MailboxEntryStatus::Accepted);
    }

    #[tokio::test]
    async fn control_interrupt_clears_pending() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        // Submit first (will run)
        let _ = svc
            .submit(
                "test",
                make_request("svc-thread-3", "svc-run-3a"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit 1");

        // Submit second (will buffer)
        let (_, _, entry2) = svc
            .submit(
                "test",
                make_request("svc-thread-3", "svc-run-3b"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit 2");

        // Interrupt
        let result = svc
            .control("svc-thread-3", ControlSignal::Interrupt)
            .await
            .expect("interrupt");
        assert!(result.generation.is_some());

        // Give time for completion
        tokio::time::sleep(Duration::from_millis(300)).await;

        // The second entry should be superseded (interrupt supersedes queued entries)
        let e2 = mailbox_store
            .load_mailbox_entry(&entry2)
            .await
            .unwrap()
            .expect("entry2");
        assert_eq!(e2.status, MailboxEntryStatus::Superseded);
    }

    #[tokio::test]
    async fn submit_rejects_unknown_agent() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let result = svc
            .submit(
                "nonexistent",
                make_request("svc-thread-4", "svc-run-4"),
                EnqueueOptions::default(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn recover_loads_queued_entries() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        // Pre-populate store with a queued entry
        let now = now_unix_millis();
        store
            .ensure_mailbox_state("svc-thread-5", now)
            .await
            .unwrap();
        let entry = mailbox_entry_from_request(
            &make_request("svc-thread-5", "svc-run-5"),
            0,
            &EnqueueOptions::default(),
            now,
        );
        store.enqueue_mailbox_entry(&entry).await.unwrap();

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let count = svc.recover().await.expect("recover");
        assert_eq!(count, 1);

        // Give time for dispatch
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Entry should be accepted
        let loaded = mailbox_store
            .load_mailbox_entry(&entry.entry_id)
            .await
            .unwrap()
            .expect("entry");
        assert_eq!(loaded.status, MailboxEntryStatus::Accepted);
    }

    #[tokio::test]
    async fn submit_streaming_returns_stream() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let mut run = svc
            .submit_streaming(
                "test",
                make_request("svc-thread-6", "svc-run-6"),
                EnqueueOptions::default(),
            )
            .await
            .expect("submit_streaming");

        // Drain the stream
        while run.events.next().await.is_some() {}

        // Give completion callback time
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Check mailbox state is idle after stream exhaustion
        let map = svc.mailboxes.read().await;
        if let Some(mb) = map.get("svc-thread-6") {
            let inner = mb.inner.lock().await;
            assert!(
                matches!(inner.status, ThreadStatus::Idle),
                "expected idle after stream exhaustion"
            );
        }
    }

    #[tokio::test]
    async fn completed_background_run_notifies_parent_task_on_same_thread() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let parent_run_id = "svc-parent-run-1";
        seed_completed_run(
            store.as_ref(),
            parent_run_id,
            "svc-thread-parent-1",
            "test",
            RunOrigin::User,
            None,
        )
        .await;
        let mut request = make_request("svc-thread-parent-1", "svc-child-run-1");
        request.parent_run_id = Some(parent_run_id.to_string());

        let (_thread_id, run_id, entry_id) = svc
            .submit("test", request, EnqueueOptions::default())
            .await
            .expect("submit");

        tokio::time::sleep(Duration::from_millis(400)).await;

        let thread = store
            .load_thread("svc-thread-parent-1")
            .await
            .unwrap()
            .expect("thread should exist");
        let notification = completion_message_for_parent(&thread.messages, parent_run_id)
            .expect("expected parent completion notification");
        assert_eq!(
            notification["child_task_id"].as_str(),
            Some(entry_id.as_str())
        );
        assert_eq!(notification["child_run_id"].as_str(), Some(run_id.as_str()));
        assert_eq!(notification["status"].as_str(), Some("completed"));

        let internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-thread-parent-1".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list internal runs");
        assert!(
            !internal_runs.items.is_empty(),
            "expected completion notification to execute via internal run"
        );
    }

    #[tokio::test]
    async fn completed_background_run_notifies_parent_task_on_parent_thread() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        store
            .save(&tirea_agentos::contracts::thread::Thread::new(
                "svc-parent-thread-2",
            ))
            .await
            .expect("seed parent thread");

        let parent_run_id = "svc-parent-run-2";
        seed_completed_run(
            store.as_ref(),
            parent_run_id,
            "svc-parent-thread-2",
            "test",
            RunOrigin::User,
            None,
        )
        .await;
        let mut request = make_request("svc-child-thread-2", "svc-child-run-2");
        request.parent_run_id = Some(parent_run_id.to_string());
        request.parent_thread_id = Some("svc-parent-thread-2".to_string());

        let (_thread_id, run_id, _entry_id) = svc
            .submit("test", request, EnqueueOptions::default())
            .await
            .expect("submit");

        tokio::time::sleep(Duration::from_millis(400)).await;

        let parent_thread = store
            .load_thread("svc-parent-thread-2")
            .await
            .unwrap()
            .expect("parent thread should exist");
        let notification = completion_message_for_parent(&parent_thread.messages, parent_run_id)
            .expect("expected parent-thread completion notification");
        assert_eq!(
            notification["child_thread_id"].as_str(),
            Some("svc-child-thread-2")
        );
        assert_eq!(notification["child_run_id"].as_str(), Some(run_id.as_str()));

        let child_thread = store
            .load_thread("svc-child-thread-2")
            .await
            .unwrap()
            .expect("child thread should exist");
        assert!(
            completion_message_for_parent(&child_thread.messages, parent_run_id).is_none(),
            "notification should not be appended to child thread when parent_thread_id is set"
        );

        let internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-parent-thread-2".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list internal runs");
        assert!(
            !internal_runs.items.is_empty(),
            "expected completion notification to execute on the parent thread"
        );
    }

    #[tokio::test]
    async fn completed_foreground_child_run_does_not_notify_parent_task() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os_with_agents(store.clone(), &["parent-agent", "worker-agent"]);
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        store
            .save(&tirea_agentos::contracts::thread::Thread::new(
                "svc-parent-thread-foreground",
            ))
            .await
            .expect("seed parent thread");
        seed_completed_run(
            store.as_ref(),
            "svc-parent-run-foreground",
            "svc-parent-thread-foreground",
            "parent-agent",
            RunOrigin::User,
            None,
        )
        .await;
        seed_child_terminal_run(
            store.as_ref(),
            "svc-child-run-foreground",
            "svc-child-thread-foreground",
            "worker-agent",
            "svc-parent-run-foreground",
            None,
        )
        .await;

        svc.submit_parent_completion_notification("svc-child-run-foreground")
            .await
            .expect("foreground notification path should no-op");
        tokio::time::sleep(Duration::from_millis(250)).await;

        let parent_thread = store
            .load_thread("svc-parent-thread-foreground")
            .await
            .unwrap()
            .expect("parent thread should exist");
        assert!(
            completion_message_for_parent(&parent_thread.messages, "svc-parent-run-foreground")
                .is_none(),
            "foreground child run should not enqueue parent completion notification"
        );

        let internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-parent-thread-foreground".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list internal runs");
        assert!(
            internal_runs.items.is_empty(),
            "foreground child run should not spawn internal notification runs"
        );
    }

    #[tokio::test]
    async fn multi_agent_background_notification_runs_under_parent_agent() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os_with_agents(store.clone(), &["parent-agent", "worker-agent"]);
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        store
            .save(&tirea_agentos::contracts::thread::Thread::new(
                "svc-parent-thread-multi-agent",
            ))
            .await
            .expect("seed parent thread");
        seed_completed_run(
            store.as_ref(),
            "svc-parent-run-multi-agent",
            "svc-parent-thread-multi-agent",
            "parent-agent",
            RunOrigin::User,
            None,
        )
        .await;

        let mut request = RunRequest {
            agent_id: "worker-agent".to_string(),
            ..make_request("svc-child-thread-multi-agent", "svc-child-run-multi-agent")
        };
        request.parent_run_id = Some("svc-parent-run-multi-agent".to_string());
        request.parent_thread_id = Some("svc-parent-thread-multi-agent".to_string());

        let (_thread_id, run_id, entry_id) = svc
            .submit("worker-agent", request, EnqueueOptions::default())
            .await
            .expect("submit child background run");

        tokio::time::sleep(Duration::from_millis(450)).await;

        let parent_thread = store
            .load_thread("svc-parent-thread-multi-agent")
            .await
            .unwrap()
            .expect("parent thread should exist");
        let notification =
            completion_message_for_parent(&parent_thread.messages, "svc-parent-run-multi-agent")
                .expect("expected multi-agent completion notification");
        assert_eq!(
            notification["child_task_id"].as_str(),
            Some(entry_id.as_str())
        );
        assert_eq!(notification["child_run_id"].as_str(), Some(run_id.as_str()));
        assert_eq!(notification["status"].as_str(), Some("completed"));

        let internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-parent-thread-multi-agent".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list internal runs");
        assert!(
            internal_runs
                .items
                .iter()
                .any(|record| record.agent_id == "parent-agent"),
            "expected notification run to execute under the parent agent"
        );
        assert!(
            internal_runs
                .items
                .iter()
                .all(|record| record.agent_id != "worker-agent"),
            "notification run should not execute under the child agent"
        );
    }

    #[tokio::test]
    async fn parent_completion_notification_maps_terminal_statuses() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os_with_agents(store.clone(), &["parent-agent", "worker-agent"]);
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let parent_run_id = "svc-parent-run-status-map";
        seed_completed_run(
            store.as_ref(),
            parent_run_id,
            "svc-parent-thread-status-map",
            "parent-agent",
            RunOrigin::User,
            None,
        )
        .await;

        let cases = [
            (
                "svc-child-run-failed",
                Some("error"),
                Some("worker crashed"),
                "failed",
            ),
            (
                "svc-child-run-cancelled",
                Some("cancelled"),
                Some("user requested stop"),
                "cancelled",
            ),
            (
                "svc-child-run-stopped",
                Some("stopped:max_turns"),
                Some("max turns reached"),
                "stopped",
            ),
        ];

        for (idx, (run_id, termination_code, termination_detail, _expected_status)) in
            cases.iter().enumerate()
        {
            seed_child_terminal_run_with_status(
                store.as_ref(),
                run_id,
                &format!("svc-child-thread-status-{idx}"),
                "worker-agent",
                parent_run_id,
                Some(&format!("svc-child-entry-status-{idx}")),
                *termination_code,
                *termination_detail,
            )
            .await;

            svc.submit_parent_completion_notification(run_id)
                .await
                .expect("submit parent completion notification");
        }

        let notifications = wait_for_completion_messages(
            store.as_ref(),
            "svc-parent-thread-status-map",
            parent_run_id,
            3,
        )
        .await;
        assert_eq!(
            notifications.len(),
            3,
            "expected three completion notifications"
        );

        let by_run_id: std::collections::HashMap<_, _> = notifications
            .into_iter()
            .map(|notification| {
                (
                    notification["child_run_id"]
                        .as_str()
                        .expect("child_run_id should be present")
                        .to_string(),
                    notification,
                )
            })
            .collect();

        for (run_id, termination_code, termination_detail, expected_status) in cases {
            let notification = by_run_id
                .get(run_id)
                .unwrap_or_else(|| panic!("missing notification for {run_id}"));
            assert_eq!(notification["status"].as_str(), Some(expected_status));
            assert_eq!(notification["termination_code"].as_str(), termination_code,);
            assert_eq!(
                notification["termination_detail"].as_str(),
                termination_detail,
            );
        }
    }

    #[tokio::test]
    async fn parent_completion_notification_is_idempotent_for_repeated_completion_callbacks() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os_with_agents(store.clone(), &["parent-agent", "worker-agent"]);
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let parent_run_id = "svc-parent-run-idempotent";
        let child_run_id = "svc-child-run-idempotent";
        seed_completed_run(
            store.as_ref(),
            parent_run_id,
            "svc-parent-thread-idempotent",
            "parent-agent",
            RunOrigin::User,
            None,
        )
        .await;
        seed_child_terminal_run(
            store.as_ref(),
            child_run_id,
            "svc-child-thread-idempotent",
            "worker-agent",
            parent_run_id,
            Some("svc-child-entry-idempotent"),
        )
        .await;

        svc.submit_parent_completion_notification(child_run_id)
            .await
            .expect("first completion callback");
        svc.submit_parent_completion_notification(child_run_id)
            .await
            .expect("duplicate completion callback should no-op");
        svc.on_run_complete("svc-child-thread-idempotent", child_run_id)
            .await;

        let notifications = wait_for_completion_messages(
            store.as_ref(),
            "svc-parent-thread-idempotent",
            parent_run_id,
            1,
        )
        .await;
        assert_eq!(
            notifications.len(),
            1,
            "duplicate completion callbacks must not duplicate notifications"
        );

        let internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-parent-thread-idempotent".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list internal runs");
        assert_eq!(
            internal_runs.items.len(),
            1,
            "idempotent delivery should create exactly one internal notification run"
        );
    }

    #[tokio::test]
    async fn parent_busy_thread_buffers_multiple_child_completion_notifications() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = Arc::new(
            AgentOsBuilder::new()
                .with_registered_behavior("svc_terminate", Arc::new(TerminatePlugin))
                .with_registered_behavior(
                    "svc_parent_busy",
                    Arc::new(DelayedTerminatePlugin {
                        id: "svc_parent_busy",
                        delay_ms: 350,
                    }),
                )
                .with_registered_behavior(
                    "svc_worker_slow",
                    Arc::new(DelayedTerminatePlugin {
                        id: "svc_worker_slow",
                        delay_ms: 150,
                    }),
                )
                .with_agent_state_store(store.clone())
                .with_agent_spec(AgentDefinitionSpec::local_with_id(
                    "parent-agent",
                    AgentDefinition {
                        id: "parent-agent".to_string(),
                        behavior_ids: vec!["svc_parent_busy".to_string()],
                        ..Default::default()
                    },
                ))
                .with_agent_spec(AgentDefinitionSpec::local_with_id(
                    "worker-fast",
                    AgentDefinition {
                        id: "worker-fast".to_string(),
                        behavior_ids: vec!["svc_terminate".to_string()],
                        ..Default::default()
                    },
                ))
                .with_agent_spec(AgentDefinitionSpec::local_with_id(
                    "worker-slow",
                    AgentDefinition {
                        id: "worker-slow".to_string(),
                        behavior_ids: vec!["svc_worker_slow".to_string()],
                        ..Default::default()
                    },
                ))
                .build()
                .expect("build AgentOs"),
        );
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        let parent_thread_id = "svc-parent-thread-busy";
        let parent_run_id = "svc-parent-run-busy";
        let parent_request = RunRequest {
            agent_id: "parent-agent".to_string(),
            ..make_request(parent_thread_id, parent_run_id)
        };
        svc.submit("parent-agent", parent_request, EnqueueOptions::default())
            .await
            .expect("submit parent background run");
        let _parent_run = wait_for_run_record(store.as_ref(), parent_run_id).await;

        let mut slow_request = RunRequest {
            agent_id: "worker-slow".to_string(),
            ..make_request("svc-child-thread-busy-slow", "svc-child-run-busy-slow")
        };
        slow_request.parent_run_id = Some(parent_run_id.to_string());
        slow_request.parent_thread_id = Some(parent_thread_id.to_string());
        svc.submit("worker-slow", slow_request, EnqueueOptions::default())
            .await
            .expect("submit slow child");

        let mut fast_request = RunRequest {
            agent_id: "worker-fast".to_string(),
            ..make_request("svc-child-thread-busy-fast", "svc-child-run-busy-fast")
        };
        fast_request.parent_run_id = Some(parent_run_id.to_string());
        fast_request.parent_thread_id = Some(parent_thread_id.to_string());
        svc.submit("worker-fast", fast_request, EnqueueOptions::default())
            .await
            .expect("submit fast child");

        tokio::time::sleep(Duration::from_millis(225)).await;
        let pending_while_busy = store
            .load_thread(parent_thread_id)
            .await
            .expect("load parent thread should succeed")
            .map(|thread| completion_messages_for_parent(&thread.messages, parent_run_id))
            .unwrap_or_default();
        assert!(
            pending_while_busy.is_empty(),
            "busy parent thread should not receive completion messages before its active run finishes"
        );

        let notifications =
            wait_for_completion_messages(store.as_ref(), parent_thread_id, parent_run_id, 2).await;
        assert_eq!(
            notifications.len(),
            2,
            "both child completions should be delivered after the parent thread becomes idle"
        );
        let child_run_ids: Vec<_> = notifications
            .iter()
            .map(|notification| {
                notification["child_run_id"]
                    .as_str()
                    .expect("child_run_id should be present")
                    .to_string()
            })
            .collect();
        assert_eq!(
            child_run_ids,
            vec![
                "svc-child-run-busy-fast".to_string(),
                "svc-child-run-busy-slow".to_string(),
            ],
            "completion notifications should follow child completion order while buffering on the parent thread"
        );
    }

    #[tokio::test]
    async fn grandchild_completion_notifies_immediate_parent_not_root_parent() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os_with_agents(
            store.clone(),
            &["root-agent", "child-agent", "worker-agent"],
        );
        let svc = Arc::new(MailboxService::new(
            os.clone(),
            mailbox_store.clone(),
            "test-svc",
        ));

        seed_completed_run(
            store.as_ref(),
            "svc-root-run-nested",
            "svc-root-thread-nested",
            "root-agent",
            RunOrigin::User,
            None,
        )
        .await;
        seed_child_terminal_run(
            store.as_ref(),
            "svc-child-run-nested",
            "svc-child-thread-nested",
            "child-agent",
            "svc-root-run-nested",
            Some("svc-child-entry-nested"),
        )
        .await;
        seed_child_terminal_run(
            store.as_ref(),
            "svc-grandchild-run-nested",
            "svc-grandchild-thread-nested",
            "worker-agent",
            "svc-child-run-nested",
            Some("svc-grandchild-entry-nested"),
        )
        .await;

        svc.submit_parent_completion_notification("svc-grandchild-run-nested")
            .await
            .expect("submit grandchild completion notification");

        let child_notifications = wait_for_completion_messages(
            store.as_ref(),
            "svc-child-thread-nested",
            "svc-child-run-nested",
            1,
        )
        .await;
        assert_eq!(child_notifications.len(), 1);
        assert_eq!(
            child_notifications[0]["child_run_id"].as_str(),
            Some("svc-grandchild-run-nested")
        );

        let root_notifications = wait_for_completion_messages(
            store.as_ref(),
            "svc-root-thread-nested",
            "svc-root-run-nested",
            1,
        )
        .await;
        assert!(
            root_notifications.is_empty(),
            "grandchild completion should notify the immediate parent task, not the root parent"
        );

        let child_internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-child-thread-nested".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list child internal runs");
        assert!(
            child_internal_runs
                .items
                .iter()
                .any(|record| record.agent_id == "child-agent"),
            "nested completion notification should execute under the immediate parent agent"
        );

        let root_internal_runs = RunReader::list_runs(
            store.as_ref(),
            &RunQuery {
                thread_id: Some("svc-root-thread-nested".to_string()),
                origin: Some(RunOrigin::Internal),
                ..Default::default()
            },
        )
        .await
        .expect("list root internal runs");
        assert!(
            root_internal_runs.items.is_empty(),
            "nested completion should not spawn a root-level notification run"
        );
    }
}
