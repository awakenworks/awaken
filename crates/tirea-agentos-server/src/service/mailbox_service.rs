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

use super::mailbox::{
    ack_claimed_entry, dead_letter_claimed_entry, drain_background_run,
    is_generation_mismatch, mailbox_entry_from_request,
    mailbox_error, normalize_background_run_request, now_unix_millis, start_agent_run_for_entry,
    DEFAULT_MAILBOX_LEASE_MS, INLINE_MAILBOX_AVAILABLE_AT,
};
use super::mailbox::{EnqueueOptions, MailboxRunStartError};
use super::ApiError;

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
        _run_id: String,
        _entry_id: String,
    },
}

struct MailboxInner {
    status: ThreadStatus,
    generation: u64,
    pending: VecDeque<BufferedEntry>,
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
                Err(err) if is_generation_mismatch(&err) => continue,
                Err(err) => return Err(mailbox_error(err)),
            }
        }

        Err(ApiError::Internal(format!(
            "mailbox enqueue raced with interrupt for mailbox '{mailbox_id}'"
        )))
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
        {
            let mut inner = mailbox.inner.lock().await;
            if generation > inner.generation {
                inner.generation = generation;
            }
            inner.status = ThreadStatus::Running {
                _run_id: request
                    .run_id
                    .clone()
                    .unwrap_or_default(),
                _entry_id: entry_id.clone(),
            };
        }

        match start_agent_run_for_entry(&self.os, &self.mailbox_store, &claimed, false).await {
            Ok(run) => {
                ack_claimed_entry(&self.mailbox_store, &entry_id, &claim_token).await?;
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
                .supersede_mailbox_entry(&entry_id, now_unix_millis(), "superseded by interrupt")
                .await;
            tracing::debug!(entry_id, "entry superseded by generation mismatch");
            return;
        }

        match start_agent_run_for_entry(&self.os, &self.mailbox_store, &claimed, true).await {
            Ok(run) => {
                if let Err(err) =
                    ack_claimed_entry(&self.mailbox_store, &entry_id, &claim_token).await
                {
                    tracing::error!(entry_id, %err, "failed to ack entry");
                }

                let run_id = run.run_id.clone();
                let thread_id = mailbox.thread_id.clone();

                {
                    let mut inner = mailbox.inner.lock().await;
                    inner.status = ThreadStatus::Running {
                        _run_id: run_id,
                        _entry_id: entry_id.clone(),
                    };
                }

                // Spawn background drain with completion callback
                let svc = self.clone();
                tokio::spawn(async move {
                    drain_background_run(run).await;
                    svc.on_run_complete(&thread_id).await;
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
                inner
                    .pending
                    .push_front(BufferedEntry { entry: claimed });
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
                inner
                    .pending
                    .push_front(BufferedEntry { entry: claimed });
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
        let wrapped = futures::stream::unfold(
            (run.events, Some(svc), Some(tid)),
            |(mut events, svc, tid)| async move {
                match events.next().await {
                    Some(event) => Some((event, (events, svc, tid))),
                    None => {
                        // Stream exhausted — fire completion
                        if let (Some(svc), Some(tid)) = (svc, tid) {
                            svc.on_run_complete(&tid).await;
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

    /// Called when a run completes on a thread. Transitions to Idle and
    /// spawns dispatch of the next pending entry.
    async fn on_run_complete(self: &Arc<Self>, thread_id: &str) {
        let mailbox = {
            let map = self.mailboxes.read().await;
            match map.get(thread_id) {
                Some(mb) => mb.clone(),
                None => return,
            }
        };

        let next = {
            let mut inner = mailbox.inner.lock().await;
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
                        let mailbox =
                            self.get_or_create_mailbox(&thread_id, generation).await;
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
                                    let _ = ack_claimed_entry(
                                        &self.mailbox_store,
                                        &entry.entry_id,
                                        &claim_token,
                                    )
                                    .await;
                                    let run_id = run.run_id.clone();
                                    let entry_id = entry.entry_id.clone();
                                    {
                                        let mut inner = mailbox.inner.lock().await;
                                        inner.status = ThreadStatus::Running {
                                            _run_id: run_id,
                                            _entry_id: entry_id,
                                        };
                                    }
                                    let svc = self.clone();
                                    let tid = thread_id.clone();
                                    tokio::spawn(async move {
                                        drain_background_run(run).await;
                                        svc.on_run_complete(&tid).await;
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
    use tirea_agentos::composition::{AgentDefinition, AgentOsBuilder};
    use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
    use tirea_agentos::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
    use tirea_agentos::contracts::{AgentBehavior, TerminationReason};
    use tirea_agentos::contracts::storage::MailboxStore;
    use tirea_contract::storage::MailboxWriter;
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

    fn make_os(store: Arc<MemoryStore>) -> Arc<AgentOs> {
        Arc::new(
            AgentOsBuilder::new()
                .with_registered_behavior("svc_terminate", Arc::new(TerminatePlugin))
                .with_agent(
                    "test",
                    AgentDefinition {
                        id: "test".to_string(),
                        behavior_ids: vec!["svc_terminate".to_string()],
                        ..Default::default()
                    },
                )
                .with_agent_state_store(store)
                .build()
                .expect("build AgentOs"),
        )
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
        store.ensure_mailbox_state("svc-thread-5", now).await.unwrap();
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
}
