//! The per-thread commit boundary, in-memory or durable.
//!
//! A session's commit coordinator is the source of committed truth. It is either
//! an in-memory coordinator (tests, ephemeral sessions) or a durable one — SQLite
//! or the filesystem append-log — behind which an awaiting run and its history
//! survive a process restart. One concrete wrapper type keeps this a
//! composition-root choice: it coerces to `Arc<dyn Coordinator>` / `&dyn
//! ThreadReader` without trait upcasting, while exposing the two extra reads the
//! host projects — the awaiting position (to recover a session after a restart) and
//! the committed outcome rounds.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::audit::kind::Kind;
use awaken_agent_contract::thread::commit::coordinator::{Coordinator, Error};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::{CheckpointReader, EventScope};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use std::sync::Arc;

use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_store_fs::FsCommitCoordinator;
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// One thread's commit boundary: either a `Local` read+write store (an
/// interchangeable memory/sqlite/fs/postgres backend behind `Arc<dyn HostStore>`,
/// chosen at the composition root) or the write-only `Remote` worker boundary. The
/// four local backends are polymorphic — the enum only discriminates the one real
/// distinction (locally readable vs remote write-only), not the backend.
pub(crate) enum HostCommit {
    /// One of the interchangeable local backends (memory / sqlite / fs / postgres):
    /// polymorphic implementations of the same read+write store, chosen once at the
    /// composition root behind a trait object — no per-backend dispatch here.
    Local(Arc<dyn HostStore>),
    /// The database-less worker's boundary: `commit` posts facts to the cell server
    /// (the single writer) over HTTP. It is **write-only** — the worker holds no
    /// store, so it is deliberately NOT a [`HostStore`] and has no reads. Correct
    /// for a fresh, self-contained run (the activation carries its input); a resume
    /// needing remote reads is a separate async-reader redesign.
    Remote(crate::commit_ingest::RemoteCoordinator),
}

/// The host's read+write store over one interchangeable local backend: the commit
/// boundary plus the fact-derived reads the session substrate needs after a
/// restart. The four backends implement it; the composition root picks one as
/// `Arc<dyn HostStore>`. The remote worker boundary is write-only and is not a
/// `HostStore`.
pub(crate) trait HostStore: Coordinator + ThreadReader + RunStore + Send + Sync {
    /// The awaiting run on `thread`, if any, recovered from committed truth.
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)>;
    /// Payloads of committed `Continuation` (outcome-round) events for `thread`, in
    /// commit order — projected from durable truth so round history survives a restart.
    fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value>;
}

/// Recover the awaiting position from a durable backend's fact-derived read model
/// (`CheckpointReader`): the latest run on the thread plus its committed ticket.
fn awaiting_from_reader<R: CheckpointReader>(
    reader: &R,
    thread: &ThreadId,
) -> Option<(RunId, ResumeTicket)> {
    let run = reader.latest_run(thread)?;
    let ticket = reader.resume_ticket(&run.id)?;
    (&ticket.thread_id == thread).then_some((run.id, ticket))
}

/// Continuation payloads from any `CheckpointReader` (the fs/postgres shape).
fn continuation_from_reader<R: CheckpointReader>(
    reader: &R,
    thread: &ThreadId,
) -> Vec<serde_json::Value> {
    reader
        .list_events(&EventScope::Thread(thread.clone()), None, usize::MAX)
        .into_iter()
        .filter(|event| event.kind == Kind::Continuation)
        .map(|event| event.payload)
        .collect()
}

impl HostStore for MemoryCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let run = self.committed().latest_run?;
        let ticket = self.resume_ticket_for(&run.id)?;
        (&ticket.thread_id == thread).then_some((run.id, ticket))
    }
    fn continuation_payloads(&self, _thread: &ThreadId) -> Vec<serde_json::Value> {
        self.committed()
            .events
            .into_iter()
            .filter(|event| event.kind == Kind::Continuation)
            .map(|event| event.payload)
            .collect()
    }
}

impl HostStore for SqliteCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        // Inherent method wins over the trait method in resolution — not recursive.
        SqliteCommitCoordinator::open_wait_for_thread(self, thread)
    }
    fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        SqliteCommitCoordinator::continuation_payloads(self, thread)
    }
}

impl HostStore for FsCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        awaiting_from_reader(self, thread)
    }
    fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        continuation_from_reader(self, thread)
    }
}

impl HostStore for PostgresCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        awaiting_from_reader(self, thread)
    }
    fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        continuation_from_reader(self, thread)
    }
}

impl HostCommit {
    /// The awaiting run on `thread`, if any, recovered from committed truth. After a
    /// restart the durable variants read their hydrated projection, so a rebuilt
    /// session can restore its awaiting position and be resumed.
    pub(crate) fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        match self {
            HostCommit::Local(store) => store.open_wait_for_thread(thread),
            HostCommit::Remote(_) => None,
        }
    }

    /// Payloads of committed `Continuation` (outcome-round) events for `thread`, in
    /// commit order — projected from durable truth, so the round history survives a
    /// restart.
    pub(crate) fn continuation_payloads(&self, thread: &ThreadId) -> Vec<serde_json::Value> {
        match self {
            HostCommit::Local(store) => store.continuation_payloads(thread),
            HostCommit::Remote(_) => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for HostCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        match self {
            HostCommit::Local(store) => store.commit(commit).await,
            HostCommit::Remote(remote) => remote.commit(commit).await,
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Local(store) => store.committed_messages(thread_id),
            HostCommit::Remote(_) => Vec::new(),
        }
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        match self {
            HostCommit::Local(store) => store.resume_ticket(run_id),
            HostCommit::Remote(_) => None,
        }
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        match self {
            HostCommit::Local(store) => store.run_state(run_id),
            HostCommit::Remote(_) => None,
        }
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        match self {
            HostCommit::Local(store) => store.committed_state(thread_id),
            HostCommit::Remote(_) => Vec::new(),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Local(store) => store.get(id),
            HostCommit::Remote(_) => None,
        }
    }
}

/// Select the shared Postgres commit backend (ADR-0022 D6), or fail closed when it
/// was not initialised at startup. Extracted from `build_commit` so both the
/// initialised and the misconfigured paths are unit-testable without a `SharedHost`.
pub(crate) fn postgres_commit_or_err() -> Result<HostCommit, crate::host::HostError> {
    commit_or_err(crate::commit_backend::shared_postgres_commit())
}

/// Wrap the (maybe-initialised) shared coordinator into a `HostCommit`, or fail
/// closed. Takes the coordinator as a parameter so both the initialised (`Some`) and
/// the misconfigured (`None`) branches are testable without the process-global.
fn commit_or_err(
    coord: Option<Arc<PostgresCommitCoordinator>>,
) -> Result<HostCommit, crate::host::HostError> {
    let coord = coord.ok_or_else(|| {
        crate::host::HostError::internal(
            "AWAKEN_STORE=postgres requires init_shared_postgres_commit() at process \
             startup (with AWAKEN_DATABASE_URL)",
        )
    })?;
    Ok(HostCommit::Local(coord))
}

/// Whether the shared Postgres commit coordinator holds a committed run for `thread`.
/// Extracted so the postgres branch of `durable_thread_exists` is unit-testable
/// without mutating the process `AWAKEN_STORE` env.
pub(crate) fn durable_thread_exists_postgres(thread: &ThreadId) -> bool {
    crate::commit_backend::shared_postgres_commit()
        .and_then(|c| c.latest_run(thread))
        .is_some()
}

/// A filesystem-safe database filename stem for a thread id (durable store).
pub(crate) fn sanitize_thread(thread: &str) -> String {
    thread
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// The on-disk path a thread's commit boundary occupies under `dir` for a
/// non-Postgres backend: the fs backend keys a per-thread **directory**, the sqlite
/// backend a per-thread `<stem>.db` **file**. This is the SINGLE source of the
/// durable layout — both `plan_commit` (where the boundary is created) and
/// `durable_thread_exists` (the collision probe) derive the path here, so the probe
/// can never drift from where the commit boundary actually lives (a drift would let a
/// candidate session id be judged "not durable" and be reused for a different thread).
pub(crate) fn thread_commit_path(
    store: crate::deployment_config::StoreKind,
    dir: &std::path::Path,
    thread: &str,
) -> std::path::PathBuf {
    use crate::deployment_config::StoreKind;
    if store == StoreKind::Fs {
        dir.join(sanitize_thread(thread))
    } else {
        dir.join(format!("{}.db", sanitize_thread(thread)))
    }
}

/// The commit backend a thread resolves to, decided PURELY from config — no I/O, no
/// process-global, no env read. Extracted from `build_commit` so the whole
/// backend-selection decision table (including the fail-closed rows) is unit-testable
/// without a `SharedHost`. `build_commit` matches on this and performs the I/O.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommitPlan {
    /// Database-less worker: every thread commits to the cell server's ingest at `url`.
    Remote(String),
    /// The shared Postgres coordinator (resolved from the process-global at build time,
    /// which itself fails closed when uninitialised).
    Postgres,
    /// Filesystem append-log directory for the thread.
    Fs(std::path::PathBuf),
    /// Per-thread SQLite database file.
    Sqlite(std::path::PathBuf),
    /// In-memory ephemeral coordinator: the intended mode when no store dir is set and
    /// the backend is the default/sqlite (tests, ephemeral sessions).
    Memory,
    /// Fail closed: `AWAKEN_STORE=fs` was selected but there is no storage dir. The
    /// filesystem append-log has no in-memory form, so silently using an ephemeral
    /// memory store would drop committed history on restart (data loss).
    FsNeedsStorageDir,
}

/// Decide a thread's commit backend from the deployment axes alone (see [`CommitPlan`]).
/// Ordering mirrors the historic `build_commit`: a database-less worker (`upstream`)
/// wins first, then the shared Postgres backend, then the on-disk fs/sqlite layout —
/// with the no-store-dir case splitting into the fail-closed fs row and the ephemeral
/// memory row.
pub(crate) fn plan_commit(
    store: crate::deployment_config::StoreKind,
    store_dir: Option<&std::path::Path>,
    upstream: Option<&str>,
    thread: &str,
) -> CommitPlan {
    use crate::deployment_config::StoreKind;
    if let Some(url) = upstream {
        return CommitPlan::Remote(url.to_string());
    }
    if store == StoreKind::Postgres {
        return CommitPlan::Postgres;
    }
    let Some(dir) = store_dir else {
        // No store dir: the default/sqlite path is the intended ephemeral mode, but an
        // explicit fs selection with no dir is a misconfiguration — fail closed rather
        // than silently drop committed history on restart.
        return if store == StoreKind::Fs {
            CommitPlan::FsNeedsStorageDir
        } else {
            CommitPlan::Memory
        };
    };
    match store {
        StoreKind::Fs => CommitPlan::Fs(thread_commit_path(store, dir, thread)),
        // Postgres is handled above; sqlite (and the default) land here.
        _ => CommitPlan::Sqlite(thread_commit_path(store, dir, thread)),
    }
}

/// True when the durable store under `store_dir` already holds `thread`,
/// WITHOUT opening it: mirrors the commit-boundary layout exactly (fs backend
/// keys a per-thread directory, default SQLite a per-thread db file; no store
/// dir = nothing durable). Session-id minting probes through this so a
/// candidate id is never materialized as a side effect (a prematurely built
/// session context would lack the session's agent config and MCP tools).
pub(crate) fn durable_thread_exists(store_dir: Option<&std::path::Path>, thread: &str) -> bool {
    use crate::deployment_config::{DeploymentConfig, StoreKind};
    let store = DeploymentConfig::from_env().store;
    // Shared Postgres backend: the coordinator is keyed by thread, so a committed
    // run for the thread means it durably exists (no per-thread file to stat).
    if store == StoreKind::Postgres {
        return durable_thread_exists_postgres(&ThreadId(thread.to_string()));
    }
    let Some(dir) = store_dir else {
        return false;
    };
    // Derive the path through the SAME helper `build_commit` uses, so the probe can
    // never disagree with where the boundary is actually created.
    thread_commit_path(store, dir, thread).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_or_err_fails_closed_without_the_shared_coordinator() {
        // AWAKEN_STORE=postgres but init_shared_postgres_commit was never called →
        // build_commit fails closed rather than silently using an ephemeral store.
        assert!(commit_or_err(None).is_err());
    }

    #[test]
    fn durable_thread_exists_routes_to_postgres_when_selected() {
        // AWAKEN_STORE=postgres routes the probe to the shared coordinator. With no
        // coordinator initialised here it reads false, but this exercises the backend
        // dispatch (the postgres branch of durable_thread_exists). Set-and-restore is
        // safe: only this crate's durable_thread_exists reads AWAKEN_STORE in a unit
        // test, and it is called nowhere else here.
        // SAFETY: single-threaded within this test's critical section; restored below.
        unsafe { std::env::set_var("AWAKEN_STORE", "postgres") };
        let exists = durable_thread_exists(None, "no-such-thread-xyz");
        unsafe { std::env::remove_var("AWAKEN_STORE") };
        assert!(!exists, "an unknown thread has no committed run");
    }

    use crate::deployment_config::StoreKind;
    use std::path::{Path, PathBuf};

    // --- thread_commit_path: the single durable-layout source ------------------

    #[test]
    fn fs_layout_is_a_per_thread_directory_sqlite_a_db_file() {
        let dir = Path::new("/data");
        // Fs backend → a per-thread directory named by the sanitized stem (no suffix).
        assert_eq!(
            thread_commit_path(StoreKind::Fs, dir, "t1"),
            PathBuf::from("/data/t1")
        );
        // Sqlite backend → a per-thread `<stem>.db` file.
        assert_eq!(
            thread_commit_path(StoreKind::Sqlite, dir, "t1"),
            PathBuf::from("/data/t1.db")
        );
    }

    #[test]
    fn layout_sanitizes_non_alphanumeric_thread_ids() {
        // A thread id with path-hostile chars maps to a filesystem-safe stem, so the
        // probe and the boundary agree on where an `acp:foo/bar` thread lives.
        let dir = Path::new("/data");
        assert_eq!(
            thread_commit_path(StoreKind::Sqlite, dir, "acp:foo/bar"),
            PathBuf::from("/data/acp_foo_bar.db")
        );
        assert_eq!(
            thread_commit_path(StoreKind::Fs, dir, "acp:foo/bar"),
            PathBuf::from("/data/acp_foo_bar")
        );
    }

    // --- plan_commit: the full backend-selection decision table ----------------

    #[test]
    fn plan_upstream_worker_wins_over_every_local_backend() {
        // A database-less worker commits to the cell server's ingest — even when a
        // store dir or the postgres backend is also configured, upstream is first.
        assert_eq!(
            plan_commit(
                StoreKind::Postgres,
                Some(Path::new("/data")),
                Some("http://cell"),
                "t"
            ),
            CommitPlan::Remote("http://cell".to_string())
        );
    }

    #[test]
    fn plan_postgres_ignores_the_store_dir() {
        // The shared coordinator is keyed by thread, not a per-thread file, so a store
        // dir is irrelevant to the postgres decision.
        assert_eq!(
            plan_commit(StoreKind::Postgres, None, None, "t"),
            CommitPlan::Postgres
        );
        assert_eq!(
            plan_commit(StoreKind::Postgres, Some(Path::new("/data")), None, "t"),
            CommitPlan::Postgres
        );
    }

    #[test]
    fn plan_fs_with_a_dir_is_a_thread_directory() {
        assert_eq!(
            plan_commit(StoreKind::Fs, Some(Path::new("/data")), None, "t1"),
            CommitPlan::Fs(PathBuf::from("/data/t1"))
        );
    }

    #[test]
    fn plan_fs_without_a_dir_fails_closed_not_ephemeral() {
        // The regression that matters: an explicit fs backend with NO store dir must
        // fail closed rather than silently resolve to an ephemeral memory store (which
        // would drop committed history on restart — the data-loss footgun).
        assert_eq!(
            plan_commit(StoreKind::Fs, None, None, "t1"),
            CommitPlan::FsNeedsStorageDir
        );
    }

    #[test]
    fn plan_sqlite_with_a_dir_is_a_db_file() {
        assert_eq!(
            plan_commit(StoreKind::Sqlite, Some(Path::new("/data")), None, "t1"),
            CommitPlan::Sqlite(PathBuf::from("/data/t1.db"))
        );
    }

    #[test]
    fn plan_sqlite_without_a_dir_is_the_intended_ephemeral_mode() {
        // The default/sqlite no-dir case is the documented ephemeral mode (unit tests,
        // throwaway sessions) — NOT a footgun, so it stays memory rather than an error.
        assert_eq!(
            plan_commit(StoreKind::Sqlite, None, None, "t1"),
            CommitPlan::Memory
        );
    }

    #[tokio::test]
    async fn postgres_commit_and_thread_probe_after_init() {
        let Ok(url) = std::env::var("AWAKEN_TEST_PG_URL") else {
            eprintln!("skip: AWAKEN_TEST_PG_URL unset");
            return;
        };
        crate::commit_backend::init_shared_postgres_commit(&url)
            .await
            .expect("init");
        // Selecting the postgres backend now yields a HostCommit::Postgres.
        assert!(matches!(
            postgres_commit_or_err().expect("ok after init"),
            HostCommit::Local(_)
        ));
        // The thread probe: an unknown thread has no committed run in the shared DB.
        assert!(!durable_thread_exists_postgres(&ThreadId(
            "no-such-thread-xyz".to_string()
        )));
    }
}
