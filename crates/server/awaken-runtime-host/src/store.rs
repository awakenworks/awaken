//! The per-thread commit boundary, in-memory or durable.
//!
//! A session's commit coordinator is the source of committed truth. It is either
//! an in-memory coordinator (tests, ephemeral sessions) or a durable one — SQLite
//! or the filesystem append-log — behind which an awaiting run and its history
//! survive a process restart. One concrete wrapper type keeps this a
//! composition-root choice: it coerces to `Arc<dyn Coordinator>` / `&dyn
//! ThreadReader` without trait upcasting, while exposing the two extra reads the
//! host projects — the awaiting position (to recover a session after a restart) and
//! the awaiting position used for restart recovery.

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use std::sync::Arc;

use awaken_runtime::memory::MemoryCommitCoordinator;
use awaken_store_fs::FsCommitCoordinator;
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

/// One thread's commit boundary: either a `Local` read+write store (an
/// interchangeable memory/sqlite/fs/postgres backend behind `Arc<dyn HostStore>`,
/// chosen at the composition root) or the `Remote` Worker boundary backed by a
/// non-authoritative recovery projection. The four local backends are polymorphic
/// — the enum only discriminates local authoritative reads from projected remote
/// reads, not the backend.
pub(crate) enum HostCommit {
    /// One of the interchangeable local backends (memory / sqlite / fs / postgres):
    /// polymorphic implementations of the same read+write store, chosen once at the
    /// composition root behind a trait object — no per-backend dispatch here.
    Local(Arc<dyn HostStore>),
    /// The database-independent Worker's read boundary. Authoritative writes go
    /// through the attempt's claim-fenced operation coordinator; reads use the
    /// [`awaken_run_ingress::RecoveryProjection`]. It deliberately is not a
    /// [`HostStore`]: the projection is an execution cache, never authoritative
    /// storage or an alternate commit path.
    Remote(RemoteHostCommit),
}

pub(crate) struct RemoteHostCommit {
    projection: Arc<awaken_run_ingress::RecoveryProjection>,
}

impl RemoteHostCommit {
    pub(crate) fn new() -> Self {
        Self {
            projection: Arc::new(awaken_run_ingress::RecoveryProjection::new()),
        }
    }
}

/// The host's read+write store over one interchangeable local backend: the commit
/// boundary plus the fact-derived reads the session substrate needs after a
/// restart. The four backends implement it; the composition root picks one as
/// `Arc<dyn HostStore>`. The remote Worker boundary is projected and is not a
/// `HostStore`.
pub(crate) trait HostStore:
    Coordinator + OperationCoordinator + CheckpointReader + RunStore + RunRecoverySource + Send + Sync
{
    /// The awaiting run on `thread`, if any, recovered from committed truth.
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)>;
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

impl HostStore for MemoryCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        let run = self.committed().latest_run?;
        let ticket = self.resume_ticket_for(&run.id)?;
        (&ticket.thread_id == thread).then_some((run.id, ticket))
    }
}

impl HostStore for SqliteCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        // Inherent method wins over the trait method in resolution — not recursive.
        SqliteCommitCoordinator::open_wait_for_thread(self, thread)
    }
}

impl HostStore for FsCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        awaiting_from_reader(self, thread)
    }
}

impl HostStore for PostgresCommitCoordinator {
    fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        awaiting_from_reader(self, thread)
    }
}

impl HostCommit {
    pub(crate) fn lifecycle_feed(
        &self,
    ) -> Option<awaken_agent_contract::CheckpointRunLifecycleFeed> {
        match self {
            HostCommit::Local(store) => {
                let reader: Arc<dyn CheckpointReader> = store.clone();
                Some(awaken_agent_contract::CheckpointRunLifecycleFeed::new(
                    reader,
                ))
            }
            HostCommit::Remote(_) => None,
        }
    }

    /// Latest committed run from authoritative local storage or the remote
    /// Worker's non-authoritative recovery projection.
    pub(crate) fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        match self {
            HostCommit::Local(store) => CheckpointReader::latest_run(store.as_ref(), thread),
            HostCommit::Remote(remote) => remote.projection.current().and_then(|snapshot| {
                snapshot
                    .latest_run_id
                    .and_then(|latest| snapshot.runs.into_iter().find(|run| run.id == latest))
            }),
        }
    }

    /// The awaiting run on `thread`, if any, recovered from committed truth. After a
    /// restart the durable variants read their hydrated projection, so a rebuilt
    /// session can restore its awaiting position and be resumed.
    pub(crate) fn open_wait_for_thread(&self, thread: &ThreadId) -> Option<(RunId, ResumeTicket)> {
        match self {
            HostCommit::Local(store) => store.open_wait_for_thread(thread),
            HostCommit::Remote(remote) => remote.projection.current().and_then(|snapshot| {
                snapshot
                    .resume_tickets
                    .into_iter()
                    .find(|entry| entry.ticket.thread_id == *thread)
                    .map(|entry| (entry.run_id, entry.ticket))
            }),
        }
    }

    pub(crate) fn recovery_projection(
        &self,
    ) -> Option<Arc<awaken_run_ingress::RecoveryProjection>> {
        match self {
            HostCommit::Local(_) => None,
            HostCommit::Remote(remote) => Some(remote.projection.clone()),
        }
    }
}

#[async_trait::async_trait]
impl Coordinator for HostCommit {
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        match self {
            HostCommit::Local(store) => store.commit(commit).await,
            HostCommit::Remote(_) => Err(Error::Rejected(
                "a remote Worker must commit through its claim-fenced operation coordinator"
                    .to_string(),
            )),
        }
    }
}

#[async_trait::async_trait]
impl OperationCoordinator for HostCommit {
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        match self {
            HostCommit::Local(store) => store.commit_operation(operation).await,
            HostCommit::Remote(_) => Err(Error::Rejected(
                "a remote Worker cannot coordinate authoritative commit operations".to_string(),
            )),
        }
    }
}

impl ThreadReader for HostCommit {
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        match self {
            HostCommit::Local(store) => store.committed_messages(thread_id),
            HostCommit::Remote(remote) => remote.projection.committed_messages(thread_id),
        }
    }

    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        match self {
            HostCommit::Local(store) => store.resume_ticket(run_id),
            HostCommit::Remote(remote) => remote.projection.resume_ticket(run_id),
        }
    }

    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        match self {
            HostCommit::Local(store) => store.run_state(run_id),
            HostCommit::Remote(remote) => remote.projection.run_state(run_id),
        }
    }

    fn committed_state(
        &self,
        thread_id: &ThreadId,
    ) -> Vec<awaken_agent_contract::agent::state::Command> {
        match self {
            HostCommit::Local(store) => store.committed_state(thread_id),
            HostCommit::Remote(remote) => remote.projection.committed_state(thread_id),
        }
    }
}

impl RunStore for HostCommit {
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        match self {
            HostCommit::Local(store) => store.get(id),
            HostCommit::Remote(remote) => remote.projection.get(id),
        }
    }
}

#[async_trait::async_trait]
impl RunRecoverySource for HostCommit {
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        match self {
            HostCommit::Local(store) => store.recovery_snapshot(thread_id, claimed_run_id).await,
            HostCommit::Remote(remote) => remote
                .projection
                .current()
                .filter(|snapshot| {
                    &snapshot.thread_id == thread_id && &snapshot.claimed_run_id == claimed_run_id
                })
                .ok_or_else(|| {
                    RecoveryError::Rejected(
                        "remote recovery projection is not loaded for this claim".to_string(),
                    )
                }),
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
            "DeploymentConfig::store=Postgres requires init_shared_postgres_commit() at process \
             startup (with DeploymentConfig::database_url)",
        )
    })?;
    Ok(HostCommit::Local(coord))
}

/// Whether the shared Postgres commit coordinator holds a committed run for `thread`.
/// Extracted so the postgres branch of `durable_thread_exists` is unit-testable
/// without mutating the process `DeploymentConfig::store` env.
pub(crate) fn durable_thread_exists_postgres(thread: &ThreadId) -> bool {
    crate::commit_backend::shared_postgres_commit()
        .and_then(|c| CheckpointReader::latest_run(c.as_ref(), thread))
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
    /// Fail closed: `DeploymentConfig::store=Fs` was selected but there is no storage dir. The
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
pub(crate) fn durable_thread_exists_with_store(
    store: crate::deployment_config::StoreKind,
    store_dir: Option<&std::path::Path>,
    thread: &str,
) -> bool {
    use crate::deployment_config::StoreKind;
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
        // DeploymentConfig::store=Postgres but init_shared_postgres_commit was never called →
        // build_commit fails closed rather than silently using an ephemeral store.
        assert!(commit_or_err(None).is_err());
    }

    #[test]
    fn durable_thread_exists_routes_to_postgres_when_selected() {
        // Drive the pure backend-selection seam directly. Tests must never mutate a
        // process-global deployment variable while parallel Host tests are building
        // commits from that same environment.
        let exists = durable_thread_exists_with_store(
            crate::deployment_config::StoreKind::Postgres,
            None,
            "no-such-thread-xyz",
        );
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
