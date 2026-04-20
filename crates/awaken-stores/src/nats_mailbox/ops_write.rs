//! Write-path operations.

use async_nats::HeaderMap;
use async_nats::jetstream::kv::CreateErrorKind;
use awaken_contract::contract::lifecycle::RunStatus;
use awaken_contract::contract::mailbox::{RunDispatch, RunDispatchResult, RunDispatchStatus};
use awaken_contract::contract::storage::StorageError;

use super::{NatsMailboxStore, claim_guard, codec, keys};

const DEDUPE_ORPHAN_GRACE_MS: u64 = 5_000;

pub async fn enqueue(store: &NatsMailboxStore, dispatch: &RunDispatch) -> Result<(), StorageError> {
    // ── Stamp dispatch_epoch from authoritative KV state ──
    //
    // The `MailboxStore` trait contract states "sets dispatch epoch from
    // current thread state". Without this, foreground `Mailbox::submit()`
    // calls `interrupt()` (bumps epoch 0→N) and then enqueues a dispatch
    // whose `dispatch_epoch` is still 0 — the epoch-safe claim path then
    // rejects it as stale, so every foreground submit after any interrupt
    // would fail.
    let mut dispatch = dispatch.clone();
    dispatch.dispatch_epoch = current_thread_epoch(store, &dispatch.thread_id).await?;

    // ── Authoritative dedupe (race-free across nodes) ──
    //
    // `kv.create()` on the dedupe lock is an atomic admission check. If
    // two nodes concurrently attempt the same `(thread, dedupe_key)`, at
    // most one succeeds; the loser observes `AlreadyExists`. If a prior
    // holder crashed between lock create and dispatch put, or if the
    // holding dispatch has become terminal/stale-by-epoch, the acquire
    // path reconciles by purging the orphan lock and retrying.
    if let Some(ref dedupe_key) = dispatch.dedupe_key {
        acquire_dedupe_lock(
            store,
            &dispatch.thread_id,
            dedupe_key,
            &dispatch.dispatch_id,
        )
        .await?;
    }

    // The per-thread index is written before the dispatch record. This
    // makes queue scans O(thread dispatches) without introducing a
    // stranded-dispatch gap: if the later dispatch put fails, the index
    // entry is a harmless dangling id that strong loads skip.
    if let Err(e) = append_thread_index(store, &dispatch.thread_id, &dispatch.dispatch_id).await {
        if let Some(ref dedupe_key) = dispatch.dedupe_key {
            release_dedupe_lock(
                store,
                &dispatch.thread_id,
                dedupe_key,
                &dispatch.dispatch_id,
            )
            .await;
        }
        return Err(e);
    }

    // ── Commit point: KV put ──
    //
    // Once the dispatch record is durable in KV, enqueue returns Ok even
    // if later signal publication fails. The sweeper / recovery paths
    // reconcile by re-publishing the JetStream delivery signal for queued
    // dispatches that still need a wakeup.
    let bytes = codec::encode(&dispatch)?;
    let revision = match store
        .kv_dispatch
        .put(keys::dispatch_key(&dispatch.dispatch_id), bytes)
        .await
    {
        Ok(revision) => revision,
        // Roll back the dedupe lock so a retry isn't permanently blocked.
        Err(e) => {
            if let Some(ref dedupe_key) = dispatch.dedupe_key {
                release_dedupe_lock(
                    store,
                    &dispatch.thread_id,
                    dedupe_key,
                    &dispatch.dispatch_id,
                )
                .await;
            }
            return Err(StorageError::Io(format!("kv put: {e}")));
        }
    };

    // Update in-memory index synchronously so later `claim()` can see it
    // without waiting for the KV watcher.
    store
        .index
        .write()
        .await
        .upsert_with_revision(dispatch.clone(), revision);

    // Best-effort: JetStream delivery signal. Failure is recovered by the
    // sweeper, which re-publishes for every Queued dispatch still missing
    // its notification.
    let subject = keys::dispatch_subject(&dispatch.thread_id);
    let payload = bytes::Bytes::from(dispatch.dispatch_id.clone().into_bytes());
    let publish_result = if let Some(ref dedupe_key) = dispatch.dedupe_key {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Nats-Msg-Id",
            keys::dedupe_msg_id(&dispatch.thread_id, dedupe_key, &dispatch.dispatch_id).as_str(),
        );
        store
            .jetstream
            .publish_with_headers(subject, headers, payload)
            .await
    } else {
        store.jetstream.publish(subject, payload).await
    };
    match publish_result {
        Ok(future) => {
            if let Err(e) = future.await {
                tracing::warn!(
                    thread_id = dispatch.thread_id,
                    dispatch_id = dispatch.dispatch_id,
                    error = %e,
                    "JetStream publish ack failed; sweeper will retry"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                thread_id = dispatch.thread_id,
                dispatch_id = dispatch.dispatch_id,
                error = %e,
                "JetStream publish failed; sweeper will retry"
            );
        }
    }

    Ok(())
}

/// Create the dedupe lock KV entry. Returns `AlreadyExists` if another
/// node legitimately holds the lock for this `(thread, dedupe_key)`.
///
/// Before surfacing `AlreadyExists`, reconciles the lock against the
/// authoritative dispatch record and thread epoch so a crash between
/// lock create and dispatch put, or a dispatch that has since become
/// terminal / stale-by-epoch, does not permanently block the key.
async fn acquire_dedupe_lock(
    store: &NatsMailboxStore,
    thread_id: &str,
    dedupe_key: &str,
    dispatch_id: &str,
) -> Result<(), StorageError> {
    let key = keys::dedupe_lock_key(thread_id, dedupe_key);
    for _ in 0..3 {
        let record = codec::DedupeLockRecord {
            dispatch_id: dispatch_id.to_string(),
            created_at: current_millis(),
        };
        let value = codec::encode_dedupe_lock(&record)?;
        match store.kv_thread_index.create(&key, value).await {
            Ok(_) => return Ok(()),
            Err(err) => {
                if err.kind() != CreateErrorKind::AlreadyExists {
                    return Err(StorageError::Io(format!("dedupe lock create: {err}")));
                }
                // Conflict → reconcile against the authoritative state.
                if reconcile_dedupe_lock(store, thread_id, dedupe_key).await? {
                    // Orphan/terminal/stale lock purged; retry acquire.
                    continue;
                }
                return Err(StorageError::AlreadyExists(format!(
                    "dedupe_key '{dedupe_key}' already active on thread '{thread_id}'"
                )));
            }
        }
    }
    Err(StorageError::Io(format!(
        "dedupe lock reconcile exhausted retries for key '{dedupe_key}' on thread '{thread_id}'"
    )))
}

/// Inspect the holder of a dedupe lock; if its dispatch is missing,
/// terminal, or stale-by-epoch, CAS-purge the lock and report that a
/// retry should succeed. Returns `Ok(true)` when the lock was purged.
async fn reconcile_dedupe_lock(
    store: &NatsMailboxStore,
    thread_id: &str,
    dedupe_key: &str,
) -> Result<bool, StorageError> {
    let key = keys::dedupe_lock_key(thread_id, dedupe_key);
    let entry = match store
        .kv_thread_index
        .entry(&key)
        .await
        .map_err(|e| StorageError::Io(format!("dedupe lock entry: {e}")))?
    {
        Some(entry) => entry,
        None => return Ok(true), // gone already; retry acquire
    };
    let lock = codec::decode_dedupe_lock(&entry.value)?;
    if lock.dispatch_id.is_empty() {
        return purge_lock_if_revision(store, &key, entry.revision).await;
    }

    let holder_entry = store
        .kv_dispatch
        .entry(&keys::dispatch_key(&lock.dispatch_id))
        .await
        .map_err(|e| StorageError::Io(format!("dedupe reconcile dispatch lookup: {e}")))?;
    let Some(holder_entry) = holder_entry else {
        // Lock created, dispatch not materialised yet. Treat young locks
        // as in-flight enqueue owners; only purge after a short orphan
        // grace so concurrent enqueue cannot steal the lock.
        if !is_dedupe_orphan_expired(lock.created_at) {
            return Ok(false);
        }
        return purge_lock_if_revision(store, &key, entry.revision).await;
    };
    let holder = codec::decode(&holder_entry.value)?;
    if matches!(
        holder.status,
        RunDispatchStatus::Acked
            | RunDispatchStatus::Cancelled
            | RunDispatchStatus::DeadLetter
            | RunDispatchStatus::Superseded
    ) {
        return purge_lock_if_revision(store, &key, entry.revision).await;
    }
    let thread_epoch = match store
        .kv_epoch
        .entry(&keys::epoch_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("dedupe reconcile epoch lookup: {e}")))?
    {
        Some(e) => codec::decode_epoch(&e.value)?,
        None => 0,
    };
    if holder.dispatch_epoch < thread_epoch {
        // Holder wasn't seen by interrupt's local-index sweep but is
        // stale by authoritative epoch. Queued holders can be released so a
        // fresh enqueue can proceed; Claimed holders are still active and keep
        // their dedupe lock until their terminal transition releases it.
        if holder.status == RunDispatchStatus::Queued {
            return purge_lock_if_revision(store, &key, entry.revision).await;
        }
        return Ok(false);
    }
    Ok(false)
}

fn is_dedupe_orphan_expired(created_at: u64) -> bool {
    created_at == 0 || current_millis().saturating_sub(created_at) >= DEDUPE_ORPHAN_GRACE_MS
}

async fn purge_lock_if_revision(
    store: &NatsMailboxStore,
    key: &str,
    revision: u64,
) -> Result<bool, StorageError> {
    // Revision-guarded purge prevents a stale reconciler from deleting a
    // newer owner that acquired the same dedupe key after our inspection.
    match store
        .kv_thread_index
        .purge_expect_revision(key, Some(revision))
        .await
    {
        Ok(_) => Ok(true),
        Err(err) => {
            if store
                .kv_thread_index
                .entry(key)
                .await
                .map_err(|e| {
                    StorageError::Io(format!("dedupe lock entry after purge conflict: {e}"))
                })?
                .is_none()
            {
                return Ok(true);
            }
            tracing::warn!(key, revision, error = %err, "dedupe lock purge failed");
            Ok(false)
        }
    }
}

pub(crate) async fn current_thread_epoch(
    store: &NatsMailboxStore,
    thread_id: &str,
) -> Result<u64, StorageError> {
    match store
        .kv_epoch
        .entry(&keys::epoch_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry epoch: {e}")))?
    {
        Some(e) => codec::decode_epoch(&e.value),
        None => Ok(0),
    }
}

/// Delete the dedupe lock. Idempotent — failures are logged and swallowed
/// because a stale lock is recoverable (manual purge or TTL sweep) while
/// a panicking release would strand a whole terminal transition.
pub(super) async fn release_dedupe_lock(
    store: &NatsMailboxStore,
    thread_id: &str,
    dedupe_key: &str,
    dispatch_id: &str,
) {
    let key = keys::dedupe_lock_key(thread_id, dedupe_key);
    let entry = match store.kv_thread_index.entry(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(
                thread_id,
                dedupe_key,
                dispatch_id,
                error = %err,
                "failed to read dedupe lock before release"
            );
            return;
        }
    };
    let lock = match codec::decode_dedupe_lock(&entry.value) {
        Ok(lock) => lock,
        Err(err) => {
            tracing::warn!(
                thread_id,
                dedupe_key,
                dispatch_id,
                error = %err,
                "failed to decode dedupe lock before release"
            );
            return;
        }
    };
    if lock.dispatch_id != dispatch_id {
        return;
    }
    if let Err(err) = store
        .kv_thread_index
        .purge_expect_revision(&key, Some(entry.revision))
        .await
    {
        tracing::warn!(
            thread_id,
            dedupe_key,
            dispatch_id,
            revision = entry.revision,
            error = %err,
            "failed to release dedupe lock (idempotent; sweeper may reconcile)"
        );
    }
}

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) async fn append_thread_index(
    store: &NatsMailboxStore,
    thread_id: &str,
    dispatch_id: &str,
) -> Result<(), StorageError> {
    let key = keys::thread_index_key(thread_id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut backoff = std::time::Duration::from_micros(200);
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let entry = store
            .kv_thread_index
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let (mut ids, revision) = match entry {
            Some(e) => (codec::decode_thread_index(&e.value)?, e.revision),
            None => (Vec::new(), 0),
        };
        if ids.contains(&dispatch_id.to_string()) {
            return Ok(());
        }
        ids.push(dispatch_id.to_string());
        let bytes = codec::encode_thread_index(&ids)?;
        let result: Result<(), String> = if revision == 0 {
            store
                .kv_thread_index
                .create(&key, bytes)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        } else {
            store
                .kv_thread_index
                .update(&key, bytes, revision)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        match result {
            Ok(_) => return Ok(()),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(StorageError::Io(format!(
                        "thread_index CAS timeout after {attempts} attempts"
                    )));
                }
                tracing::debug!(error = %e, "thread_index CAS retry");
                tokio::time::sleep(backoff).await;
                backoff = std::cmp::min(
                    backoff.saturating_mul(2),
                    std::time::Duration::from_millis(20),
                );
            }
        }
    }
}

pub async fn extend_lease(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    extension_ms: u64,
    now: u64,
) -> Result<bool, StorageError> {
    for _ in 0..5 {
        let entry = store
            .kv_dispatch
            .entry(&keys::dispatch_key(dispatch_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(false);
        };
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status != RunDispatchStatus::Claimed {
            return Ok(false);
        }
        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Ok(false);
        }
        let thread_epoch = current_thread_epoch(store, &dispatch.thread_id).await?;
        if dispatch.dispatch_epoch < thread_epoch {
            dispatch.status = RunDispatchStatus::Superseded;
            dispatch.dispatch_epoch = thread_epoch;
            dispatch.completed_at = Some(now);
            dispatch.updated_at = now;
            dispatch.claim_token = None;
            dispatch.claimed_by = None;
            dispatch.lease_until = None;
            let bytes = codec::encode(&dispatch)?;
            if let Ok(revision) = store
                .kv_dispatch
                .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
                .await
            {
                store
                    .index
                    .write()
                    .await
                    .upsert_with_revision(dispatch.clone(), revision);
                claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
                if let Some(ref dedupe_key) = dispatch.dedupe_key {
                    release_dedupe_lock(
                        store,
                        &dispatch.thread_id,
                        dedupe_key,
                        &dispatch.dispatch_id,
                    )
                    .await;
                }
                return Ok(false);
            }
            continue;
        }
        let lease_until = now.saturating_add(extension_ms);
        if !claim_guard::extend(
            store,
            &dispatch.thread_id,
            dispatch_id,
            claim_token,
            lease_until,
        )
        .await?
        {
            return Ok(false);
        }
        dispatch.lease_until = Some(lease_until);
        dispatch.updated_at = now;
        let bytes = codec::encode(&dispatch)?;
        if let Ok(revision) = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
        {
            store
                .index
                .write()
                .await
                .upsert_with_revision(dispatch, revision);
            return Ok(true);
        }
    }
    Ok(false)
}

/// Load, mutate via closure, CAS write back. Returns the updated `RunDispatch`.
///
/// Returns `Ok(None)` if the dispatch doesn't exist.
/// Returns `Err(StorageError::NotFound)` if the mutate closure rejects.
async fn cas_update<F>(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    stale_check_now: Option<u64>,
    mutate: F,
) -> Result<Option<(RunDispatch, u64)>, StorageError>
where
    F: Fn(&mut RunDispatch) -> Result<(), StorageError>,
{
    for _ in 0..5 {
        let entry = store
            .kv_dispatch
            .entry(&keys::dispatch_key(dispatch_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let mut dispatch = codec::decode(&entry.value)?;
        if let Some(now) = stale_check_now
            && dispatch.status == RunDispatchStatus::Claimed
        {
            let thread_epoch = current_thread_epoch(store, &dispatch.thread_id).await?;
            if dispatch.dispatch_epoch < thread_epoch {
                let stale_epoch = dispatch.dispatch_epoch;
                let old_claim_token = dispatch.claim_token.clone();
                dispatch.status = RunDispatchStatus::Superseded;
                dispatch.dispatch_epoch = thread_epoch;
                dispatch.last_error =
                    Some("claimed dispatch superseded by newer dispatch epoch".to_string());
                dispatch.completed_at = Some(now);
                dispatch.updated_at = now;
                clear_claim_fields(&mut dispatch);
                let bytes = codec::encode(&dispatch)?;
                if let Ok(revision) = store
                    .kv_dispatch
                    .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
                    .await
                {
                    store
                        .index
                        .write()
                        .await
                        .upsert_with_revision(dispatch.clone(), revision);
                    if let Some(ref claim_token) = old_claim_token {
                        claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token)
                            .await?;
                    }
                    if let Some(ref dedupe_key) = dispatch.dedupe_key {
                        release_dedupe_lock(
                            store,
                            &dispatch.thread_id,
                            dedupe_key,
                            &dispatch.dispatch_id,
                        )
                        .await;
                    }
                    return Err(StorageError::VersionConflict {
                        expected: stale_epoch,
                        actual: thread_epoch,
                    });
                }
                continue;
            }
        }
        mutate(&mut dispatch)?;
        let bytes = codec::encode(&dispatch)?;
        if let Ok(revision) = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
        {
            store
                .index
                .write()
                .await
                .upsert_with_revision(dispatch.clone(), revision);
            return Ok(Some((dispatch, revision)));
        }
    }
    Err(StorageError::Io("CAS exhausted retries".to_string()))
}

fn clear_claim_fields(dispatch: &mut RunDispatch) {
    dispatch.claim_token = None;
    dispatch.claimed_by = None;
    dispatch.lease_until = None;
}

pub async fn ack(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    now: u64,
) -> Result<(), StorageError> {
    let result = cas_update(store, dispatch_id, Some(now), |d| {
        if d.status != RunDispatchStatus::Claimed {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not claimed (status={:?})",
                d.status
            )));
        }
        if d.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::NotFound(format!(
                "claim_token mismatch for {dispatch_id}"
            )));
        }
        d.status = RunDispatchStatus::Acked;
        d.completed_at = Some(now);
        d.updated_at = now;
        clear_claim_fields(d);
        Ok(())
    })
    .await?;
    if result.is_none() {
        return Err(StorageError::NotFound(dispatch_id.to_string()));
    }
    if let Some((dispatch, _)) = result {
        claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
        // Terminal state — release the dedupe lock so a future request
        // with the same key can succeed.
        if let Some(ref dedupe_key) = dispatch.dedupe_key {
            release_dedupe_lock(
                store,
                &dispatch.thread_id,
                dedupe_key,
                &dispatch.dispatch_id,
            )
            .await;
        }
    }
    Ok(())
}

pub async fn nack(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    retry_at: u64,
    error: &str,
    now: u64,
) -> Result<(), StorageError> {
    let result = cas_update(store, dispatch_id, Some(now), |d| {
        if d.status != RunDispatchStatus::Claimed {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not claimed"
            )));
        }
        if d.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::NotFound(format!(
                "claim_token mismatch for {dispatch_id}"
            )));
        }
        d.attempt_count += 1;
        d.last_error = Some(error.to_string());
        d.updated_at = now;
        d.claim_token = None;
        d.claimed_by = None;
        d.lease_until = None;
        if d.attempt_count >= d.max_attempts {
            d.status = RunDispatchStatus::DeadLetter;
            d.completed_at = Some(now);
        } else {
            d.status = RunDispatchStatus::Queued;
            d.available_at = retry_at;
        }
        Ok(())
    })
    .await?;
    if result.is_none() {
        return Err(StorageError::NotFound(dispatch_id.to_string()));
    }
    if let Some((dispatch, _)) = result {
        claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
        // Only release the dedupe lock if THIS attempt was actually
        // terminal (nack can either retry-queue or dead-letter).
        if dispatch.status == RunDispatchStatus::DeadLetter
            && let Some(ref dedupe_key) = dispatch.dedupe_key
        {
            release_dedupe_lock(
                store,
                &dispatch.thread_id,
                dedupe_key,
                &dispatch.dispatch_id,
            )
            .await;
        }
    }
    Ok(())
}

pub async fn dead_letter(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    error: &str,
    now: u64,
) -> Result<(), StorageError> {
    let result = cas_update(store, dispatch_id, Some(now), |d| {
        if d.status != RunDispatchStatus::Claimed {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not claimed"
            )));
        }
        if d.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::NotFound(format!(
                "claim_token mismatch for {dispatch_id}"
            )));
        }
        d.status = RunDispatchStatus::DeadLetter;
        d.last_error = Some(error.to_string());
        d.completed_at = Some(now);
        d.updated_at = now;
        clear_claim_fields(d);
        Ok(())
    })
    .await?;
    if result.is_none() {
        return Err(StorageError::NotFound(dispatch_id.to_string()));
    }
    if let Some((dispatch, _)) = result {
        claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
        if let Some(ref dedupe_key) = dispatch.dedupe_key {
            release_dedupe_lock(
                store,
                &dispatch.thread_id,
                dedupe_key,
                &dispatch.dispatch_id,
            )
            .await;
        }
    }
    Ok(())
}

pub async fn supersede_claimed(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    now: u64,
    reason: &str,
) -> Result<Option<RunDispatch>, StorageError> {
    for _ in 0..5 {
        let entry = store
            .kv_dispatch
            .entry(&keys::dispatch_key(dispatch_id))
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let mut dispatch = codec::decode(&entry.value)?;
        if dispatch.status != RunDispatchStatus::Claimed {
            return Ok(None);
        }
        if dispatch.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::VersionConflict {
                expected: 0,
                actual: 1,
            });
        }
        let thread_epoch = current_thread_epoch(store, &dispatch.thread_id).await?;
        let old_claim_token = dispatch.claim_token.clone();
        dispatch.status = RunDispatchStatus::Superseded;
        dispatch.dispatch_epoch = dispatch.dispatch_epoch.max(thread_epoch);
        dispatch.last_error = Some(reason.to_string());
        dispatch.completed_at = Some(now);
        dispatch.updated_at = now;
        clear_claim_fields(&mut dispatch);
        let bytes = codec::encode(&dispatch)?;
        if let Ok(revision) = store
            .kv_dispatch
            .update(&keys::dispatch_key(dispatch_id), bytes, entry.revision)
            .await
        {
            store
                .index
                .write()
                .await
                .upsert_with_revision(dispatch.clone(), revision);
            if let Some(ref claim_token) = old_claim_token {
                claim_guard::release(store, &dispatch.thread_id, dispatch_id, claim_token).await?;
            }
            if let Some(ref dedupe_key) = dispatch.dedupe_key {
                release_dedupe_lock(
                    store,
                    &dispatch.thread_id,
                    dedupe_key,
                    &dispatch.dispatch_id,
                )
                .await;
            }
            return Ok(Some(dispatch));
        }
    }
    Err(StorageError::Io(
        "supersede claimed CAS exhausted retries".to_string(),
    ))
}

pub async fn cancel(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    now: u64,
) -> Result<Option<RunDispatch>, StorageError> {
    let result = match cas_update(store, dispatch_id, None, |d| {
        if d.status != RunDispatchStatus::Queued {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not cancellable (status={:?})",
                d.status
            )));
        }
        d.status = RunDispatchStatus::Cancelled;
        d.completed_at = Some(now);
        d.updated_at = now;
        clear_claim_fields(d);
        Ok(())
    })
    .await
    {
        Ok(result) => result,
        Err(StorageError::NotFound(_)) => return Ok(None),
        Err(other) => return Err(other),
    };
    let result = result.map(|(dispatch, _)| dispatch);
    if let Some(ref dispatch) = result
        && let Some(ref dedupe_key) = dispatch.dedupe_key
    {
        release_dedupe_lock(
            store,
            &dispatch.thread_id,
            dedupe_key,
            &dispatch.dispatch_id,
        )
        .await;
    }
    Ok(result)
}

pub async fn record_dispatch_start(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    dispatch_instance_id: &str,
    now: u64,
) -> Result<(), StorageError> {
    let result = cas_update(store, dispatch_id, Some(now), |d| {
        if d.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::NotFound(format!(
                "claim_token mismatch for {dispatch_id}"
            )));
        }
        if d.status != RunDispatchStatus::Claimed {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not claimed"
            )));
        }
        d.dispatch_instance_id = Some(dispatch_instance_id.to_string());
        d.run_status = Some(RunStatus::Running);
        d.termination = None;
        d.run_response = None;
        d.run_error = None;
        d.completed_at = None;
        d.updated_at = now;
        Ok(())
    })
    .await?;
    if result.is_none() {
        return Err(StorageError::NotFound(dispatch_id.to_string()));
    }
    Ok(())
}

pub async fn record_run_result(
    store: &NatsMailboxStore,
    dispatch_id: &str,
    claim_token: &str,
    result: &RunDispatchResult,
    now: u64,
) -> Result<(), StorageError> {
    let updated = cas_update(store, dispatch_id, Some(now), |d| {
        if d.claim_token.as_deref() != Some(claim_token) {
            return Err(StorageError::NotFound(format!(
                "claim_token mismatch for {dispatch_id}"
            )));
        }
        if d.status != RunDispatchStatus::Claimed {
            return Err(StorageError::NotFound(format!(
                "dispatch {dispatch_id} not claimed"
            )));
        }
        d.dispatch_instance_id = Some(result.dispatch_instance_id.clone());
        d.run_status = Some(result.status);
        d.termination = result.termination.clone();
        d.run_response = result.response.clone();
        d.run_error = result.error.clone();
        d.completed_at = Some(now);
        d.updated_at = now;
        Ok(())
    })
    .await?;
    if updated.is_none() {
        return Err(StorageError::NotFound(dispatch_id.to_string()));
    }
    Ok(())
}
