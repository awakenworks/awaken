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
use awaken_agent_contract::agent::run::{Id as RunId, Record as RunRecord, RunState};
use awaken_agent_contract::agent::state::Command;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::stream::checkpoint::StreamCheckpointStore;
use awaken_agent_contract::thread::commit::coordinator::{
    Coordinator, Error, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_agent_contract::thread::read::checkpoint::CheckpointReader;
use awaken_agent_contract::thread::read::lifecycle::{
    RunLifecycleCursor, RunLifecycleFeed, RunLifecycleFeedError, RunLifecyclePage,
};
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_agent_contract::thread::read::run_store::RunStore;
use awaken_agent_contract::thread::read::thread_reader::ThreadReader;
use awaken_run_ingress::{AnyDispatchStore, WakeSignal};
use awaken_runtime_host::{
    DeploymentConfig, DispatchBackend, LocalCommit, ProjectedLocalCommit, RuntimeAuthority,
    StoreKind, Wake,
};
use awaken_store_fs::{FsCommitCoordinator, FsStreamCheckpointStore};
use awaken_store_postgres::PostgresCommitCoordinator;
use awaken_store_sqlite::SqliteCommitCoordinator;

#[derive(Clone, Copy)]
pub enum SchemaAccess {
    Migrate,
    Verify,
}

pub struct DurableRuntimeAuthority {
    deployment: DeploymentConfig,
    dispatch: Arc<AnyDispatchStore>,
    wake: Option<Arc<dyn WakeSignal>>,
    postgres_commit: Option<Arc<PostgresCommitCoordinator>>,
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

    /// Open every Runtime persistence adapter over the composition root's one
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
        Ok(Arc::new(Self {
            deployment: deployment.clone(),
            dispatch,
            wake,
            postgres_commit,
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
    async fn open_commit(&self, thread: &str) -> Result<Arc<dyn LocalCommit>, String> {
        match self.deployment.store {
            StoreKind::Postgres => self
                .postgres_commit
                .clone()
                .map(|store| Arc::new(PostgresCommit(store)) as Arc<dyn LocalCommit>)
                .ok_or_else(|| "Postgres commit authority was not opened at startup".to_owned()),
            StoreKind::Fs => {
                let path = self.commit_path(thread)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                let store = FsCommitCoordinator::open(&path)
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Arc::new(ProjectedLocalCommit(store)))
            }
            StoreKind::Sqlite => {
                let path = self.commit_path(thread)?;
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                let store = SqliteCommitCoordinator::open(&path.to_string_lossy())
                    .map_err(|error| error.to_string())?;
                Ok(Arc::new(SqliteCommit(store)))
            }
        }
    }

    fn durable_thread_exists(&self, thread: &str) -> bool {
        match self.deployment.store {
            StoreKind::Postgres => self.postgres_commit.as_ref().is_some_and(|store| {
                CheckpointReader::latest_run(store.as_ref(), &ThreadId(thread.to_owned())).is_some()
            }),
            StoreKind::Fs | StoreKind::Sqlite => {
                self.commit_path(thread).is_ok_and(|path| path.exists())
            }
        }
    }

    fn dispatch_store(&self) -> Arc<AnyDispatchStore> {
        self.dispatch.clone()
    }

    fn dispatch_wake(&self) -> Option<Arc<dyn WakeSignal>> {
        self.wake.clone()
    }

    fn stream_checkpoint(&self, thread: &str) -> Result<Arc<dyn StreamCheckpointStore>, String> {
        if self.deployment.store == StoreKind::Postgres {
            return self
                .dispatch
                .stream_checkpoint_store()
                .map(|store| store as Arc<dyn StreamCheckpointStore>)
                .ok_or_else(|| {
                    "Postgres runtime requires the checkpoint store paired with dispatch".to_owned()
                });
        }
        let root = self
            .deployment
            .storage_dir
            .as_deref()
            .ok_or_else(|| "stream checkpoints require runtime.storage_dir".to_owned())?;
        let path = root
            .join(thread_path_stem(thread))
            .join("stream-checkpoints");
        FsStreamCheckpointStore::open(&path)
            .map(|store| Arc::new(store) as Arc<dyn StreamCheckpointStore>)
            .map_err(|error| error.to_string())
    }
}

impl DurableRuntimeAuthority {
    fn commit_path(&self, thread: &str) -> Result<PathBuf, String> {
        let root = self
            .deployment
            .storage_dir
            .as_deref()
            .ok_or_else(|| "local commit authority requires runtime.storage_dir".to_owned())?;
        Ok(thread_commit_path(self.deployment.store, root, thread))
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

struct SqliteCommit(SqliteCommitCoordinator);

struct PostgresCommit(Arc<PostgresCommitCoordinator>);

#[async_trait::async_trait]
impl LocalCommit for SqliteCommit {
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String> {
        Ok(RunStore::get(&self.0, run_id))
    }
    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        Ok(ThreadReader::committed_messages(&self.0, thread_id))
    }
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        CheckpointReader::latest_run(&self.0, thread)
    }
    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        Ok(SqliteCommitCoordinator::open_wait_for_thread(
            &self.0, thread,
        ))
    }
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        RunLifecycleFeed::events_after(&self.0, cursor, limit).await
    }
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        Coordinator::commit(&self.0, commit).await
    }
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        OperationCoordinator::commit_operation(&self.0, operation).await
    }
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        ThreadReader::committed_messages(&self.0, thread_id)
    }
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        ThreadReader::resume_ticket(&self.0, run_id)
    }
    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        ThreadReader::run_state(&self.0, run_id)
    }
    fn committed_state(&self, thread_id: &ThreadId) -> Vec<Command> {
        ThreadReader::committed_state(&self.0, thread_id)
    }
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        RunStore::get(&self.0, id)
    }
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        RunRecoverySource::recovery_snapshot(&self.0, thread_id, claimed_run_id).await
    }
}

#[async_trait::async_trait]
impl LocalCommit for PostgresCommit {
    async fn authoritative_run(&self, run_id: &RunId) -> Result<Option<RunRecord>, String> {
        self.0
            .authoritative_run_record(run_id)
            .await
            .map_err(|error| error.to_string())
    }
    async fn authoritative_committed_messages(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>, String> {
        PostgresCommitCoordinator::authoritative_committed_messages(&self.0, thread_id)
            .await
            .map_err(|error| error.to_string())
    }
    fn latest_run(&self, thread: &ThreadId) -> Option<RunRecord> {
        CheckpointReader::latest_run(self.0.as_ref(), thread)
    }
    async fn open_wait_for_thread(
        &self,
        thread: &ThreadId,
    ) -> Result<Option<(RunId, ResumeTicket)>, String> {
        PostgresCommitCoordinator::authoritative_open_wait_for_thread(&self.0, thread)
            .await
            .map_err(|error| error.to_string())
    }
    async fn events_after(
        &self,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunLifecycleFeedError> {
        RunLifecycleFeed::events_after(self.0.as_ref(), cursor, limit).await
    }
    async fn commit(&self, commit: ThreadCommit) -> Result<CommitRecord, Error> {
        Coordinator::commit(self.0.as_ref(), commit).await
    }
    async fn commit_operation(&self, operation: CommitOperation) -> Result<CommitReceipt, Error> {
        OperationCoordinator::commit_operation(self.0.as_ref(), operation).await
    }
    fn committed_messages(&self, thread_id: &ThreadId) -> Vec<Message> {
        ThreadReader::committed_messages(self.0.as_ref(), thread_id)
    }
    fn resume_ticket(&self, run_id: &RunId) -> Option<ResumeTicket> {
        ThreadReader::resume_ticket(self.0.as_ref(), run_id)
    }
    fn run_state(&self, run_id: &RunId) -> Option<RunState> {
        ThreadReader::run_state(self.0.as_ref(), run_id)
    }
    fn committed_state(&self, thread_id: &ThreadId) -> Vec<Command> {
        ThreadReader::committed_state(self.0.as_ref(), thread_id)
    }
    fn get(&self, id: &RunId) -> Option<RunRecord> {
        RunStore::get(self.0.as_ref(), id)
    }
    async fn recovery_snapshot(
        &self,
        thread_id: &ThreadId,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        RunRecoverySource::recovery_snapshot(self.0.as_ref(), thread_id, claimed_run_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
