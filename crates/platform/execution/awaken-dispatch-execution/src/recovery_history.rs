// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Retains bounded Worker-local history only as a hint for claim-authorized recovery.
//! Bounded Worker-local reuse of committed Thread bodies.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::ThreadId;
use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, ThreadPrefixRef};
use awaken_dispatch_contract::Claimed;
use awaken_sync::InvariantLock;
use awaken_time::mono::{self, Instant};

use crate::RecoveryProjection;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_THREADS: usize = 128;
const IDLE: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct HistoryKey {
    pub scope: String,
    pub session: ThreadId,
    pub thread: ThreadId,
}

impl HistoryKey {
    pub(crate) fn for_claimed(claimed: &Claimed) -> Option<Self> {
        let request = &claimed.request;
        Some(Self {
            scope: request.execution_scope.as_ref()?.0.as_str().to_owned(),
            session: request.session_thread_id().clone(),
            thread: request.thread_id().clone(),
        })
    }
}

pub(crate) struct CachedThread {
    pub prefix: ThreadPrefixRef,
    pub snapshot: RunRecoverySnapshot,
}

struct Entry {
    thread: Arc<CachedThread>,
    last_use: Instant,
    bytes: usize,
}

/// Process-local committed bodies, evictable between attempts.
#[derive(Default)]
pub struct RecoveryHistoryCache {
    entries: InvariantLock<HashMap<HistoryKey, Entry>>,
}

impl RecoveryHistoryCache {
    pub(crate) fn get(&self, key: &HistoryKey) -> Option<Arc<CachedThread>> {
        let mut entries = self.entries.lock().ok()?;
        entries.retain(|_, entry| entry.last_use.elapsed() < IDLE);
        let entry = entries.get_mut(key)?;
        entry.last_use = mono::now();
        Some(entry.thread.clone())
    }

    fn put(&self, key: HistoryKey, thread: CachedThread) {
        let mut counter = ByteCount(0);
        if serde_json::to_writer(&mut counter, &thread.snapshot).is_err() || counter.0 > MAX_BYTES {
            return;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.retain(|_, entry| entry.last_use.elapsed() < IDLE);
        if entries
            .get(&key)
            .is_some_and(|entry| entry.thread.prefix.last_commit > thread.prefix.last_commit)
        {
            return;
        }
        entries.remove(&key);
        let mut bytes = entries.values().map(|entry| entry.bytes).sum::<usize>();
        while entries.len() >= MAX_THREADS || bytes.saturating_add(counter.0) > MAX_BYTES {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_use)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = entries.remove(&oldest) {
                bytes -= entry.bytes;
            }
        }
        entries.insert(
            key,
            Entry {
                thread: Arc::new(thread),
                last_use: mono::now(),
                bytes: counter.0,
            },
        );
    }
}

pub(crate) struct RetainHistory<'a> {
    pub cache: &'a RecoveryHistoryCache,
    pub key: HistoryKey,
    pub projection: &'a RecoveryProjection,
    pub claim: awaken_worker_contract::RunClaim,
}

impl Drop for RetainHistory<'_> {
    fn drop(&mut self) {
        if let Some((prefix, snapshot)) = self.projection.cache_snapshot(&self.claim) {
            self.cache
                .put(self.key.clone(), CachedThread { prefix, snapshot });
        }
    }
}

struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
