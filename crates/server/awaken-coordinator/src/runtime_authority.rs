//! Coordinator-owned Runtime persistence adapters.
//!
//! This is the only Coordinator module that turns Runtime deployment configuration into
//! concrete commit, dispatch, wake, and stream-checkpoint stores. The Runtime
//! Host receives the resulting capability through `RuntimeAuthority` and never
//! opens a database itself.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_agent_contract::agent::awaiting::ResumeTicket;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeed, RunLifecycleFeedError, RunLifecyclePage,
};
use awaken_run_ingress::{AnyDispatchStore, WakeSignal};
use awaken_runtime_host::{
    DeploymentConfig, DispatchBackend, LocalCommit, LocalCommitAdapter, LocalCommitQueries,
    RuntimeAuthority, RuntimeAuthorityError, StoreKind, Wake,
};
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

#[derive(Clone, Copy)]
pub(super) enum SchemaAccess {
    Migrate,
    Verify,
}

pub struct DurableRuntimeAuthority {
    commit: CommitAuthorityConfig,
    dispatch: Arc<AnyDispatchStore>,
    wake: Option<Arc<dyn WakeSignal>>,
    postgres_commit: Option<Arc<PostgresCommitCoordinator>>,
    postgres_local_commit: Option<Arc<dyn LocalCommit>>,
    /// One live projection per local Thread. Reopening the same SQLite/FS
    /// authority would create competing in-memory projections over one durable
    /// log, so the Coordinator owns and reuses the handle here.
    local_commits: tokio::sync::Mutex<std::collections::HashMap<String, Arc<dyn LocalCommit>>>,
}

/// The minimal immutable configuration retained by the durable commit owner.
/// Worker sandbox, ACP, upstream, capture, and execution-pool settings never
/// enter this authority object.
#[derive(Clone)]
struct CommitAuthorityConfig {
    store: StoreKind,
    storage_dir: Option<PathBuf>,
}

impl From<&DeploymentConfig> for CommitAuthorityConfig {
    fn from(deployment: &DeploymentConfig) -> Self {
        Self {
            store: deployment.store,
            storage_dir: deployment.storage_dir.clone(),
        }
    }
}

impl DurableRuntimeAuthority {
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open(
        deployment: &DeploymentConfig,
        schema: SchemaAccess,
    ) -> Result<Arc<Self>, String> {
        let postgres_pool = if deployment.dispatch_backend == DispatchBackend::Postgres
            || deployment.store == StoreKind::Postgres
        {
            Some(
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(deployment.postgres_max_connections.get())
                    .connect(required_database_url(
                        deployment,
                        "Postgres Runtime authority",
                    )?)
                    .await
                    .map_err(|error| format!("connect Coordinator Postgres pool: {error}"))?,
            )
        } else {
            None
        };
        Self::open_with_postgres_pool(deployment, schema, postgres_pool).await
    }

    /// Open every Runtime persistence adapter over the process's one
    /// process pool when Postgres is selected. SQLite callers pass no pool.
    pub async fn open_with_postgres_pool(
        deployment: &DeploymentConfig,
        schema: SchemaAccess,
        postgres_pool: Option<sqlx::PgPool>,
    ) -> Result<Arc<Self>, String> {
        let (dispatch, wake) = open_dispatch(deployment, schema, postgres_pool.clone()).await?;
        let postgres_commit = if deployment.store == StoreKind::Postgres {
            let pool = postgres_pool.ok_or_else(|| {
                "Postgres commit requires the process-owned Coordinator pool".to_owned()
            })?;
            let commit = match schema {
                SchemaAccess::Migrate => PostgresCommitCoordinator::with_pool(pool).await,
                SchemaAccess::Verify => PostgresCommitCoordinator::with_existing_pool(pool).await,
            }
            .map_err(|error| error.to_string())?;
            Some(Arc::new(commit))
        } else {
            None
        };
        let postgres_local_commit = postgres_commit.clone().map(postgres_local_commit);
        Ok(Arc::new(Self {
            commit: CommitAuthorityConfig::from(deployment),
            dispatch,
            wake,
            postgres_commit,
            postgres_local_commit,
            local_commits: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }))
    }
}

/// The one Coordinator-owned durable commit layout. Both opening and existence
/// probing call this function so a sanitized Session id cannot be reused while
/// its authoritative store already exists.
fn thread_commit_path(store: StoreKind, root: &std::path::Path, thread: &str) -> PathBuf {
    let stem = thread_path_stem(thread);
    if store == StoreKind::Fs {
        root.join(stem)
    } else {
        root.join(format!("{stem}.db"))
    }
}

pub async fn migrate(deployment: &DeploymentConfig) -> Result<(), String> {
    if deployment.dispatch_backend == DispatchBackend::Postgres {
        AnyDispatchStore::connect_postgres(
            required_database_url(deployment, "Postgres dispatch")?,
            deployment.postgres_max_connections.get(),
        )
        .await
        .map(drop)?;
    }
    if deployment.store == StoreKind::Postgres {
        PostgresCommitCoordinator::migrate(
            required_database_url(deployment, "Postgres commit")?,
            deployment.postgres_max_connections.get(),
        )
        .await
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn required_database_url<'a>(
    deployment: &'a DeploymentConfig,
    component: &str,
) -> Result<&'a str, String> {
    deployment
        .database_url
        .as_deref()
        .ok_or_else(|| format!("{component} requires runtime.database_url"))
}

async fn open_dispatch(
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
    postgres_pool: Option<sqlx::PgPool>,
) -> Result<(Arc<AnyDispatchStore>, Option<Arc<dyn WakeSignal>>), String> {
    match deployment.dispatch_backend {
        DispatchBackend::Sqlite => {
            let root = deployment.storage_dir.as_deref().ok_or_else(|| {
                "Coordinator SQLite dispatch requires runtime.storage_dir; refusing volatile dispatch authority"
                    .to_owned()
            })?;
            std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
            let store = AnyDispatchStore::open_sqlite(&root.join("dispatch.db").to_string_lossy())?;
            Ok((Arc::new(store), None))
        }
        DispatchBackend::Postgres => {
            let pool = postgres_pool.ok_or_else(|| {
                "Postgres dispatch requires the process-owned Coordinator pool".to_owned()
            })?;
            match deployment.wake {
                Wake::None => {
                    let store = match schema {
                        SchemaAccess::Migrate => AnyDispatchStore::with_postgres_pool(pool).await?,
                        SchemaAccess::Verify => {
                            AnyDispatchStore::with_existing_postgres_pool(pool).await?
                        }
                    };
                    Ok((Arc::new(store), None))
                }
                Wake::PgNotify => {
                    let (store, wake) = match schema {
                        SchemaAccess::Migrate => {
                            AnyDispatchStore::with_postgres_pool_and_wake(
                                pool,
                                &deployment.wake_channel,
                            )
                            .await?
                        }
                        SchemaAccess::Verify => {
                            AnyDispatchStore::with_existing_postgres_pool_and_wake(
                                pool,
                                &deployment.wake_channel,
                            )
                            .await?
                        }
                    };
                    Ok((Arc::new(store), Some(wake)))
                }
                Wake::Nats => open_nats_dispatch(pool, deployment, schema).await,
            }
        }
    }
}

#[cfg(feature = "nats")]
async fn open_nats_dispatch(
    pool: sqlx::PgPool,
    deployment: &DeploymentConfig,
    schema: SchemaAccess,
) -> Result<(Arc<AnyDispatchStore>, Option<Arc<dyn WakeSignal>>), String> {
    let nats_url = deployment
        .nats_url
        .as_deref()
        .ok_or_else(|| "NATS dispatch wake requires runtime.nats_url".to_owned())?;
    let store = match schema {
        SchemaAccess::Migrate => AnyDispatchStore::with_postgres_pool(pool).await?,
        SchemaAccess::Verify => AnyDispatchStore::with_existing_postgres_pool(pool).await?,
    };
    let wake: Arc<dyn WakeSignal> = Arc::new(
        awaken_run_ingress::NatsWakeSignal::connect(nats_url, deployment.wake_channel.clone())
            .await
            .map_err(|error| error.to_string())?,
    );
    Ok((Arc::new(store), Some(wake)))
}

#[cfg(not(feature = "nats"))]
async fn open_nats_dispatch(
    _pool: sqlx::PgPool,
    _deployment: &DeploymentConfig,
    _schema: SchemaAccess,
) -> Result<(Arc<AnyDispatchStore>, Option<Arc<dyn WakeSignal>>), String> {
    Err("NATS dispatch wake requested but awaken-coordinator was built without `nats`".to_owned())
}

#[async_trait::async_trait]
impl RuntimeAuthority for DurableRuntimeAuthority {
    async fn open_commit(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn LocalCommit>, RuntimeAuthorityError> {
        match self.commit.store {
            StoreKind::Postgres => self.postgres_local_commit.clone().ok_or_else(|| {
                RuntimeAuthorityError::misconfigured(
                    "Postgres commit authority was not opened at startup",
                )
            }),
            StoreKind::Fs | StoreKind::Sqlite => {
                let mut commits = self.local_commits.lock().await;
                if let Some(commit) = commits.get(thread) {
                    return Ok(commit.clone());
                }
                let path = self
                    .commit_path(thread)
                    .map_err(RuntimeAuthorityError::misconfigured)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| RuntimeAuthorityError::unavailable(error.to_string()))?;
                }
                let commit: Arc<dyn LocalCommit> = match self.commit.store {
                    StoreKind::Fs => Arc::new(LocalCommitAdapter::projected(
                        FsCommitCoordinator::open(&path).await.map_err(|error| {
                            RuntimeAuthorityError::unavailable(error.to_string())
                        })?,
                    )),
                    StoreKind::Sqlite => Arc::new(LocalCommitAdapter::projected(
                        SqliteCommitCoordinator::open(&path.to_string_lossy()).map_err(
                            |error| RuntimeAuthorityError::unavailable(error.to_string()),
                        )?,
                    )),
                    StoreKind::Postgres => unreachable!("handled above"),
                };
                commits.insert(thread.to_owned(), commit.clone());
                Ok(commit)
            }
        }
    }

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RuntimeAuthorityError> {
        match self.commit.store {
            StoreKind::Postgres => self
                .postgres_commit
                .as_ref()
                .ok_or_else(|| {
                    RuntimeAuthorityError::misconfigured(
                        "Postgres commit authority was not opened at startup",
                    )
                })?
                .authoritative_thread_exists(&ThreadId(thread.to_owned()))
                .await
                .map_err(|error| RuntimeAuthorityError::unavailable(error.to_string())),
            StoreKind::Fs | StoreKind::Sqlite => {
                let path = self
                    .commit_path(thread)
                    .map_err(RuntimeAuthorityError::misconfigured)?;
                match std::fs::metadata(path) {
                    Ok(_) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(RuntimeAuthorityError::unavailable(format!(
                        "inspect durable thread authority: {error}"
                    ))),
                }
            }
        }
    }

    fn dispatch_store(&self) -> Arc<AnyDispatchStore> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn WakeSignal>> {
        self.wake.clone()
    }

    fn stream_checkpoint(
        &self,
        thread: &str,
    ) -> Result<Arc<dyn StreamCheckpointStore>, RuntimeAuthorityError> {
        if self.commit.store == StoreKind::Postgres {
            return self
                .dispatch
                .stream_checkpoint_store()
                .map(|store| store as Arc<dyn StreamCheckpointStore>)
                .ok_or_else(|| {
                    RuntimeAuthorityError::misconfigured(
                        "Postgres runtime requires the checkpoint store paired with dispatch",
                    )
                });
        }
        let root = self.commit.storage_dir.as_deref().ok_or_else(|| {
            RuntimeAuthorityError::misconfigured("stream checkpoints require runtime.storage_dir")
        })?;
        let path = root
            .join(thread_path_stem(thread))
            .join("stream-checkpoints");
        FsStreamCheckpointStore::open(&path)
            .map(|store| Arc::new(store) as Arc<dyn StreamCheckpointStore>)
            .map_err(|error| RuntimeAuthorityError::unavailable(error.to_string()))
    }
}

impl DurableRuntimeAuthority {
    fn commit_path(&self, thread: &str) -> Result<PathBuf, String> {
        let root = self
            .commit
            .storage_dir
            .as_deref()
            .ok_or_else(|| "local commit authority requires runtime.storage_dir".to_owned())?;
        Ok(thread_commit_path(self.commit.store, root, thread))
    }
}

fn thread_path_stem(thread: &str) -> String {
    use std::fmt::Write as _;

    let mut stem = String::with_capacity(thread.len());
    for byte in thread.bytes() {
        if byte.is_ascii_alphanumeric() {
            stem.push(char::from(byte));
        } else {
            write!(&mut stem, "_{byte:02x}").expect("writing to String cannot fail");
        }
    }
    if stem.is_empty() {
        stem.push_str("_empty");
    }
    stem
}

struct PostgresQueries;

/// Erase an already-open PostgreSQL commit coordinator behind the canonical
/// Coordinator-owned [`LocalCommit`] query policy.
///
/// Product compositions use this inlet when they own process-pool startup but
/// must retain authoritative cross-process reads. The concrete query policy
/// stays private so there is one PostgreSQL read path and no downstream SQL or
/// projection fallback to synchronize.
#[must_use]
pub fn postgres_local_commit(store: Arc<PostgresCommitCoordinator>) -> Arc<dyn LocalCommit> {
    Arc::new(LocalCommitAdapter::with_queries(store, PostgresQueries))
}

#[async_trait::async_trait]
impl LocalCommitQueries<PostgresCommitCoordinator> for PostgresQueries {
    async fn authoritative_run(
        &self,
        store: &PostgresCommitCoordinator,
        run_id: &RunId,
    ) -> Result<Option<RunRecord>, String> {
        store
            .authoritative_run_record(run_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn authoritative_committed_messages(
        &self,
        store: &PostgresCommitCoordinator,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        store
            .authoritative_committed_messages(thread_id)
            .await
            .map_err(|error| error.to_string())
    }

    async fn open_wait_for_thread(
        &self,
        store: &PostgresCommitCoordinator,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        store
            .authoritative_open_wait_for_thread(thread)
            .await
            .map_err(|error| error.to_string())
    }

    async fn events_after(
        &self,
        store: &PostgresCommitCoordinator,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        store.events_after(cursor, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_local_commit_factory_keeps_the_query_policy_erased() {
        // Factory-boundary cause/effect decision table:
        // | Cause | Effect |
        // | Coordinator opens Postgres itself | DurableRuntimeAuthority calls the factory |
        // | product already owns the opened coordinator | the same factory returns LocalCommit |
        // | downstream attempts to select query behavior | impossible: policy type stays private |
        // This compile-time signature test deliberately needs no database: the
        // store integration suite owns SQL behavior, while this test owns the
        // public composition boundary and prevents leaking PostgresQueries.
        let factory: fn(Arc<PostgresCommitCoordinator>) -> Arc<dyn LocalCommit> =
            postgres_local_commit;
        let _ = factory;
    }

    #[test]
    fn local_layout_and_postgres_presence_follow_one_authority_decision_table() {
        // Local-layout FMECA / cause-effect graph: C1 SQLite/FS has a storage
        // root; C2 Postgres has an opened shared coordinator; C3 distinct thread
        // ids differ only by path-hostile bytes. Effects: E1 existence probes
        // exactly the canonical commit path; E2 Postgres never probes a local
        // path; E3 C3 remains collision-free. F1 aliasing two threads into one
        // authority (S10,O3,D3,RPN90) is eliminated by byte-wise escaping.
        // Rules: R1 C1&&!C2 -> E1; R2 C2 -> E2; R3 C1+C3 -> E3.
        let root = std::path::Path::new("/data");
        assert_eq!(
            thread_commit_path(StoreKind::Sqlite, root, "a/b"),
            root.join("a_2fb.db"),
            "R1"
        );
        assert_eq!(
            thread_commit_path(StoreKind::Fs, root, "a/b"),
            root.join("a_2fb"),
            "R1"
        );
        assert_ne!(
            thread_commit_path(StoreKind::Sqlite, root, "a/b"),
            thread_commit_path(StoreKind::Sqlite, root, "a_b"),
            "R3/E3"
        );
        assert_ne!(thread_path_stem(""), thread_path_stem("_empty"), "R3/E3");
        assert_ne!(
            thread_path_stem("会话"),
            thread_path_stem("_e4_bc_9a_e8_af_9d"),
            "R3/E3"
        );
    }

    /// Runtime-authority cause/effect graph: C1 the Coordinator selects SQLite;
    /// C2 a durable storage root exists; C3 NATS wake is selected without the
    /// compile-time capability. Effects: E1 reject volatile queue/commit
    /// authority before serving; E2 open exactly one injected dispatch/commit/
    /// checkpoint capability under the root; E3 fail closed before dialing a
    /// database instead of silently degrading to polling.
    ///
    /// | Rule | SQLite | storage root | NATS feature | Effect |
    /// |---|---|---|---|---|
    /// | A1 | yes | no | n/a | E1 |
    /// | A2 | yes | yes | n/a | E2 |
    /// | A3 | no | n/a | absent | E3 |
    #[tokio::test]
    async fn runtime_authority_startup_decision_table() {
        let missing = DeploymentConfig::ephemeral();
        let missing_error =
            match DurableRuntimeAuthority::open(&missing, SchemaAccess::Migrate).await {
                Ok(_) => panic!("A1 must reject a volatile production authority"),
                Err(error) => error,
            };
        assert!(missing_error.contains("runtime.storage_dir"), "A1/E1");

        let root = tempfile::tempdir().expect("A2 storage root");
        let mut durable = DeploymentConfig::ephemeral();
        durable.storage_dir = Some(root.path().to_path_buf());
        let authority = DurableRuntimeAuthority::open(&durable, SchemaAccess::Migrate)
            .await
            .expect("A2/E2");
        authority.open_commit("a/b").await.expect("A2 commit");
        authority.stream_checkpoint("a/b").expect("A2 checkpoint");
        assert!(root.path().join("dispatch.db").exists(), "A2 dispatch");
        assert!(root.path().join("a_2fb.db").exists(), "A2 commit");

        #[cfg(not(feature = "nats"))]
        {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://must-not-be-dialed.invalid/db")
                .expect("syntactically valid lazy pool");
            let error = match open_nats_dispatch(pool, &durable, SchemaAccess::Verify).await {
                Ok(_) => panic!("A3 must reject a missing NATS capability"),
                Err(error) => error,
            };
            assert!(error.contains("without `nats`"), "A3/E3: {error}");
        }
    }

    /// Local commit-handle cause/effect graph: C1 two callers open the same
    /// Thread concurrently; C2 callers open distinct Threads; C3 the selected
    /// local backend is SQLite or FS. Effects: E1 C1 receives one pointer-identical
    /// projection; E2 C2 receives independent authorities; E3 both backends obey
    /// the same ownership rule.
    ///
    /// | Rule | same Thread | concurrent | backend | Effect |
    /// |---|---|---|---|---|
    /// | H1 | yes | yes | SQLite | E1,E3 |
    /// | H2 | yes | yes | FS | E1,E3 |
    /// | H3 | no | any | SQLite/FS | E2,E3 |
    ///
    /// FMECA: reopening one durable log as two projected coordinators lets a
    /// recovered child commit through projection B while its waiting parent
    /// polls stale projection A (severity critical, occurrence occasional,
    /// detection poor). Caching at the sole Coordinator authority removes the
    /// second projection instead of trying to synchronize it.
    #[tokio::test]
    async fn local_commit_authority_reuses_one_projection_per_thread() {
        for (backend, rule) in [(StoreKind::Sqlite, "H1"), (StoreKind::Fs, "H2")] {
            let root = tempfile::tempdir().expect("local authority root");
            let mut deployment = DeploymentConfig::ephemeral();
            deployment.storage_dir = Some(root.path().to_path_buf());
            deployment.store = backend;
            let authority = DurableRuntimeAuthority::open(&deployment, SchemaAccess::Migrate)
                .await
                .expect(rule);

            let (left, right) = tokio::join!(
                authority.open_commit("parent"),
                authority.open_commit("parent")
            );
            let left = left.expect(rule);
            let right = right.expect(rule);
            assert!(Arc::ptr_eq(&left, &right), "{rule}/E1");

            let child = authority.open_commit("child").await.expect("H3");
            assert!(!Arc::ptr_eq(&left, &child), "H3/E2: {backend:?}");
        }
    }
}
