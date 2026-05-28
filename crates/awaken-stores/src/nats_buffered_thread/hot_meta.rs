//! Hot metadata (latest_seq, flushed_seq, cached runs) in NATS KV.

// Reader/flusher (Tasks 5/6) consume these helpers; allow the gap until then.
#![allow(dead_code)]

use awaken_server_contract::contract::storage::{RunRecord, StorageError};
use bytes::Bytes;
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use super::keys;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThreadHotMetadata {
    /// Highest `thread_seq` a writer has reserved. Incremented via CAS to
    /// hand out unique sequences; NOT rolled back on publish failure
    /// (dead reservations are invisible to readers).
    #[serde(default)]
    pub reserved_seq: u64,
    /// Highest `thread_seq` whose WAL entry has been published and
    /// acknowledged. This is the reader-visible watermark: the hot-cache
    /// and WAL have a durable entry at exactly this seq.
    pub latest_seq: u64,
    /// JetStream stream sequence of the WAL entry corresponding to
    /// `latest_seq`. Readers fetch by this value (not by
    /// `get_last_raw_message_by_subject`) so concurrent writers whose
    /// publish arrivals invert reservation order can't make a reader
    /// observe the wrong WAL entry for the current `latest_seq`.
    #[serde(default)]
    pub latest_js_seq: u64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRun {
    #[serde(default)]
    thread_seq: u64,
    run: RunRecord,
}

pub fn encode_meta(meta: &ThreadHotMetadata) -> Result<Bytes, StorageError> {
    serde_json::to_vec(meta)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn decode_meta(bytes: &[u8]) -> Result<ThreadHotMetadata, StorageError> {
    if bytes.is_empty() {
        return Ok(ThreadHotMetadata::default());
    }
    serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn encode_run(run: &RunRecord) -> Result<Bytes, StorageError> {
    serde_json::to_vec(run)
        .map(Bytes::from)
        .map_err(|e| StorageError::Serialization(e.to_string()))
}

pub fn decode_run(bytes: &[u8]) -> Result<RunRecord, StorageError> {
    serde_json::from_slice(bytes).map_err(|e| StorageError::Serialization(e.to_string()))
}

fn encode_cached_run(run: &RunRecord, thread_seq: u64) -> Result<Bytes, StorageError> {
    serde_json::to_vec(&CachedRun {
        thread_seq,
        run: run.clone(),
    })
    .map(Bytes::from)
    .map_err(|e| StorageError::Serialization(e.to_string()))
}

fn decode_cached_run(bytes: &[u8]) -> Result<(RunRecord, u64), StorageError> {
    match serde_json::from_slice::<CachedRun>(bytes) {
        Ok(cached) => Ok((cached.run, cached.thread_seq)),
        Err(_) => decode_run(bytes).map(|run| (run, 0)),
    }
}

pub fn encode_seq(seq: u64) -> Bytes {
    Bytes::copy_from_slice(&seq.to_le_bytes())
}

pub fn decode_seq(bytes: &[u8]) -> Result<u64, StorageError> {
    if bytes.len() != 8 {
        return Err(StorageError::Serialization(format!(
            "seq expects 8 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(buf))
}

const CAS_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CAS_BACKOFF_START: std::time::Duration = std::time::Duration::from_micros(200);
const CAS_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(20);

/// CAS-increment `reserved_seq`. This hands the writer a unique thread
/// sequence BEFORE it publishes; the reservation is never rolled back on
/// publish failure (a gap in the reserved counter is invisible to readers,
/// which only look at `latest_seq`). Writers MUST call
/// [`promote_latest_seq`] once the WAL publish has been acknowledged.
pub async fn reserve_seq(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    now: u64,
) -> Result<u64, StorageError> {
    let key = keys::hot_meta_key(thread_id);
    let deadline = std::time::Instant::now() + CAS_TOTAL_TIMEOUT;
    let mut backoff = CAS_BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let entry = kv
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let (mut meta, revision) = match entry {
            Some(e) => (decode_meta(&e.value)?, e.revision),
            None => (ThreadHotMetadata::default(), 0),
        };
        // Backfill legacy rows that only carried `latest_seq` so the two
        // counters monotonically track.
        if meta.reserved_seq < meta.latest_seq {
            meta.reserved_seq = meta.latest_seq;
        }
        meta.reserved_seq += 1;
        meta.updated_at = now;
        let new_seq = meta.reserved_seq;
        let bytes = encode_meta(&meta)?;
        let succeeded = if revision == 0 {
            kv.create(&key, bytes).await.is_ok()
        } else {
            kv.update(&key, bytes, revision).await.is_ok()
        };
        if succeeded {
            return Ok(new_seq);
        }
        if std::time::Instant::now() >= deadline {
            return Err(StorageError::Io(format!(
                "reserved_seq CAS timeout after {attempts} attempts (thread={thread_id})"
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), CAS_BACKOFF_MAX);
    }
}

/// CAS `latest_seq = max(current, committed_seq)` and bind the
/// JetStream sequence of the corresponding WAL entry. Called after a
/// successful WAL publish ack. Exits without writing if another writer
/// has already raised `latest_seq` past ours.
pub async fn promote_latest_seq(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    committed_seq: u64,
    committed_js_seq: u64,
    now: u64,
) -> Result<(), StorageError> {
    let key = keys::hot_meta_key(thread_id);
    let deadline = std::time::Instant::now() + CAS_TOTAL_TIMEOUT;
    let mut backoff = CAS_BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let entry = kv
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
        let (mut meta, revision) = match entry {
            Some(e) => (decode_meta(&e.value)?, e.revision),
            None => (ThreadHotMetadata::default(), 0),
        };
        if meta.latest_seq >= committed_seq {
            return Ok(());
        }
        meta.latest_seq = committed_seq;
        meta.latest_js_seq = committed_js_seq;
        if meta.reserved_seq < meta.latest_seq {
            meta.reserved_seq = meta.latest_seq;
        }
        meta.updated_at = now;
        let bytes = encode_meta(&meta)?;
        let succeeded = if revision == 0 {
            kv.create(&key, bytes).await.is_ok()
        } else {
            kv.update(&key, bytes, revision).await.is_ok()
        };
        if succeeded {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StorageError::Io(format!(
                "latest_seq CAS timeout after {attempts} attempts (thread={thread_id})"
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), CAS_BACKOFF_MAX);
    }
}

/// Read the full hot-meta record (thread_seq watermark + JS seq).
pub async fn read_meta(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
) -> Result<ThreadHotMetadata, StorageError> {
    let entry = kv
        .entry(&keys::hot_meta_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
    match entry {
        Some(e) => decode_meta(&e.value),
        None => Ok(ThreadHotMetadata::default()),
    }
}

pub async fn read_latest_seq(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
) -> Result<u64, StorageError> {
    let entry = kv
        .entry(&keys::hot_meta_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
    match entry {
        Some(e) => Ok(decode_meta(&e.value)?.latest_seq),
        None => Ok(0),
    }
}

pub async fn read_flushed_seq(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
) -> Result<u64, StorageError> {
    let entry = kv
        .entry(&keys::flushed_seq_key(thread_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
    match entry {
        Some(e) => decode_seq(&e.value),
        None => Ok(0),
    }
}

pub async fn write_flushed_seq(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    seq: u64,
) -> Result<(), StorageError> {
    let key = keys::flushed_seq_key(thread_id);
    let deadline = std::time::Instant::now() + CAS_TOTAL_TIMEOUT;
    let mut backoff = CAS_BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let entry = kv
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry flushed_seq: {e}")))?;
        let (current, revision) = match entry {
            Some(e) => (decode_seq(&e.value)?, e.revision),
            None => (0, 0),
        };
        if current >= seq {
            return Ok(());
        }
        let bytes = encode_seq(seq);
        let succeeded = if revision == 0 {
            kv.create(&key, bytes).await.is_ok()
        } else {
            kv.update(&key, bytes, revision).await.is_ok()
        };
        if succeeded {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StorageError::Io(format!(
                "flushed_seq CAS timeout after {attempts} attempts (thread={thread_id})"
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), CAS_BACKOFF_MAX);
    }
}

/// Preserve the per-thread sequence watermark across thread deletion so a
/// recreated thread id cannot reuse an older WAL sequence number.
pub async fn write_delete_tombstone(
    kv: &async_nats::jetstream::kv::Store,
    thread_id: &str,
    now: u64,
) -> Result<u64, StorageError> {
    let meta = read_meta(kv, thread_id).await?;
    let flushed = read_flushed_seq(kv, thread_id).await?;
    let watermark = meta.reserved_seq.max(meta.latest_seq).max(flushed);

    if watermark == 0 {
        return Ok(0);
    }

    let tombstone = ThreadHotMetadata {
        reserved_seq: watermark,
        latest_seq: watermark,
        latest_js_seq: 0,
        updated_at: now,
    };
    kv.put(keys::hot_meta_key(thread_id), encode_meta(&tombstone)?)
        .await
        .map_err(|error| StorageError::Io(format!("write delete tombstone meta: {error}")))?;
    kv.put(keys::flushed_seq_key(thread_id), encode_seq(watermark))
        .await
        .map_err(|error| StorageError::Io(format!("write delete tombstone flushed: {error}")))?;

    Ok(watermark)
}

pub async fn cache_run_if_newer(
    kv: &async_nats::jetstream::kv::Store,
    run: &RunRecord,
    thread_seq: u64,
) -> Result<(), StorageError> {
    let key = keys::hot_run_key(&run.run_id);
    let deadline = std::time::Instant::now() + CAS_TOTAL_TIMEOUT;
    let mut backoff = CAS_BACKOFF_START;
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let entry = kv
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry run: {e}")))?;
        let revision = match entry {
            Some(ref e) => {
                let (_, current_seq) = decode_cached_run(&e.value)?;
                if current_seq >= thread_seq {
                    return Ok(());
                }
                e.revision
            }
            None => 0,
        };
        let bytes = encode_cached_run(run, thread_seq)?;
        let succeeded = if revision == 0 {
            kv.create(&key, bytes).await.is_ok()
        } else {
            kv.update(&key, bytes, revision).await.is_ok()
        };
        if succeeded {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(StorageError::Io(format!(
                "hot run cache CAS timeout after {attempts} attempts (run_id={})",
                run.run_id
            )));
        }
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.saturating_mul(2), CAS_BACKOFF_MAX);
    }
}

pub async fn load_cached_run(
    kv: &async_nats::jetstream::kv::Store,
    run_id: &str,
) -> Result<Option<RunRecord>, StorageError> {
    let entry = kv
        .entry(&keys::hot_run_key(run_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
    match entry {
        Some(e) => Ok(Some(decode_cached_run(&e.value)?.0)),
        None => Ok(None),
    }
}

pub async fn load_cached_run_with_seq(
    kv: &async_nats::jetstream::kv::Store,
    run_id: &str,
) -> Result<Option<(RunRecord, u64)>, StorageError> {
    let entry = kv
        .entry(&keys::hot_run_key(run_id))
        .await
        .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?;
    match entry {
        Some(e) => Ok(Some(decode_cached_run(&e.value)?)),
        None => Ok(None),
    }
}

pub async fn pending_thread_ids(
    kv: &async_nats::jetstream::kv::Store,
) -> Result<Vec<String>, StorageError> {
    let mut key_stream = kv
        .keys()
        .await
        .map_err(|e| StorageError::Io(format!("kv keys: {e}")))?;
    let mut pending = Vec::new();
    while let Some(key_result) = key_stream.next().await {
        let key = key_result.map_err(|e| StorageError::Io(format!("kv key: {e}")))?;
        let Some(thread_id) = keys::thread_id_from_hot_meta_key(&key) else {
            continue;
        };
        let meta = read_meta(kv, &thread_id).await?;
        let flushed = read_flushed_seq(kv, &thread_id).await?;
        if meta.latest_seq > flushed {
            pending.push(thread_id);
        }
    }
    pending.sort();
    pending.dedup();
    Ok(pending)
}

pub async fn pending_cached_runs(
    kv: &async_nats::jetstream::kv::Store,
) -> Result<Vec<(RunRecord, u64)>, StorageError> {
    let mut key_stream = kv
        .keys()
        .await
        .map_err(|e| StorageError::Io(format!("kv keys: {e}")))?;
    let mut runs = Vec::new();
    while let Some(key_result) = key_stream.next().await {
        let key = key_result.map_err(|e| StorageError::Io(format!("kv key: {e}")))?;
        if !key.starts_with("run.") {
            continue;
        }
        let Some(entry) = kv
            .entry(&key)
            .await
            .map_err(|e| StorageError::Io(format!("kv entry: {e}")))?
        else {
            continue;
        };
        let (run, seq) = decode_cached_run(&entry.value)?;
        let flushed = read_flushed_seq(kv, &run.thread_id).await?;
        if seq > flushed {
            runs.push((run, seq));
        }
    }
    runs.sort_by(|(a, a_seq), (b, b_seq)| {
        a.thread_id
            .cmp(&b.thread_id)
            .then_with(|| a_seq.cmp(b_seq))
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    Ok(runs)
}
