use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use tirea_agentos::contracts::storage::{
    MailboxEntry, MailboxEntryStatus, MailboxReader, MailboxStore, MailboxStoreError, ThreadReader,
};
use tirea_agentos::contracts::RunRequest;
use tirea_agentos::{AgentOs, AgentOsRunError};
use tirea_contract::storage::RunRecord;

use super::ApiError;

const DEFAULT_MAILBOX_POLL_INTERVAL_MS: u64 = 100;
const DEFAULT_MAILBOX_LEASE_MS: u64 = 30_000;
const DEFAULT_MAILBOX_RETRY_MS: u64 = 250;
const DEFAULT_MAILBOX_BATCH_SIZE: usize = 16;

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

fn mailbox_entry_from_request(request: RunRequest, dedupe_key: Option<String>) -> MailboxEntry {
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
        status: MailboxEntryStatus::Queued,
        request,
        dedupe_key,
        available_at: now,
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

fn is_permanent_dispatch_error(err: &AgentOsRunError) -> bool {
    matches!(err, AgentOsRunError::Resolve(_))
}

async fn drain_background_run(mut run: tirea_agentos::RunStream) {
    while run.events.next().await.is_some() {}
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
    let entry = mailbox_entry_from_request(request, None);

    match mailbox_store.enqueue_mailbox_entry(&entry).await {
        Ok(()) => Ok((thread_id, run_id)),
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
            Ok((existing.thread_id, existing.run_id))
        }
        Err(err) => Err(mailbox_error(err)),
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
        }
    }

    #[must_use]
    pub fn with_consumer_id(mut self, consumer_id: impl Into<String>) -> Self {
        self.consumer_id = consumer_id.into();
        self
    }

    async fn finish_ack(
        &self,
        entry_id: &str,
        claim_token: &str,
        accepted_run_id: &str,
    ) -> Result<(), ApiError> {
        match self
            .mailbox_store
            .ack_mailbox_entry(entry_id, claim_token, accepted_run_id, now_unix_millis())
            .await
        {
            Ok(()) => Ok(()),
            Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
            Err(err) => Err(mailbox_error(err)),
        }
    }

    async fn finish_retry(
        &self,
        entry_id: &str,
        claim_token: &str,
        error: &str,
    ) -> Result<(), ApiError> {
        match self
            .mailbox_store
            .nack_mailbox_entry(
                entry_id,
                claim_token,
                now_unix_millis().saturating_add(self.retry_delay_ms),
                error,
                now_unix_millis(),
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
            Err(err) => Err(mailbox_error(err)),
        }
    }

    async fn finish_dead_letter(
        &self,
        entry_id: &str,
        claim_token: &str,
        error: &str,
    ) -> Result<(), ApiError> {
        match self
            .mailbox_store
            .dead_letter_mailbox_entry(entry_id, claim_token, error, now_unix_millis())
            .await
        {
            Ok(()) => Ok(()),
            Err(MailboxStoreError::ClaimConflict(_)) => Ok(()),
            Err(err) => Err(mailbox_error(err)),
        }
    }

    async fn dispatch_claimed_entry(&self, entry: MailboxEntry) -> Result<(), ApiError> {
        let claim_token = entry.claim_token.clone().ok_or_else(|| {
            ApiError::Internal(format!(
                "mailbox entry '{}' was claimed without claim_token",
                entry.entry_id
            ))
        })?;

        if self
            .os
            .current_run_id_for_thread(&entry.agent_id, &entry.thread_id)
            .await
            .map_err(ApiError::from)?
            .is_some_and(|run_id| run_id != entry.run_id)
        {
            return self
                .finish_retry(
                    &entry.entry_id,
                    &claim_token,
                    "thread already has an active run",
                )
                .await;
        }

        let resolved = match self.os.resolve(&entry.agent_id) {
            Ok(resolved) => resolved,
            Err(err) => {
                return self
                    .finish_dead_letter(&entry.entry_id, &claim_token, &err.to_string())
                    .await
            }
        };

        match self
            .os
            .start_active_run_with_persistence(
                &entry.agent_id,
                entry.request.clone(),
                resolved,
                true,
                false,
            )
            .await
        {
            Ok(run) => {
                let run_id = run.run_id.clone();
                tokio::spawn(drain_background_run(run));
                self.finish_ack(&entry.entry_id, &claim_token, &run_id)
                    .await
            }
            Err(err) if is_permanent_dispatch_error(&err) => {
                self.finish_dead_letter(&entry.entry_id, &claim_token, &err.to_string())
                    .await
            }
            Err(err) => {
                self.finish_retry(&entry.entry_id, &claim_token, &err.to_string())
                    .await
            }
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

        let mut processed = 0usize;
        for entry in claimed {
            self.dispatch_claimed_entry(entry).await?;
            processed = processed.saturating_add(1);
        }
        Ok(processed)
    }

    pub async fn run_forever(self) {
        loop {
            if let Err(err) = self.dispatch_ready_once().await {
                tracing::error!("mailbox dispatcher failed: {err}");
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
}
