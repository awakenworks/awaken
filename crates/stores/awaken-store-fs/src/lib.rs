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

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::state::Command as StateCommand;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::agent::waiting::WaitingTicket;
use awaken_agent_contract::commit::coordinator::{Coordinator, Error};
use awaken_agent_contract::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::event::record::Record as EventRecord;
use awaken_agent_contract::store::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::store::run_store::RunStore;
use awaken_agent_contract::store::stream_checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_agent_contract::store::thread_reader::ThreadReader;
use awaken_store_inmem::MemoryCommitCoordinator;

const LOG_FILE: &str = "commits.ndjson";

/// A filesystem `CommitCoordinator` + `CheckpointReader`. Durable truth is the
/// append-only log; reads are served from an in-memory model rebuilt from it.
pub struct FsCommitCoordinator {
    log: Mutex<File>,
    inner: MemoryCommitCoordinator,
}

impl FsCommitCoordinator {
    /// Open (creating if needed) a store rooted at `dir`, replaying the committed
    /// log into the read model. A torn final line is discarded (crash recovery).
    pub async fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let path: PathBuf = dir.join(LOG_FILE);

        let inner = MemoryCommitCoordinator::new();
        if let Ok(file) = File::open(&path) {
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(_) => break, // torn tail: stop at the last durable line
                };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<ThreadCommit>(&line) {
                    Ok(commit) => {
                        // Replaying through the tested read model reconstructs the
                        // same committed state (and sequence) the writer produced.
                        inner
                            .commit(commit)
                            .await
                            .map_err(|err| std::io::Error::other(err.to_string()))?;
                    }
                    Err(_) => break, // torn/partial final record: discard and stop
                }
            }
        }

        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            log: Mutex::new(log),
            inner,
        })
    }
}

#[async_trait]
impl Coordinator for FsCommitCoordinator {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        // Durable first: append + fsync the record before it is acknowledged, so a
        // crash after ack cannot lose it and a crash before ack replays nothing.
        let line = serde_json::to_string(&commit)
            .map_err(|err| Error::Rejected(format!("serialize commit: {err}")))?;
        {
            let mut file = self
                .log
                .lock()
                .map_err(|_| Error::Rejected("fs commit log poisoned".to_string()))?;
            file.write_all(line.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
                .map_err(|err| Error::Rejected(format!("append commit: {err}")))?;
        }
        // Then advance the read model; sequence matches the replayed order.
        self.inner.commit(commit).await
    }
}

impl ThreadReader for FsCommitCoordinator {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        self.inner.committed_messages(thread_id)
    }

    fn waiting_ticket(&self, run_id: &RunId) -> Option<WaitingTicket> {
        self.inner.waiting_ticket(run_id)
    }

    fn committed_state(&self, thread_id: &ThreadId) -> Vec<StateCommand> {
        self.inner.committed_state(thread_id)
    }
}

impl RunStore for FsCommitCoordinator {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        self.inner.get(id)
    }
}

impl CheckpointReader for FsCommitCoordinator {
    fn run(&self, id: &RunId) -> Option<RunRecord> {
        self.inner.run(id)
    }

    fn latest_run(&self, thread_id: &ThreadId) -> Option<RunRecord> {
        self.inner.latest_run(thread_id)
    }

    fn list_events(&self, scope: &EventScope, from: Option<u64>, limit: usize) -> Vec<EventRecord> {
        self.inner.list_events(scope, from, limit)
    }
}

/// Filesystem-backed [`StreamCheckpointStore`]: one JSON file per `run_id` under
/// a directory. Unlike the commit log this is mutable key-value state, so a `put`
/// writes a temp file and atomically renames it over the target (a crash mid-write
/// leaves the prior checkpoint intact, never a torn one), and `delete` removes it.
/// Durable across process restarts — this is the backend that makes an interrupted
/// inference step resumable in a fresh process. Best-effort per the port contract:
/// every error is swallowed (a `get` fault reads as "no checkpoint").
pub struct FsStreamCheckpointStore {
    dir: PathBuf,
}

impl FsStreamCheckpointStore {
    /// Open (creating if needed) a checkpoint directory rooted at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
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
    async fn get(&self, run_id: &str) -> Option<StreamCheckpoint> {
        let bytes = std::fs::read(self.path_for(run_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    async fn put(&self, checkpoint: StreamCheckpoint) {
        let path = self.path_for(&checkpoint.run_id);
        let Ok(json) = serde_json::to_vec(&checkpoint) else {
            return;
        };
        // Atomic replace: write a sibling temp, fsync it, then rename over the
        // target so a reader never observes a partial write.
        let tmp = path.with_extension("json.tmp");
        let write = (|| -> std::io::Result<()> {
            let mut file = File::create(&tmp)?;
            file.write_all(&json)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    async fn delete(&self, run_id: &str) {
        match std::fs::remove_file(self.path_for(run_id)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
}
