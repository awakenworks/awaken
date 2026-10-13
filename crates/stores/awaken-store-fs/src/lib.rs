// @formal entrypoint checkpoint.crash_preserves_durable_truth
// @formal entrypoint checkpoint.recovery_never_precedes_commit
// @formal entrypoint remote_worker.operation_identity_maps_to_one_hash_and_append
// @formal entrypoint run.commit_shape_atomicity
// @formal entrypoint store_history.commits_form_one_dense_stable_global_order
// @formal entrypoint store_history.exact_retry_and_every_rejection_are_atomic_stutters
// @formal entrypoint store_history.receipt_refines_one_exact_durable_record
// @formal entrypoint store_history.recovery_preserves_committed_time_and_receipt_identity
// @formal entrypoint store_history.terminal_run_is_final_for_every_backend_history
// @formal entrypoint thread_state.atomic_commit_and_replay
// @formal evidence outcome.stable_run_identity_and_exactly_once_ack
//! Filesystem-backed durable store (ADR-0039 slice 2.3).
//!
//! Persistence is an append-only commit log (`commits.ndjson`): each `commit`
//! serializes the [`ThreadCommit`] as one line and fsyncs it before the commit is
//! acknowledged. The in-memory read model is the tested `awaken-store-inmem`
//! coordinator, **rebuilt from the log on open** — so a fresh instance over the
//! same directory replays committed facts and resumes a durable run after a
//! process restart (invariant 2, ADR-0039 D4; facts are the authority, ADR-0006).
//!
//! Crash safety: a torn final line (a write interrupted mid-append) fails to
//! parse and is discarded on open, so recovery keeps exactly the committed
//! prefix. Whole committed lines are durable (fsynced) before acknowledgement.
//! A retained OS lock makes one coordinator the directory owner across processes;
//! format upgrades replace the log atomically before publishing their version.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::record::Record as EventRecord;
use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{
    StreamCheckpoint, StreamCheckpointError, StreamCheckpointStore,
};
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{
    CommitOperation, CommitReceipt, require_commit_operation_receipt,
};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::committed_thread_view::CommittedThreadView;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_fs_durability::sync_directory;
use awaken_sandbox_fs::{ExclusiveFileLock, PathEntry, try_lock_exclusive};
use awaken_store_inmem::{MemoryCommitCoordinator, MemoryCommitReplay};

const LOG_FILE: &str = "commits.ndjson";
const LOG_LOCK_FILE: &str = "commits.lock";
const LOG_VERSION_FILE: &str = "commits.version";
const PREVIOUS_LOG_VERSION: &[u8] = b"1\n";
const CURRENT_LOG_VERSION: &[u8] = b"2\n";
static CHECKPOINT_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum CommitLogEntry {
    Operation(CommitOperation),
    Commit(ThreadCommit),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredCommitLogEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_at_unix_ms: Option<u64>,
    #[serde(flatten)]
    entry: CommitLogEntry,
}

/// Typed hand-off from durable decoding to the sole replay owner. Keeping the
/// complete entry and its original observation together prevents recovery from
/// sampling a new clock or reconstructing operation identity from projections.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecoveryReplayPlan<E, T> {
    entry: E,
    committed_at: T,
}

fn recovery_replay_plan<E, T>(entry: E, committed_at: T) -> RecoveryReplayPlan<E, T> {
    RecoveryReplayPlan {
        entry,
        committed_at,
    }
}

#[cfg(kani)]
/// @obligation store_history.recovery_preserves_committed_time_and_receipt_identity
#[kani::proof]
fn recovery_replay_preserves_complete_entry_and_time() {
    // Full scalar cause/effect table: every decoded entry identity byte and
    // every optional u64 committed time are carried unchanged to replay. There
    // is no alternate clock/identity branch. Filesystem decoding, fsync and the
    // inner coordinator's lock remain TCB/executable evidence.
    let entry: u8 = kani::any();
    let committed_at: Option<u64> = kani::any();
    let plan = recovery_replay_plan(entry, committed_at);
    assert_eq!(plan.entry, entry);
    assert_eq!(plan.committed_at, committed_at);
}

/// A filesystem `CommitCoordinator` + `CheckpointReader`. Durable truth is the
/// append-only log; reads are served from an in-memory model rebuilt from it.
pub struct FsCommitCoordinator {
    log: Mutex<File>,
    inner: MemoryCommitCoordinator,
    /// Stamps each appended record's `committed_at_unix_ms`, the same clock the
    /// in-memory read model is built over.
    clock: Arc<dyn awaken_time::WallClock>,
    write_lock: tokio::sync::Mutex<()>,
    // The OS lock is retained for the complete coordinator lifetime. Migration,
    // replay, append admission, and append acknowledgement therefore share one
    // cross-process ownership boundary instead of racing through separate locks.
    _lifecycle_lock: ExclusiveFileLock,
}

fn invalid_log(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn read_next_log_entry<R: BufRead>(
    reader: &mut R,
    record: &mut Vec<u8>,
    durable_len: &mut u64,
    normalize_historical_wire: bool,
) -> std::io::Result<Option<StoredCommitLogEntry>> {
    loop {
        record.clear();
        let read = reader.read_until(b'\n', record)?;
        if read == 0 || !record.ends_with(b"\n") {
            return Ok(None);
        }

        let offset = *durable_len;
        *durable_len += read as u64;
        let payload = record.strip_suffix(b"\n").unwrap_or(record);
        let payload = payload.strip_suffix(b"\r").unwrap_or(payload);
        if payload.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut value: serde_json::Value = serde_json::from_slice(payload).map_err(|error| {
            invalid_log(format!(
                "invalid newline-terminated commit log record at byte {offset}: {error}"
            ))
        })?;
        if normalize_historical_wire {
            normalize_historical_log_entry(&mut value)?;
        }
        return serde_json::from_value(value).map(Some).map_err(|error| {
            invalid_log(format!(
                "invalid newline-terminated commit log record at byte {offset}: {error}"
            ))
        });
    }
}

fn normalize_historical_log_entry(value: &mut serde_json::Value) -> std::io::Result<()> {
    let is_operation = value
        .as_object()
        .is_some_and(|object| object.contains_key("operation_id"));
    let commit = if is_operation {
        value
            .get_mut("commit")
            .ok_or_else(|| invalid_log("historical commit operation has no commit payload"))?
    } else {
        value
    };
    let object = commit
        .as_object_mut()
        .ok_or_else(|| invalid_log("historical ThreadCommit is not an object"))?;

    if let Some(waiting) = object.remove("waiting") {
        if object.contains_key("resume_ticket") {
            return Err(invalid_log(
                "historical ThreadCommit contains both waiting and resume_ticket",
            ));
        }
        object.insert("resume_ticket".to_string(), waiting);
    }
    if let Some(events) = object
        .get_mut("events")
        .and_then(serde_json::Value::as_array_mut)
    {
        for event in events {
            let Some(kind) = event.get_mut("kind") else {
                continue;
            };
            match kind.as_str() {
                Some("RunPhaseChanged") => {
                    *kind = serde_json::Value::String("RunStateChanged".to_string());
                }
                Some("RunWaiting") => {
                    *kind = serde_json::Value::String("RunAwaiting".to_string());
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn canonicalize_log(
    source: &mut File,
    stage: &mut File,
    normalize_historical_wire: bool,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(source);
    let mut record = Vec::new();
    let mut durable_len = 0_u64;
    while let Some(entry) = read_next_log_entry(
        &mut reader,
        &mut record,
        &mut durable_len,
        normalize_historical_wire,
    )? {
        serde_json::to_writer(&mut *stage, &entry)
            .map_err(|error| invalid_log(format!("serialize migrated commit log: {error}")))?;
        stage.write_all(b"\n")?;
    }
    Ok(())
}

fn migrate_log_if_needed(lock: &ExclusiveFileLock) -> std::io::Result<()> {
    let version_leaf = Path::new(LOG_VERSION_FILE);
    let previous_version = match lock.classify_sibling_nofollow(version_leaf)? {
        PathEntry::RegularFile => {
            let version = lock.read_sibling_regular_file_nofollow(version_leaf)?;
            if version == CURRENT_LOG_VERSION {
                return match lock.classify_sibling_nofollow(Path::new(LOG_FILE))? {
                    PathEntry::RegularFile => Ok(()),
                    PathEntry::Absent => Err(invalid_log(
                        "filesystem commit log version exists without its commit log",
                    )),
                    _ => Err(invalid_log("filesystem commit log is not a regular file")),
                };
            }
            if version == PREVIOUS_LOG_VERSION {
                true
            } else {
                return Err(invalid_log(format!(
                    "unsupported filesystem commit log version `{}`",
                    String::from_utf8_lossy(&version).trim()
                )));
            }
        }
        PathEntry::Absent => false,
        _ => {
            return Err(invalid_log(
                "filesystem commit log version is not a regular file",
            ));
        }
    };

    let log_leaf = Path::new(LOG_FILE);
    let log_existed = match lock.classify_sibling_nofollow(log_leaf)? {
        PathEntry::Absent => false,
        PathEntry::RegularFile => true,
        _ => return Err(invalid_log("filesystem commit log is not a regular file")),
    };
    if previous_version && !log_existed {
        return Err(invalid_log(
            "filesystem commit log version exists without its commit log",
        ));
    }

    // Publication order is the crash protocol: the canonical log becomes
    // durable before its version. A crash before replacement leaves the old log;
    // a crash after replacement but before the marker leaves an idempotently
    // re-migratable current log. No state can claim the version before the data.
    if log_existed {
        lock.transform_sibling_regular_file_atomic(log_leaf, |source, stage| {
            // A v1 marker proves phase 1 already emitted only current wire names.
            // Unversioned logs are normalized structurally before the current-only
            // domain decoder runs; no retired enum alias participates.
            canonicalize_log(source, stage, !previous_version)
        })?;
    } else {
        lock.publish_sibling_file_noreplace(log_leaf, b"")?;
    }
    if previous_version {
        lock.replace_sibling_regular_file_atomic(version_leaf, CURRENT_LOG_VERSION)
    } else {
        lock.publish_sibling_file_noreplace(version_leaf, CURRENT_LOG_VERSION)
    }
}

impl FsCommitCoordinator {
    /// Open (creating if needed) a store rooted at `dir`, replaying the committed
    /// log into the read model. A torn final line is discarded (crash recovery).
    pub async fn open(
        dir: impl AsRef<Path>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> std::io::Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let path: PathBuf = dir.join(LOG_FILE);
        let lifecycle_lock = try_lock_exclusive(&dir.join(LOG_LOCK_FILE))?;
        migrate_log_if_needed(&lifecycle_lock)?;

        let mut replay = MemoryCommitReplay::new(clock.clone());
        let mut durable_len = 0_u64;
        if let Ok(file) = File::open(&path) {
            let mut reader = BufReader::new(file);
            let mut record = Vec::new();
            while let Some(stored) =
                read_next_log_entry(&mut reader, &mut record, &mut durable_len, false)?
            {
                let plan = recovery_replay_plan(stored.entry, stored.committed_at_unix_ms);
                match plan.entry {
                    CommitLogEntry::Commit(commit) => {
                        // Replaying through the tested read model reconstructs the
                        // same committed state (and sequence) the writer produced.
                        replay
                            .replay_commit_at(commit, plan.committed_at)
                            .await
                            .map_err(|err| std::io::Error::other(err.to_string()))?;
                    }
                    CommitLogEntry::Operation(operation) => {
                        let receipt = replay
                            .replay_operation_at(operation.clone(), plan.committed_at)
                            .await
                            .map_err(|err| std::io::Error::other(err.to_string()))?;
                        // Recovery refuses to open if the rebuilt receipt does
                        // not name the exact durable operation and payload.
                        require_commit_operation_receipt(&operation, &receipt)
                            .map_err(|err| invalid_log(err.to_string()))?;
                    }
                }
            }
        }

        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        if log.metadata()?.len() != durable_len {
            // Windows append handles cannot truncate a torn tail. The
            // lifecycle lock keeps this repair exclusive; close the append
            // handle, use a dedicated write handle for the exact truncation,
            // then reopen append-only before accepting the next commit.
            drop(log);
            let repair = OpenOptions::new().write(true).open(&path)?;
            repair.set_len(durable_len)?;
            repair.sync_all()?;
            drop(repair);
            sync_directory(dir)?;
            log = OpenOptions::new().read(true).append(true).open(&path)?;
        } else if durable_len == 0 {
            // Persist creation of an empty log before it can acknowledge data.
            sync_directory(dir)?;
        }
        Ok(Self {
            log: Mutex::new(log),
            inner: replay.finish(),
            clock,
            write_lock: tokio::sync::Mutex::new(()),
            _lifecycle_lock: lifecycle_lock,
        })
    }

    fn append(&self, entry: &StoredCommitLogEntry) -> Result<(), Error> {
        let line = serde_json::to_string(entry)
            .map_err(|err| Error::Rejected(format!("serialize commit: {err}")))?;
        let mut file = self
            .log
            .lock()
            .map_err(|_| Error::Rejected("fs commit log poisoned".to_string()))?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|err| Error::Rejected(format!("append commit: {err}")))
    }
}

#[async_trait]
impl Coordinator for FsCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        let _writing = self.write_lock.lock().await;
        // The private inner model cannot change between inspection and apply:
        // every writer holds this append lock and the retained lifecycle lock
        // excludes another process. Reuse its complete shape, terminal, global
        // sequence, and event-cursor admission before writing any durable byte.
        self.inner.inspect_commit(&commit)?;
        // Durable first: append + fsync the record before it is acknowledged, so a
        // crash after ack cannot lose it and a crash before ack replays nothing.
        let committed_at_unix_ms = Some(awaken_time::WallClockExt::now_ms(&*self.clock));
        self.append(&StoredCommitLogEntry {
            committed_at_unix_ms,
            entry: CommitLogEntry::Commit(commit.clone()),
        })?;
        // Then advance the read model; sequence matches the replayed order.
        self.inner.commit_at(commit, committed_at_unix_ms).await
    }
}

#[async_trait]
impl OperationCoordinator for FsCommitCoordinator {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        let _writing = self.write_lock.lock().await;
        if let Some(duplicate) = self.inner.inspect_operation(&operation)? {
            return Ok(duplicate);
        }
        let committed_at_unix_ms = Some(awaken_time::WallClockExt::now_ms(&*self.clock));
        self.append(&StoredCommitLogEntry {
            committed_at_unix_ms,
            entry: CommitLogEntry::Operation(operation.clone()),
        })?;
        self.inner
            .commit_operation_at(operation, committed_at_unix_ms)
            .await
    }
}

impl CommittedThreadView for FsCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.inner.committed_messages(thread_id)
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        self.inner.resume_ticket(run_id)
    }

    fn open_wait_for_thread(&self, thread_id: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        self.inner.open_wait_for_thread(thread_id)
    }

    fn run(&self, run_id: &RunId) -> Option<RunRecord> {
        self.inner.run(run_id)
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.inner.latest_run(thread_id)
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        self.inner.run_state(run_id)
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        self.inner.committed_state(thread_id)
    }
}

#[async_trait]
impl CheckpointReader for FsCommitCoordinator {
    async fn list_events(
        &self,
        scope: &EventScope,
        from: Option<u64>,
        limit: usize,
    ) -> Result<Vec<EventRecord>, String> {
        self.inner.list_events(scope, from, limit).await
    }
}

#[async_trait]
impl RunRecoverySource for FsCommitCoordinator {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        self.inner
            .recovery_snapshot(thread_id, claimed_run_id)
            .await
    }
}

/// Filesystem-backed [`StreamCheckpointStore`]: one JSON file per `run_id` under
/// a directory. Unlike the commit log this is mutable key-value state, so a `put`
/// writes a temp file and atomically renames it over the target (a crash mid-write
/// leaves the prior checkpoint intact, never a torn one), and `delete` removes it.
/// Durable across process restarts — this is the backend that makes an interrupted
/// inference step recoverable in a fresh process. Only a missing file reads as
/// `Observed::Absent`; I/O and decoding errors read as `Observed::Unknown` so
/// Runtime cannot mistake unavailable recovery evidence for a fresh invocation.
pub struct FsStreamCheckpointStore {
    dir: PathBuf,
}

impl FsStreamCheckpointStore {
    /// Open (creating if needed) a checkpoint directory rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        sync_directory(&dir)?;
        Ok(Self { dir })
    }

    /// The checkpoint file path for `run_id`. The id is filename-encoded so an
    /// opaque or path-bearing run id cannot escape the directory or collide:
    /// every byte outside `[A-Za-z0-9._-]` becomes `%XX` (reversible, injective).
    fn path_for(&self, run_id: &str) -> PathBuf {
        let mut name = String::with_capacity(run_id.len() + 5);
        for byte in run_id.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
                name.push(byte as char);
            } else {
                name.push_str(&format!("%{byte:02X}"));
            }
        }
        name.push_str(".json");
        self.dir.join(name)
    }
}

#[async_trait]
impl StreamCheckpointStore for FsStreamCheckpointStore {
    async fn get(&self, run_id: &str) -> Observed<StreamCheckpoint> {
        let bytes = match std::fs::read(self.path_for(run_id)) {
            Ok(bytes) => bytes,
            // Only a missing file is evidence of absence. Any other I/O failure
            // leaves us ignorant of whether a partial is on disk.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Observed::Absent,
            Err(error) => return Observed::Unknown(UnknownCause::Storage(error.to_string())),
        };
        // A file that exists but will not decode is corruption, not emptiness.
        match serde_json::from_slice(&bytes) {
            Ok(checkpoint) => Observed::Present(checkpoint),
            Err(error) => Observed::Unknown(UnknownCause::Storage(error.to_string())),
        }
    }

    async fn put(&self, checkpoint: StreamCheckpoint) -> Result<(), StreamCheckpointError> {
        let path = self.path_for(&checkpoint.run_id);
        let json = serde_json::to_vec(&checkpoint)
            .map_err(|error| StreamCheckpointError::Storage(error.to_string()))?;
        // Atomic replace: write a sibling temp, fsync it, then rename over the
        // target so a reader never observes a partial write.
        // Each writer owns a distinct sibling temp. A fixed `.tmp` name lets two
        // same-run puts rename/remove one another's in-progress file.
        let sequence = CHECKPOINT_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), sequence));
        let write = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)?;
            sync_directory(&self.dir)
        })();
        if let Err(error) = write {
            let _ = std::fs::remove_file(&tmp);
            return Err(StreamCheckpointError::Storage(error.to_string()));
        }
        Ok(())
    }

    async fn delete(&self, run_id: &str) -> Result<(), StreamCheckpointError> {
        match std::fs::remove_file(self.path_for(run_id)) {
            Ok(()) => sync_directory(&self.dir)
                .map_err(|error| StreamCheckpointError::Storage(error.to_string())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(StreamCheckpointError::Storage(error.to_string())),
        }
    }
}

#[cfg(test)]
mod recovery_plan_tests {
    use super::*;

    // Layout-only coverage: test builds retain this existing replay table;
    // non-test/Kani builds retain the same production owner and proof harness.
    // No behavior changes, so Clippy plus the existing full crate suite own
    // validation; no additional runtime decision-table rule is introduced.

    #[test]
    fn replay_plan_has_no_identity_or_time_remapping_branch() {
        // Cause/effect decision table: C1 known time + operation identity -> E1
        // both unchanged; C2 legacy unknown time + plain entry -> E2 None and
        // identity unchanged. Kani generalizes both rules to all u8/u64 values.
        assert_eq!(
            recovery_replay_plan(7_u8, Some(41_u64)),
            RecoveryReplayPlan {
                entry: 7,
                committed_at: Some(41)
            },
            "C1/E1"
        );
        assert_eq!(
            recovery_replay_plan(9_u8, None::<u64>),
            RecoveryReplayPlan {
                entry: 9,
                committed_at: None
            },
            "C2/E2"
        );
    }
}
