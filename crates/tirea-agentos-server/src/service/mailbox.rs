use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tirea_agentos::contracts::storage::{
    MailboxEntry, MailboxEntryStatus, MailboxQuery, MailboxReader, MailboxStore, MailboxStoreError,
    ThreadReader,
};
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::{AgentOs, AgentOsRunError, RunStream};
use tirea_contract::storage::RunRecord;

use super::ApiError;

const DEFAULT_MAILBOX_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_MAILBOX_LEASE_MS: u64 = 30_000;
const DEFAULT_MAILBOX_RETRY_MS: u64 = 250;
const DEFAULT_MAILBOX_BATCH_SIZE: usize = 16;
const DEFAULT_MAILBOX_MAX_ATTEMPTS: u32 = 10;
const DEFAULT_MAILBOX_GC_INTERVAL_SECS: u64 = 60;
const DEFAULT_MAILBOX_GC_TTL_MS: u64 = 24 * 60 * 60 * 1000; // 24 hours
const INLINE_MAILBOX_AVAILABLE_AT: u64 = i64::MAX as u64;

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn new_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

fn normalize_background_run_request(agent_id: &str, mut request: RunRequest) -> RunRequest {
    request.agent_id = agent_id.to_string();
    if request.thread_id.is_none() {
        request.thread_id = Some(new_id());
    }
    if request.run_id.is_none() {
        request.run_id = Some(new_id());
    }
    request
}

fn mailbox_entry_from_request(
    request: RunRequest,
    generation: u64,
    dedupe_key: Option<String>,
    available_at: u64,
) -> MailboxEntry {
    let now = now_unix_millis();
    MailboxEntry {
        entry_id: new_id(),
        thread_id: request
            .thread_id
            .clone()
            .expect("background mailbox request should have thread_id"),
        run_id: request
            .run_id
            .clone()
            .expect("background mailbox request should have run_id"),
        agent_id: request.agent_id.clone(),
        generation,
        status: MailboxEntryStatus::Queued,
        request,
        dedupe_key,
        available_at,
        attempt_count: 0,
        last_error: None,
        claim_token: None,
        claimed_by: None,
        lease_until: None,
        accepted_run_id: None,
        created_at: now,
        updated_at: now,
    }
}

fn mailbox_error(err: MailboxStoreError) -> ApiError {
    ApiError::Internal(err.to_string())
}

fn is_generation_mismatch(err: &MailboxStoreError) -> bool {
    matches!(err, MailboxStoreError::GenerationMismatch { .. })
}

fn is_permanent_dispatch_error(err: &AgentOsRunError) -> bool {
    matches!(err, AgentOsRunError::Resolve(_))
}

async fn drain_background_run(mut run: tirea_agentos::RunStream) {
    while run.events.next().await.is_some() {}
}

async fn ack_claimed_entry(
    mailbox_store: &Arc<dyn MailboxStore>,
    entry_id: &str,
    claim_token: &str,
    accepted_run_id: &str,
) -> Result<(), ApiError> {
    match mailbox_store
        .ack_mailbox_entry(entry_id, claim_token, accepted_run_id, now_unix_millis())
        .await
    {
        Ok(()) => Ok(()),
        Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
        Err(err) => Err(mailbox_error(err)),
    }
}

async fn nack_claimed_entry(
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

async fn dead_letter_claimed_entry(
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

enum MailboxRunStartError {
    Busy(String),
    Superseded(String),
    Permanent(String),
    Retryable(String),
    Internal(ApiError),
}

async fn start_run_for_claimed_entry(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    entry: &MailboxEntry,
    persist_run: bool,
) -> Result<RunStream, MailboxRunStartError> {
    if let Some(current_entry) = mailbox_store
        .load_mailbox_entry(&entry.entry_id)
        .await
        .map_err(mailbox_error)
        .map_err(MailboxRunStartError::Internal)?
    {
        if current_entry.status != MailboxEntryStatus::Claimed
            || current_entry.claim_token != entry.claim_token
        {
            return Err(MailboxRunStartError::Superseded(
                current_entry
                    .last_error
                    .unwrap_or_else(|| "mailbox entry is no longer active".to_string()),
            ));
        }
    }

    if mailbox_store
        .load_mailbox_thread_state(&entry.thread_id)
        .await
        .map_err(mailbox_error)
        .map_err(MailboxRunStartError::Internal)?
        .is_some_and(|state| state.current_generation != entry.generation)
    {
        return Err(MailboxRunStartError::Superseded(
            "mailbox entry superseded by interrupt".to_string(),
        ));
    }

    match os
        .current_run_id_for_thread(&entry.agent_id, &entry.thread_id)
        .await
    {
        Ok(Some(run_id)) if run_id != entry.run_id => {
            return Err(MailboxRunStartError::Busy(
                "thread already has an active run".to_string(),
            ));
        }
        Ok(_) => {}
        Err(err) => return Err(MailboxRunStartError::Internal(ApiError::from(err))),
    }

    let resolved = os
        .resolve(&entry.agent_id)
        .map_err(|err| MailboxRunStartError::Permanent(err.to_string()))?;

    let mut request = entry.request.clone();
    request.source_mailbox_entry_id = Some(entry.entry_id.clone());

    os.start_active_run_with_persistence(
        &entry.agent_id,
        request,
        resolved,
        persist_run,
        !persist_run,
    )
    .await
    .map_err(|err| {
        if is_permanent_dispatch_error(&err) {
            MailboxRunStartError::Permanent(err.to_string())
        } else {
            MailboxRunStartError::Retryable(err.to_string())
        }
    })
}

async fn enqueue_mailbox_run(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
    available_at: u64,
) -> Result<(String, String), ApiError> {
    os.resolve(agent_id).map_err(AgentOsRunError::from)?;

    let request = normalize_background_run_request(agent_id, request);
    let thread_id = request
        .thread_id
        .clone()
        .expect("normalized mailbox run request should have thread_id");
    let run_id = request
        .run_id
        .clone()
        .expect("normalized mailbox run request should have run_id");

    for _ in 0..2 {
        let now = now_unix_millis();
        let thread_state = mailbox_store
            .ensure_mailbox_thread_state(&thread_id, now)
            .await
            .map_err(mailbox_error)?;
        let entry = mailbox_entry_from_request(
            request.clone(),
            thread_state.current_generation,
            None,
            available_at,
        );

        match mailbox_store.enqueue_mailbox_entry(&entry).await {
            Ok(()) => return Ok((thread_id.clone(), run_id.clone())),
            Err(MailboxStoreError::AlreadyExists(_)) => {
                let existing = mailbox_store
                    .load_mailbox_entry_by_run_id(&run_id)
                    .await
                    .map_err(mailbox_error)?
                    .ok_or_else(|| {
                        ApiError::Internal(format!(
                            "mailbox enqueue reported duplicate run '{run_id}' but no entry exists"
                        ))
                    })?;
                return Ok((existing.thread_id, existing.run_id));
            }
            Err(err) if is_generation_mismatch(&err) => continue,
            Err(err) => return Err(mailbox_error(err)),
        }
    }

    Err(ApiError::Internal(format!(
        "mailbox enqueue raced with interrupt for thread '{thread_id}'"
    )))
}

pub fn require_mailbox_store(state: &super::AppState) -> Result<Arc<dyn MailboxStore>, ApiError> {
    state
        .mailbox_store
        .clone()
        .ok_or_else(|| ApiError::Internal("mailbox store not configured".to_string()))
}

pub async fn enqueue_background_run(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
) -> Result<(String, String), ApiError> {
    enqueue_mailbox_run(os, mailbox_store, agent_id, request, now_unix_millis()).await
}

pub async fn start_streaming_run_via_mailbox(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    agent_id: &str,
    request: RunRequest,
    consumer_id: &str,
) -> Result<RunStream, ApiError> {
    let (_thread_id, run_id) = enqueue_mailbox_run(
        os,
        mailbox_store,
        agent_id,
        request,
        INLINE_MAILBOX_AVAILABLE_AT,
    )
    .await?;

    let Some(entry) = mailbox_store
        .claim_mailbox_entry_by_run_id(
            &run_id,
            consumer_id,
            now_unix_millis(),
            DEFAULT_MAILBOX_LEASE_MS,
        )
        .await
        .map_err(mailbox_error)?
    else {
        let existing = mailbox_store
            .load_mailbox_entry_by_run_id(&run_id)
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
            Some(_) => ApiError::BadRequest("run is already claimed".to_string()),
            None => ApiError::Internal(format!(
                "mailbox entry for run '{run_id}' disappeared before inline dispatch"
            )),
        });
    };

    let claim_token = entry.claim_token.clone().ok_or_else(|| {
        ApiError::Internal(format!(
            "mailbox entry '{}' was claimed without claim_token",
            entry.entry_id
        ))
    })?;

    match start_run_for_claimed_entry(os, mailbox_store, &entry, false).await {
        Ok(run) => {
            let accepted_run_id = run.run_id.clone();
            ack_claimed_entry(
                mailbox_store,
                &entry.entry_id,
                &claim_token,
                &accepted_run_id,
            )
            .await?;
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
                .cancel_mailbox_entry_by_run_id(&entry.run_id, now_unix_millis())
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

pub async fn cancel_pending_mailbox_for_thread(
    mailbox_store: &Arc<dyn MailboxStore>,
    thread_id: &str,
    exclude_run_id: Option<&str>,
) -> Result<Vec<MailboxEntry>, ApiError> {
    mailbox_store
        .cancel_pending_mailbox_for_thread(thread_id, now_unix_millis(), exclude_run_id)
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
        .interrupt_mailbox_thread(thread_id, now_unix_millis())
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
                    thread_id: Some(thread_id.to_string()),
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
        generation: interrupted.thread_state.current_generation,
        superseded_entries: interrupted.superseded_entries,
    })
}

pub enum BackgroundTaskLookup {
    Run(RunRecord),
    Mailbox(MailboxEntry),
}

pub async fn load_background_task(
    read_store: &dyn ThreadReader,
    mailbox_store: &dyn MailboxReader,
    run_id: &str,
) -> Result<Option<BackgroundTaskLookup>, ApiError> {
    if let Some(record) = read_store
        .load_run(run_id)
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
    {
        return Ok(Some(BackgroundTaskLookup::Run(record)));
    }

    mailbox_store
        .load_mailbox_entry_by_run_id(run_id)
        .await
        .map(|maybe| maybe.map(BackgroundTaskLookup::Mailbox))
        .map_err(mailbox_error)
}

pub enum CancelBackgroundRunResult {
    Active,
    Pending,
}

pub async fn try_cancel_active_or_queued_run_by_id(
    os: &Arc<AgentOs>,
    mailbox_store: &Arc<dyn MailboxStore>,
    run_id: &str,
) -> Result<Option<CancelBackgroundRunResult>, ApiError> {
    if os.cancel_active_run_by_id(run_id).await {
        return Ok(Some(CancelBackgroundRunResult::Active));
    }

    let cancelled = mailbox_store
        .cancel_mailbox_entry_by_run_id(run_id, now_unix_millis())
        .await
        .map_err(mailbox_error)?;
    if cancelled
        .as_ref()
        .is_some_and(|entry| entry.status == MailboxEntryStatus::Cancelled)
    {
        return Ok(Some(CancelBackgroundRunResult::Pending));
    }
    Ok(None)
}

#[derive(Clone)]
pub struct MailboxDispatcher {
    os: Arc<AgentOs>,
    mailbox_store: Arc<dyn MailboxStore>,
    consumer_id: String,
    poll_interval: Duration,
    lease_duration_ms: u64,
    retry_delay_ms: u64,
    batch_size: usize,
    max_attempts: u32,
}

impl MailboxDispatcher {
    pub fn new(os: Arc<AgentOs>, mailbox_store: Arc<dyn MailboxStore>) -> Self {
        Self {
            os,
            mailbox_store,
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

    async fn dispatch_claimed_entry(
        &self,
        entry: MailboxEntry,
    ) -> Result<Option<RunStream>, ApiError> {
        let claim_token = entry.claim_token.clone().ok_or_else(|| {
            ApiError::Internal(format!(
                "mailbox entry '{}' was claimed without claim_token",
                entry.entry_id
            ))
        })?;

        let entry_id = entry.entry_id.as_str();
        let run_id = entry.run_id.as_str();

        match start_run_for_claimed_entry(&self.os, &self.mailbox_store, &entry, true).await {
            Ok(run) => {
                let accepted_run_id = run.run_id.clone();
                ack_claimed_entry(
                    &self.mailbox_store,
                    entry_id,
                    &claim_token,
                    &accepted_run_id,
                )
                .await?;
                tracing::debug!(entry_id, run_id, "mailbox entry accepted");
                Ok(Some(run))
            }
            Err(MailboxRunStartError::Superseded(error)) => {
                let _ = self
                    .mailbox_store
                    .supersede_mailbox_entry(entry_id, now_unix_millis(), &error)
                    .await
                    .map_err(mailbox_error)?;
                tracing::debug!(entry_id, run_id, %error, "mailbox entry superseded");
                Ok(None)
            }
            Err(MailboxRunStartError::Busy(error))
            | Err(MailboxRunStartError::Retryable(error)) => {
                if entry.attempt_count >= self.max_attempts {
                    dead_letter_claimed_entry(
                        &self.mailbox_store,
                        entry_id,
                        &claim_token,
                        &format!("max attempts ({}) exceeded: {error}", self.max_attempts),
                    )
                    .await?;
                    tracing::warn!(entry_id, run_id, attempts = entry.attempt_count, "mailbox entry dead-lettered after max attempts");
                } else {
                    nack_claimed_entry(
                        &self.mailbox_store,
                        entry_id,
                        &claim_token,
                        self.retry_delay_ms,
                        &error,
                    )
                    .await?;
                    tracing::debug!(entry_id, run_id, attempts = entry.attempt_count, %error, "mailbox entry nacked for retry");
                }
                Ok(None)
            }
            Err(MailboxRunStartError::Permanent(error)) => {
                dead_letter_claimed_entry(
                    &self.mailbox_store,
                    entry_id,
                    &claim_token,
                    &error,
                )
                .await?;
                tracing::warn!(entry_id, run_id, %error, "mailbox entry dead-lettered (permanent)");
                Ok(None)
            }
            Err(MailboxRunStartError::Internal(error)) => Err(error),
        }
    }

    pub async fn dispatch_ready_once(&self) -> Result<usize, ApiError> {
        let claimed = self
            .mailbox_store
            .claim_mailbox_entries(
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
            match result {
                Ok(Some(run)) => {
                    tokio::spawn(drain_background_run(run));
                }
                Ok(None) => {}
                Err(err) if first_error.is_none() => {
                    first_error = Some(err);
                }
                Err(_) => {}
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
                match self.mailbox_store.purge_terminal_mailbox_entries(cutoff).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tirea_agentos::contracts::runtime::behavior::ReadOnlyContext;
    use tirea_agentos::contracts::runtime::phase::{ActionSet, BeforeInferenceAction};
    use tirea_agentos::contracts::{AgentBehavior, TerminationReason};
    use tirea_agentos::{AgentDefinition, AgentOsBuilder};
    use tirea_contract::storage::{MailboxReader, RunReader, ThreadReader};
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
                .with_agent(
                    "test",
                    AgentDefinition {
                        id: "test".to_string(),
                        behavior_ids: vec!["mailbox_terminate".to_string()],
                        ..Default::default()
                    },
                )
                .with_agent_state_store(store)
                .build()
                .expect("build AgentOs"),
        )
    }

    #[tokio::test]
    async fn dispatcher_accepts_enqueued_background_run() {
        let store = Arc::new(MemoryStore::new());
        let mailbox_store: Arc<dyn MailboxStore> = store.clone();
        let os = make_os(store.clone());

        let (thread_id, run_id) = enqueue_background_run(
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
        )
        .await
        .expect("enqueue background run");
        assert_eq!(thread_id, "mailbox-thread");
        assert_eq!(run_id, "mailbox-run");

        MailboxDispatcher::new(os.clone(), mailbox_store.clone())
            .with_consumer_id("test-dispatcher")
            .dispatch_ready_once()
            .await
            .expect("dispatch mailbox run");

        let mailbox_entry =
            MailboxReader::load_mailbox_entry_by_run_id(mailbox_store.as_ref(), &run_id)
                .await
                .expect("load mailbox entry")
                .expect("mailbox entry should exist");
        assert_eq!(mailbox_entry.status, MailboxEntryStatus::Accepted);

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

        let (_thread_id, run_id) = enqueue_background_run(
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
        )
        .await
        .expect("enqueue background run");

        let claimed = mailbox_store
            .claim_mailbox_entries(1, "test-dispatcher", now_unix_millis(), 5_000)
            .await
            .expect("claim mailbox entry");
        assert_eq!(claimed.len(), 1);

        mailbox_store
            .interrupt_mailbox_thread("mailbox-supersede-thread", now_unix_millis())
            .await
            .expect("interrupt mailbox thread");

        let dispatcher = MailboxDispatcher::new(os.clone(), mailbox_store.clone())
            .with_consumer_id("test-dispatcher");
        let started = dispatcher
            .dispatch_claimed_entry(claimed.into_iter().next().expect("claimed entry"))
            .await
            .expect("dispatch after supersede");
        assert!(started.is_none());

        let mailbox_entry =
            MailboxReader::load_mailbox_entry_by_run_id(mailbox_store.as_ref(), &run_id)
                .await
                .expect("load mailbox entry")
                .expect("mailbox entry should exist");
        assert_eq!(mailbox_entry.status, MailboxEntryStatus::Superseded);
        assert!(RunReader::load_run(store.as_ref(), &run_id)
            .await
            .expect("load run record")
            .is_none());
    }
}
