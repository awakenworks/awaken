//! Checkpoint write path: prepare WAL state, publish WAL, then commit.

use std::sync::Arc;

use awaken_server_contract::contract::message::Message;
use awaken_server_contract::contract::storage::{
    RunRecord, StorageError, ThreadRunStore, checkpoint_parent_thread_id,
};
use awaken_server_contract::thread::Thread;

use super::{NatsBufferedThreadStore, entry, hierarchy_claim, hot_meta, keys, reader, wal_state};

#[derive(Debug, Clone, Default)]
pub(crate) struct WriterTestHooks {
    pre_publish_pause: Arc<tokio::sync::Mutex<Option<PausePoint>>>,
    post_publish_pause: Arc<tokio::sync::Mutex<Option<PausePoint>>>,
    post_publish_claim_check_pause: Arc<tokio::sync::Mutex<Option<PausePoint>>>,
    fail_after_mark_committed: Arc<tokio::sync::Mutex<Option<String>>>,
}

#[derive(Debug, Clone)]
struct PausePoint {
    reached: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl WriterTestHooks {
    pub(crate) async fn set_pre_publish_pause(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.pre_publish_pause.lock().await = Some(PausePoint { reached, release });
    }

    pub(crate) async fn set_post_publish_pause(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.post_publish_pause.lock().await = Some(PausePoint { reached, release });
    }

    pub(crate) async fn set_post_publish_claim_check_pause(
        &self,
        reached: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) {
        *self.post_publish_claim_check_pause.lock().await = Some(PausePoint { reached, release });
    }

    async fn pause_before_wal_publish_if_configured(&self) {
        pause_if_configured(&self.pre_publish_pause).await;
    }

    async fn pause_after_wal_publish_if_configured(&self) {
        pause_if_configured(&self.post_publish_pause).await;
    }

    async fn pause_after_post_publish_claim_check_if_configured(&self) {
        pause_if_configured(&self.post_publish_claim_check_pause).await;
    }

    pub(crate) async fn set_fail_after_mark_committed(&self, message: impl Into<String>) {
        *self.fail_after_mark_committed.lock().await = Some(message.into());
    }

    async fn fail_after_mark_committed_if_configured(&self) -> Result<(), StorageError> {
        let Some(message) = self.fail_after_mark_committed.lock().await.take() else {
            return Ok(());
        };
        Err(StorageError::CommitUnknown(message))
    }
}

async fn pause_if_configured(slot: &tokio::sync::Mutex<Option<PausePoint>>) {
    let pause = slot.lock().await.take();
    let Some(pause) = pause else {
        return;
    };
    pause.reached.notify_waiters();
    pause.release.notified().await;
}

pub async fn checkpoint<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    claim: &hierarchy_claim::AcquiredHierarchyClaim,
    thread_id: &str,
    messages: &[Message],
    run: &RunRecord,
) -> Result<(), StorageError> {
    run.validate_for_persist()?;
    let existing_thread = reader::load_thread_overlay(store, thread_id).await?;
    reader::validate_thread_hierarchy_overlay(
        store,
        thread_id,
        checkpoint_parent_thread_id(existing_thread.as_ref(), run),
    )
    .await?;

    let now = now_millis();
    let mut projected_thread = existing_thread.unwrap_or_else(|| Thread::with_id(thread_id));
    projected_thread.metadata.created_at.get_or_insert(now);
    projected_thread.metadata.updated_at = Some(now);
    projected_thread.apply_run_projection(run);

    claim.ensure_current(&store.kv_hot).await?;
    let seq = hot_meta::reserve_seq(&store.kv_hot, thread_id, now).await?;
    wal_state::mark_prepared(&store.kv_hot, thread_id, seq, claim.claim_token(), now).await?;

    let wal_entry = entry::CheckpointEntry {
        thread_id: thread_id.to_string(),
        run: run.clone(),
        messages: messages.to_vec(),
        projected_thread: Some(projected_thread),
        thread_seq: seq,
        written_at: now,
    };
    let payload = entry::encode(&wal_entry)?;

    let result = checkpoint_after_prepare(store, claim, thread_id, run, seq, payload, now).await;
    if result.is_err()
        && let Err(abort_error) =
            wal_state::mark_aborted(&store.kv_hot, thread_id, seq, claim.claim_token()).await
    {
        tracing::warn!(
            thread_id,
            thread_seq = seq,
            error = %abort_error,
            "failed to mark prepared WAL entry aborted"
        );
    }
    result
}

async fn checkpoint_after_prepare<T: ThreadRunStore + Send + Sync + 'static>(
    store: &NatsBufferedThreadStore<T>,
    claim: &hierarchy_claim::AcquiredHierarchyClaim,
    thread_id: &str,
    run: &RunRecord,
    seq: u64,
    payload: bytes::Bytes,
    now: u64,
) -> Result<(), StorageError> {
    claim.ensure_current(&store.kv_hot).await?;
    store
        .writer_test_hooks
        .pause_before_wal_publish_if_configured()
        .await;

    let publish_ack = store
        .jetstream
        .publish(keys::thread_subject(thread_id), payload)
        .await
        .map_err(|error| StorageError::Io(format!("publish: {error}")))?;
    let ack = publish_ack
        .await
        .map_err(|error| StorageError::Io(format!("publish ack: {error}")))?;

    store
        .writer_test_hooks
        .pause_after_wal_publish_if_configured()
        .await;
    claim.ensure_current(&store.kv_hot).await?;
    store
        .writer_test_hooks
        .pause_after_post_publish_claim_check_if_configured()
        .await;

    wal_state::mark_committed(
        &store.kv_hot,
        thread_id,
        seq,
        claim.claim_token(),
        ack.sequence,
    )
    .await?;
    store
        .writer_test_hooks
        .fail_after_mark_committed_if_configured()
        .await?;
    hot_meta::cache_run_if_newer(&store.kv_hot, run, seq).await.map_err(|error| {
        StorageError::CommitUnknown(format!(
            "checkpoint committed before cache promotion failed (thread={thread_id}, seq={seq}): {error}"
        ))
    })?;
    hot_meta::promote_latest_seq(&store.kv_hot, thread_id, seq, ack.sequence, now)
        .await
        .map_err(|error| {
            StorageError::CommitUnknown(format!(
                "checkpoint committed before latest_seq promote failed (thread={thread_id}, seq={seq}): {error}"
            ))
        })?;
    Ok(())
}

use super::recovery::now_millis;
