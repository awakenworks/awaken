// @formal entrypoint session.delete_tombstone_requires_hidden_terminal_verified_cleanup
// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session.owner_saved_atomically
// @formal entrypoint session.visible_row_always_owned
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal entrypoint webhook.committed_outbox_row_is_recoverable
// @formal entrypoint webhook.lifecycle_commit_and_outbox_are_atomic
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )
)]
#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

mod creation_intents;
mod reads;
#[cfg(test)]
mod transaction_crash_tests;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_scoped_migration::{
    LedgerSchema, MigrationBundle, MigrationError, check_ledger_version,
};
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::SqliteConnection;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotFileIdentity {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SnapshotFileIdentity {
    fn read(metadata: &fs::Metadata, path: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().map_err(|error| {
                format!(
                    "inspect Session SQLite snapshot member {} modification time: {error}",
                    path.display()
                )
            })?,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

struct SnapshotSource {
    path: PathBuf,
    destination: PathBuf,
    file: File,
    identity: SnapshotFileIdentity,
}

struct SqliteProbeSnapshot {
    database: PathBuf,
    _directory: tempfile::TempDir,
}

impl SqliteProbeSnapshot {
    fn capture(path: &Path) -> Result<Self, String> {
        let directory = tempfile::Builder::new()
            .prefix("awaken-session-sqlite-probe-")
            .tempdir()
            .map_err(|error| format!("create Session SQLite probe snapshot: {error}"))?;
        let database = directory.path().join("sessions.db");
        let mut sources = vec![
            Self::open_source(path, database.clone(), true)?
                .expect("the required Session SQLite source is present"),
        ];
        let mut sidecar_presence = Vec::new();
        for suffix in ["-wal", "-shm"] {
            let source = append_sqlite_suffix(path, suffix);
            let destination = append_sqlite_suffix(&database, suffix);
            let opened = Self::open_source(&source, destination, false)?;
            sidecar_presence.push((source, opened.is_some()));
            if let Some(opened) = opened {
                sources.push(opened);
            }
        }

        for source in &mut sources {
            let mut destination = File::create(&source.destination).map_err(|error| {
                format!(
                    "create Session SQLite probe snapshot member {}: {error}",
                    source.destination.display()
                )
            })?;
            io::copy(&mut source.file, &mut destination).map_err(|error| {
                format!(
                    "copy Session SQLite snapshot member {}: {error}",
                    source.path.display()
                )
            })?;
        }

        // All source handles stay open through the complete copy. Re-check the
        // path identities and optional-member set only after every member was
        // copied, so a concurrent checkpoint or WAL publication fails closed
        // rather than yielding a mixed physical snapshot.
        for source in &sources {
            let handle_metadata = source.file.metadata().map_err(|error| {
                format!(
                    "reinspect Session SQLite snapshot member {}: {error}",
                    source.path.display()
                )
            })?;
            ensure_unaliased_snapshot_member(&handle_metadata, &source.path)?;
            let handle_identity = SnapshotFileIdentity::read(&handle_metadata, &source.path)?;
            let path_metadata = snapshot_member_metadata(&source.path, true)?
                .expect("a required snapshot member remains present");
            let path_identity = SnapshotFileIdentity::read(&path_metadata, &source.path)?;
            if handle_identity != source.identity || path_identity != source.identity {
                return Err(format!(
                    "Session SQLite snapshot member {} changed while it was copied",
                    source.path.display()
                ));
            }
        }
        for (sidecar, was_present) in sidecar_presence {
            let is_present = snapshot_member_metadata(&sidecar, false)?.is_some();
            if is_present != was_present {
                return Err(format!(
                    "Session SQLite sidecar {} changed presence while it was copied",
                    sidecar.display()
                ));
            }
        }

        Ok(Self {
            database,
            _directory: directory,
        })
    }

    fn open_source(
        path: &Path,
        destination: PathBuf,
        required: bool,
    ) -> Result<Option<SnapshotSource>, String> {
        let Some(path_metadata) = snapshot_member_metadata(path, required)? else {
            return Ok(None);
        };
        let path_identity = SnapshotFileIdentity::read(&path_metadata, path)?;
        let file = File::open(path).map_err(|error| {
            format!(
                "open Session SQLite snapshot member {} read-only: {error}",
                path.display()
            )
        })?;
        let handle_metadata = file.metadata().map_err(|error| {
            format!(
                "inspect open Session SQLite snapshot member {}: {error}",
                path.display()
            )
        })?;
        ensure_unaliased_snapshot_member(&handle_metadata, path)?;
        let handle_identity = SnapshotFileIdentity::read(&handle_metadata, path)?;
        if handle_identity != path_identity {
            return Err(format!(
                "Session SQLite snapshot member {} changed while it was opened",
                path.display()
            ));
        }
        Ok(Some(SnapshotSource {
            path: path.to_path_buf(),
            destination,
            file,
            identity: handle_identity,
        }))
    }
}

fn append_sqlite_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn snapshot_member_metadata(path: &Path, required: bool) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if !required && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "inspect Session SQLite snapshot member {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Session SQLite snapshot member {} is a symbolic link",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "Session SQLite snapshot member {} is not a regular file",
            path.display()
        ));
    }
    ensure_unaliased_snapshot_member(&metadata, path)?;
    Ok(Some(metadata))
}

#[cfg(unix)]
fn ensure_unaliased_snapshot_member(metadata: &fs::Metadata, path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    if metadata.nlink() != 1 {
        return Err(format!(
            "Session SQLite snapshot member {} has {} hard links; aliased storage is not accepted",
            path.display(),
            metadata.nlink()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_unaliased_snapshot_member(_metadata: &fs::Metadata, _path: &Path) -> Result<(), String> {
    // Stable std does not expose a Windows hard-link count. Symlinks are still
    // rejected and the before/open/after metadata comparison remains active.
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqliteColumnShape {
    id: i64,
    name: String,
    data_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key: i64,
    hidden: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqliteIndexColumnShape {
    sequence: i64,
    column_id: i64,
    name: Option<String>,
    descending: bool,
    collation: Option<String>,
    key: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SqliteSchemaObjectShape {
    object_type: String,
    table_name: String,
    columns: Vec<SqliteColumnShape>,
    index_columns: Vec<SqliteIndexColumnShape>,
}

pub struct SqliteManagedSessionRepository {
    pub(crate) pool: EmbeddedPool,
}

impl SqliteManagedSessionRepository {
    /// Open (or create) `sessions.db` at `path` and apply the schema migrations.
    pub async fn open(path: &str) -> Result<Self, String> {
        Self::open_location(EmbeddedSqlite::file(path)).await
    }

    /// Verify that an existing SQLite file is a readable, initialized Session
    /// authority without applying migrations or normalizing rows.
    ///
    /// This is the read-only startup/doctor companion to [`Self::open`]. It uses
    /// the same migration stream selector and plan authority, accepts a valid
    /// legacy or pending-tail prefix for `open` to migrate, and rejects a blank,
    /// corrupt, or unrelated SQLite file before it can be mistaken for an empty
    /// deployment.
    pub async fn verify_existing(path: &str) -> Result<(), String> {
        // SQLite may create an empty WAL beside a clean WAL-mode database, and
        // may update an existing SHM even on a read-only connection. Snapshot
        // the complete physical read set first and confine those SQLite-owned
        // effects to the temporary copy. Copying WAL/SHM also preserves committed
        // pages not yet checkpointed into the main file. A store this process
        // dropped may still be closing, and that close checkpoints into the main
        // file, so let it finish before reading the files.
        EmbeddedSqlite::file(path).settled().await;
        let snapshot = SqliteProbeSnapshot::capture(Path::new(path))?;
        let pool = EmbeddedSqlite::read_only(&snapshot.database)
            .open()
            .await
            .map_err(|error| format!("open Session SQLite read-only: {error}"))?;
        let verified = Self::verify_snapshot(&pool).await;
        pool.close().await;
        verified
    }

    async fn verify_snapshot(pool: &EmbeddedPool) -> Result<(), String> {
        let mut conn = pool
            .acquire()
            .await
            .map_err(|error| format!("open Session SQLite read-only: {error}"))?;
        let integrity: String = sqlx::query_scalar("PRAGMA quick_check(1)")
            .fetch_one(&mut *conn)
            .await
            .map_err(|error| format!("check Session SQLite integrity: {error}"))?;
        if integrity != "ok" {
            return Err(format!(
                "Session SQLite integrity check failed: {integrity}"
            ));
        }

        let ledger = LedgerSchema::with_prefix(NS).map_err(|error| error.to_string())?;
        Self::verify_ledger_generation(&mut conn, &ledger).await?;
        let receipts = Self::migration_receipts_for(&mut conn, &ledger, BUNDLE_ID).await?;
        if receipts.is_empty() {
            return Err("Session SQLite migration bundle is not initialized".to_owned());
        }
        let selected = selected_session_schema(&receipts).map_err(|error| error.to_string())?;
        debug_assert!(selected.pre_convergence.is_none() || selected.stream.is_legacy());
        awaken_scoped_migration::plan(
            &selected.complete,
            &receipts,
            awaken_scoped_migration::Dialect::Sqlite,
        )
        .map_err(|error| error.to_string())?;
        Self::verify_schema_matches_receipts(&mut conn, &selected.complete, &receipts).await?;

        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let converged_receipts =
            Self::migration_receipts_for(&mut conn, &ledger, CONVERGED_BUNDLE_ID).await?;
        awaken_scoped_migration::plan(
            &converged,
            &converged_receipts,
            awaken_scoped_migration::Dialect::Sqlite,
        )
        .map_err(|error| error.to_string())?;

        Ok(())
    }

    /// Close the store's connection and wait until SQLite has released the
    /// file. Dropping the repository also closes it, but in the background;
    /// a caller that next inspects or rewrites the file closes it first.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// An in-memory database (tests).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, String> {
        Self::open_location(EmbeddedSqlite::memory()).await
    }

    async fn open_location(location: EmbeddedSqlite) -> Result<Self, String> {
        let pool = location.open().await.map_err(|e| e.to_string())?;
        match Self::migrate(&pool).await {
            Ok(()) => Ok(Self { pool }),
            Err(error) => {
                // Release the connection (and the rolled-back migration or
                // normalization transaction) before reporting the failure.
                pool.close().await;
                Err(error)
            }
        }
    }

    async fn migrate(pool: &EmbeddedPool) -> Result<(), String> {
        let receipts = Self::migration_receipts(pool).await?;
        let selected = selected_session_schema(&receipts).map_err(|error| error.to_string())?;
        debug_assert!(selected.pre_convergence.is_none() || selected.stream.is_legacy());
        awaken_scoped_migration::plan(
            &selected.complete,
            &receipts,
            awaken_scoped_migration::Dialect::Sqlite,
        )
        .map_err(|error| error.to_string())?;
        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
            .map_err(|error| error.to_string())?;
        if let Some(pre_convergence) = &selected.pre_convergence {
            // Every supported legacy prefix first reaches the last published
            // branch shape. V1/V9/V12 do not yet carry every column consumed by
            // normalization, so reading rows before this phase is invalid.
            awaken_sqlite_pool::held(runner.run_bundle(pre_convergence))
                .await
                .map_err(|error| error.to_string())?;
            Self::normalize_session_aggregates(pool, false)
                .await
                .map_err(|error| error.to_string())?;
        }
        awaken_sqlite_pool::held(runner.run_bundle(&selected.complete))
            .await
            .map_err(|error| error.to_string())?;
        awaken_sqlite_pool::held(runner.run_bundle(&converged))
            .await
            .map_err(|error| error.to_string())?;
        Self::normalize_session_aggregates(pool, true)
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    async fn migration_receipts(pool: &EmbeddedPool) -> Result<BTreeMap<i64, String>, String> {
        let ledger = LedgerSchema::with_prefix(NS).map_err(|error| error.to_string())?;
        let mut conn = pool.acquire().await.map_err(|error| error.to_string())?;
        if !Self::table_exists(&mut conn, ledger.ledger_table()).await? {
            return Ok(BTreeMap::new());
        }
        Self::migration_receipts_for(&mut conn, &ledger, BUNDLE_ID).await
    }

    async fn migration_receipts_for(
        conn: &mut SqliteConnection,
        ledger: &LedgerSchema,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, String> {
        let sql = format!(
            "SELECT version,checksum FROM {} WHERE bundle_id=?1 ORDER BY version",
            ledger.ledger_table()
        );
        sqlx::query_as::<_, (i64, String)>(&sql)
            .bind(bundle_id)
            .fetch_all(&mut *conn)
            .await
            .map(|rows| rows.into_iter().collect())
            .map_err(|error| error.to_string())
    }

    async fn table_exists(conn: &mut SqliteConnection, table: &str) -> Result<bool, String> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        )
        .bind(table)
        .fetch_one(&mut *conn)
        .await
        .map_err(|error| format!("inspect Session SQLite schema: {error}"))
    }

    async fn verify_ledger_generation(
        conn: &mut SqliteConnection,
        ledger: &LedgerSchema,
    ) -> Result<(), String> {
        let ledger_exists = Self::table_exists(conn, ledger.ledger_table()).await?;
        let meta_exists = Self::table_exists(conn, ledger.meta_table()).await?;
        ledger
            .verify_presence(ledger_exists, meta_exists)
            .map_err(|error| error.to_string())?;
        let sql = format!("SELECT ledger_version FROM {}", ledger.meta_table());
        let versions = sqlx::query_scalar::<_, i64>(&sql)
            .fetch_all(&mut *conn)
            .await
            .map_err(|error| error.to_string())?;
        if versions.len() != 1 {
            return Err(MigrationError::LedgerMetadataRowCount {
                meta_table: ledger.meta_table().to_owned(),
                found: versions.len(),
            }
            .to_string());
        }
        check_ledger_version(ledger.ledger_table(), versions[0]).map_err(|error| error.to_string())
    }

    async fn verify_schema_matches_receipts(
        actual: &mut SqliteConnection,
        bundle: &MigrationBundle,
        receipts: &BTreeMap<i64, String>,
    ) -> Result<(), String> {
        // Materialize the receipt-selected prefix with the canonical runner in
        // memory. This derives the expected SQLite shape from the one migration
        // authority instead of maintaining an adapter-owned table/column list.
        let applied = MigrationBundle::new(
            bundle.bundle_id(),
            bundle
                .migrations()
                .iter()
                .filter(|migration| receipts.contains_key(&migration.version()))
                .cloned()
                .collect(),
        )
        .map_err(|error| error.to_string())?;
        let expected = EmbeddedSqlite::memory()
            .open()
            .await
            .map_err(|error| error.to_string())?;
        let compared = Self::compare_receipt_shapes(actual, &expected, bundle, &applied).await;
        expected.close().await;
        compared
    }

    async fn compare_receipt_shapes(
        actual: &mut SqliteConnection,
        expected: &EmbeddedPool,
        bundle: &MigrationBundle,
        applied: &MigrationBundle,
    ) -> Result<(), String> {
        let runner = SqliteMigrationRunner::with_prefix(expected.sqlx().clone(), NS)
            .map_err(|error| error.to_string())?;
        awaken_sqlite_pool::held(runner.run_bundle(applied))
            .await
            .map_err(|error| error.to_string())?;
        let applied_shapes = {
            let mut conn = expected
                .acquire()
                .await
                .map_err(|error| error.to_string())?;
            Self::schema_object_shapes(&mut conn).await?
        };

        // Applying the remaining canonical tail to an empty in-memory shape is
        // also the ownership oracle for objects a forged database might create
        // before recording their receipts. Such premature objects would make
        // the real opener's later DDL fail even if every applied object matched.
        awaken_sqlite_pool::held(runner.run_bundle(bundle))
            .await
            .map_err(|error| error.to_string())?;
        let complete_shapes = {
            let mut conn = expected
                .acquire()
                .await
                .map_err(|error| error.to_string())?;
            Self::schema_object_shapes(&mut conn).await?
        };
        let actual_shapes = Self::schema_object_shapes(actual).await?;
        let known_names = applied_shapes
            .keys()
            .chain(complete_shapes.keys())
            .collect::<BTreeSet<_>>();
        for name in known_names {
            let expected_shape = applied_shapes.get(name);
            let actual_shape = actual_shapes.get(name);
            if actual_shape != expected_shape {
                return Err(format!(
                    "Session SQLite schema disagrees with migration receipts at object `{name}`: expected {expected_shape:?}, found {actual_shape:?}"
                ));
            }
        }
        Ok(())
    }

    async fn schema_object_shapes(
        conn: &mut SqliteConnection,
    ) -> Result<BTreeMap<String, SqliteSchemaObjectShape>, String> {
        let objects = sqlx::query_as::<_, (String, String, String)>(
            "SELECT type,name,tbl_name FROM sqlite_schema \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
        )
        .fetch_all(&mut *conn)
        .await
        .map_err(|error| error.to_string())?;
        let mut shapes = BTreeMap::new();
        for (object_type, name, table_name) in objects {
            let columns = if object_type == "table" {
                sqlx::query_as::<_, (i64, String, String, bool, Option<String>, i64, i64)>(
                    "SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden \
                     FROM pragma_table_xinfo(?1) ORDER BY cid",
                )
                .bind(&name)
                .fetch_all(&mut *conn)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(
                    |(id, name, data_type, not_null, default_value, primary_key, hidden)| {
                        SqliteColumnShape {
                            id,
                            name,
                            data_type,
                            not_null,
                            default_value,
                            primary_key,
                            hidden,
                        }
                    },
                )
                .collect()
            } else {
                Vec::new()
            };
            let index_columns = if object_type == "index" {
                sqlx::query_as::<_, (i64, i64, Option<String>, bool, Option<String>, bool)>(
                    "SELECT seqno,cid,name,desc,coll,key \
                     FROM pragma_index_xinfo(?1) ORDER BY seqno",
                )
                .bind(&name)
                .fetch_all(&mut *conn)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|(sequence, column_id, name, descending, collation, key)| {
                    SqliteIndexColumnShape {
                        sequence,
                        column_id,
                        name,
                        descending,
                        collation,
                        key,
                    }
                })
                .collect()
            } else {
                Vec::new()
            };
            shapes.insert(
                name,
                SqliteSchemaObjectShape {
                    object_type,
                    table_name,
                    columns,
                    index_columns,
                },
            );
        }
        Ok(shapes)
    }

    async fn normalize_session_aggregates(
        pool: &EmbeddedPool,
        sync_indexes: bool,
    ) -> Result<(), SessionRepositoryError> {
        // Historical pre-convergence DDL has not installed every index yet.
        // The final pass decodes each root once and atomically repairs all its
        // projections under the same writer fence as byte normalization.
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(storage)?;
        let rows = sqlx::query_as::<_, (String, Option<String>, i64)>(
            "SELECT session_id,aggregate_json,revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        for (stored_session_id, aggregate_json, revision) in rows {
            let (session, canonical) =
                normalize_published_row(&stored_session_id, aggregate_json, revision)
                    .map_err(corrupt)?;
            if let Some(canonical) = canonical {
                sqlx::query(
                    "UPDATE managed_session SET aggregate_json=?2, revision=?3 WHERE session_id=?1",
                )
                .bind(&stored_session_id)
                .bind(canonical)
                .bind(i64::try_from(session.revision.0).map_err(corrupt)?)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            }
            if sync_indexes {
                Self::sync_session_indexes(&mut tx, &session).await?;
            }
        }
        tx.commit().await.map_err(storage)
    }

    async fn sync_session_indexes(
        tx: &mut SqliteConnection,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_session_vault_reference WHERE session_id = ?1")
            .bind(session.session_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        for vault_id in referenced_vault_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_vault_reference (session_id, vault_id) \
                 VALUES (?1, ?2)",
            )
            .bind(session.session_id.as_str())
            .bind(vault_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        Self::sync_credential_source_index(tx, session).await?;
        let obligation = worker_obligation_index(session);
        if session.needs_reconciliation() {
            sqlx::query(
                "INSERT INTO managed_session_reconciliation_work \
                    (session_id, observed_revision, worker_owner, \
                     preparation_candidate, unbound_preparation, unbound_terminal, demand_invalid) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                    observed_revision = excluded.observed_revision, \
                    worker_owner = excluded.worker_owner, \
                    preparation_candidate = excluded.preparation_candidate, \
                    unbound_preparation = excluded.unbound_preparation, \
                    unbound_terminal = excluded.unbound_terminal, \
                    demand_invalid = excluded.demand_invalid",
            )
            .bind(session.session_id.as_str())
            .bind(db_revision(session.revision)?)
            .bind(obligation.owner)
            .bind(i64::from(obligation.preparation_candidate))
            .bind(i64::from(obligation.unbound_preparation))
            .bind(i64::from(obligation.unbound_terminal))
            .bind(i64::from(obligation.invalid))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        } else {
            if obligation.owner.is_some()
                || obligation.preparation_candidate
                || obligation.unbound_preparation
                || obligation.unbound_terminal
                || obligation.invalid
            {
                return Err(corrupt(
                    "Worker demand root is absent from reconciliation index",
                ));
            }
            sqlx::query("DELETE FROM managed_session_reconciliation_work WHERE session_id = ?1")
                .bind(session.session_id.as_str())
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        Ok(())
    }

    async fn sync_credential_source_index(
        tx: &mut SqliteConnection,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query(
            "DELETE FROM managed_session_credential_source_reference WHERE session_id = ?1",
        )
        .bind(session.session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for source_id in referenced_mcp_credential_source_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_credential_source_reference \
                    (session_id, credential_source_id) VALUES (?1, ?2)",
            )
            .bind(session.session_id.as_str())
            .bind(source_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        Ok(())
    }

    async fn is_tombstoned(
        tx: &mut SqliteConnection,
        session_id: &str,
    ) -> Result<bool, SessionRepositoryError> {
        sqlx::query("SELECT 1 FROM managed_session_tombstone WHERE session_id = ?1")
            .bind(session_id)
            .fetch_optional(&mut *tx)
            .await
            .map(|row| row.is_some())
            .map_err(storage)
    }

    async fn session_identity(
        tx: &mut SqliteConnection,
        session_id: &str,
    ) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
        let rows = sqlx::query_as::<_, (i64, Option<String>, Option<i64>, Option<String>)>(
            "SELECT identity_kind, scope_id, revision, aggregate_json FROM (
                SELECT 0 AS identity_kind, scope_id, revision, aggregate_json
                FROM managed_session WHERE session_id = ?1
                UNION ALL
                SELECT 1 AS identity_kind, scope_id, deleted_revision AS revision,
                       NULL AS aggregate_json
                FROM managed_session_tombstone WHERE session_id = ?1
             ) ORDER BY identity_kind",
        )
        .bind(session_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        .into_iter()
        .map(
            |(kind, owner_scope, revision, aggregate_json)| RawSessionIdentity {
                kind,
                owner_scope,
                revision,
                aggregate_json,
            },
        )
        .collect();
        transactional_session_identity(rows)
    }

    async fn create_replay(
        tx: &mut SqliteConnection,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
        missing_receipt: MissingCreateReceipt,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        let rows = sqlx::query_as::<
            _,
            (
                Option<String>,
                Option<i64>,
                Option<String>,
                i64,
                Option<String>,
                Option<i64>,
                Option<String>,
            ),
        >(
            "WITH receipt AS (
                SELECT payload_hash, committed_revision, receipt_kind
                FROM managed_session_idempotency
                WHERE session_id = ?1 AND idempotency_key = ?2
             ), identity AS (
                SELECT 0 AS identity_kind, scope_id, revision, aggregate_json
                FROM managed_session WHERE session_id = ?1
                UNION ALL
                SELECT 1 AS identity_kind, scope_id, deleted_revision AS revision,
                       NULL AS aggregate_json
                FROM managed_session_tombstone WHERE session_id = ?1
             ), snapshot AS (
                SELECT identity_kind, scope_id, revision, aggregate_json FROM identity
                UNION ALL
                SELECT -1, NULL, NULL, NULL WHERE NOT EXISTS (SELECT 1 FROM identity)
             )
             SELECT (SELECT payload_hash FROM receipt),
                    (SELECT committed_revision FROM receipt),
                    (SELECT receipt_kind FROM receipt),
                    identity_kind, scope_id, revision, aggregate_json
             FROM snapshot ORDER BY identity_kind",
        )
        .bind(session_id)
        .bind(&idempotency.key)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        .into_iter()
        .map(
            |(
                payload_hash,
                committed_revision,
                receipt_kind,
                kind,
                owner_scope,
                revision,
                aggregate_json,
            )| {
                (
                    payload_hash,
                    committed_revision,
                    receipt_kind,
                    RawSessionIdentity {
                        kind,
                        owner_scope,
                        revision,
                        aggregate_json,
                    },
                )
            },
        )
        .collect::<Vec<_>>();
        let receipt = rows
            .first()
            .ok_or_else(|| corrupt("Session create replay snapshot is empty"))?;
        let receipt = match (&receipt.0, receipt.1, &receipt.2) {
            (None, None, None) => None,
            (Some(payload_hash), Some(committed_revision), Some(kind)) => Some(
                decode_session_receipt(payload_hash.clone(), committed_revision, kind)?,
            ),
            _ => return Err(corrupt("incomplete Session create receipt")),
        };
        let identity = transactional_session_identity(
            rows.into_iter()
                .map(|(_, _, _, identity)| identity)
                .filter(|identity| identity.kind >= 0)
                .collect(),
        )?;
        classify_create_replay(
            owner_scope,
            session_id,
            idempotency,
            receipt,
            identity,
            missing_receipt,
        )
    }
}

#[async_trait]
impl SessionRepository for SqliteManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        async {
            validate_session_create_command(owner_scope, &session, &idempotency, &lifecycle_facts)?;
            // Acquire the SQLite writer reservation before reading the expected
            // revision. A deferred read-then-write transaction can otherwise fail
            // its upgrade immediately when another aggregate writes concurrently.
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(storage)?;
            if let Some(replayed) = Self::create_replay(
                &mut tx,
                owner_scope,
                session.session_id.as_str(),
                &idempotency,
                MissingCreateReceipt::AllowInsertFence,
            )
            .await?
            {
                return Ok(SessionCreateResult::Replayed(replayed));
            }
            let new_revision = SESSION_CREATE_REVISION;
            session.revision = new_revision;
            let inserted = sqlx::query(
                "INSERT INTO managed_session \
                (session_id, scope_id, revision, aggregate_json) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (session_id) DO NOTHING",
            )
            .bind(session.session_id.as_str())
            .bind(owner_scope)
            .bind(db_revision(new_revision)?)
            .bind(aggregate_str(&session)?)
            .execute(&mut *tx)
            .await
            .map_err(storage)?
            .rows_affected();
            if inserted != 1 {
                if let Some(replayed) = Self::create_replay(
                    &mut tx,
                    owner_scope,
                    session.session_id.as_str(),
                    &idempotency,
                    MissingCreateReceipt::RejectOccupied,
                )
                .await?
                {
                    return Ok(SessionCreateResult::Replayed(replayed));
                }
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::AlreadyExists,
                ));
            }
            // A concurrent delete can win after the first tombstone read. Recheck
            // in this same write transaction before any index, receipt, or outbox
            // row makes a deleted identity live again.
            if Self::is_tombstoned(&mut tx, session.session_id.as_str()).await? {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned,
                ));
            }
            Self::adopt_creation_intent(&mut tx, session.session_id.as_str()).await?;
            Self::sync_session_indexes(&mut tx, &session).await?;
            sqlx::query(
                "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
            )
            .bind(session.session_id.as_str())
            .bind(&idempotency.key)
            .bind(&idempotency.payload_hash)
            .bind(db_revision(new_revision)?)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            for fact in lifecycle_facts {
                sqlx::query(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            }
            tx.commit().await.map_err(storage)?;
            Ok(SessionCreateResult::Applied(session))
        }
        .await
        .map_err(write_not_committed)
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        if owner_scope.trim().is_empty()
            || session_id.as_str().trim().is_empty()
            || idempotency.key.trim().is_empty()
            || idempotency.payload_hash.trim().is_empty()
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create replay query".into(),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let replay = Self::create_replay(
            &mut tx,
            owner_scope,
            session_id.as_str(),
            idempotency,
            MissingCreateReceipt::RejectOccupied,
        )
        .await?;
        tx.commit().await.map_err(storage)?;
        Ok(replay)
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        async {
            let receipt_kind = mutation.receipt_kind();
            let next = mutation
                .validate()
                .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
            let session_id = mutation.payload.session_id().to_string();
            let mut tx = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(storage)?;
            if let Some((stored_hash, committed_revision, stored_kind)) = sqlx::query_as::<_, (String, i64, String)>(
                "SELECT payload_hash, committed_revision, receipt_kind FROM managed_session_idempotency
                 WHERE session_id = ?1 AND idempotency_key = ?2",
            )
            .bind(&session_id)
            .bind(&mutation.idempotency.key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
            {
                let receipt = decode_session_receipt(stored_hash, committed_revision, &stored_kind)?;
                let identity = Self::session_identity(&mut tx, &session_id).await?;
                return classify_mutation_replay(owner_scope, &receipt, &mutation, next, &identity);
            }
            let current = sqlx::query_as::<_, (i64, String, String)>(
                "SELECT revision, scope_id, aggregate_json
                 FROM managed_session WHERE session_id = ?1",
            )
            .bind(&session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
            .map(|(revision, owner, aggregate_json)| {
                (
                    revision,
                    owner,
                    EncodedSessionRow {
                        aggregate_json,
                        revision,
                    },
                )
            });
            let Some((current_revision, current_owner, current_row)) = current else {
                let tombstone_revision = sqlx::query_scalar::<_, i64>(
                    "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = ?1",
                )
                .bind(&session_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?
                .unwrap_or_default();
                let tombstone_revision = u64::try_from(tombstone_revision)
                    .map_err(|_| corrupt("negative deleted Session revision"))?;
                return Ok(SessionMutationResult::Conflict {
                    current_revision: SessionRevision(tombstone_revision),
                });
            };
            let current_revision = SessionRevision(
                u64::try_from(current_revision)
                    .map_err(|_| corrupt("negative managed Session revision"))?,
            );
            if current_owner != owner_scope || current_revision != mutation.expected_revision {
                return Ok(SessionMutationResult::Conflict { current_revision });
            }
            let (mut deleted_admissions, private_content) = validate_current_mutation(&mutation, current_row)?;
            if matches!(&mutation.payload, SessionMutationPayload::Delete(_)) {
                let noops = sqlx::query_as::<_, (String, String, i64)>(
                    "SELECT idempotency_key, payload_hash, committed_revision \
                     FROM managed_session_idempotency \
                     WHERE session_id = ?1 AND receipt_kind = ?2",
                )
                .bind(&session_id)
                .bind(receipt_kind_value(SessionCommandReceiptKind::EventNoOp))
                .fetch_all(&mut *tx)
                .await
                .map_err(storage)?;
                if !noops.is_empty() {
                    deleted_admissions = Some(event_admissions::merge_noops(
                        deleted_admissions.as_deref().expect("delete encoded its root receipts"),
                        current_revision,
                        noops,
                    )?);
                }
            }
            match &mutation.payload {
                SessionMutationPayload::Replace(replacement) => {
                    let mut replacement = replacement.clone();
                    replacement.revision = next;
                    let affected = sqlx::query(
                        "UPDATE managed_session SET
                        aggregate_json = ?2, revision = ?3
                     WHERE session_id = ?1 AND scope_id = ?4 AND revision = ?5",
                    )
                    .bind(replacement.session_id.as_str())
                    .bind(aggregate_str(&replacement)?)
                    .bind(db_revision(next)?)
                    .bind(owner_scope)
                    .bind(db_revision(current_revision)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?
                    .rows_affected();
                    if affected != 1 {
                        return Ok(SessionMutationResult::Conflict { current_revision });
                    }
                    Self::sync_session_indexes(&mut tx, &replacement).await?;
                }
                SessionMutationPayload::Delete(tombstone) => {
                    let affected = sqlx::query(
                        "DELETE FROM managed_session
                         WHERE session_id = ?1 AND scope_id = ?2 AND revision = ?3",
                    )
                    .bind(&session_id)
                    .bind(owner_scope)
                    .bind(db_revision(current_revision)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?
                    .rows_affected();
                    if affected != 1 {
                        return Ok(SessionMutationResult::Conflict { current_revision });
                    }
                    sqlx::query(
                        "INSERT INTO managed_session_tombstone
                        (session_id, scope_id, deleted_revision, deleted_at, event_admissions_json, private_content)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    )
                    .bind(tombstone.session_id.as_str())
                    .bind(owner_scope)
                    .bind(db_revision(tombstone.deleted_revision)?)
                    .bind(&tombstone.deleted_at)
                    .bind(deleted_admissions)
                    .bind(i64::from(private_content))
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                }
                SessionMutationPayload::EventNoOp { .. } => {}
            }
            sqlx::query(
                "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision, receipt_kind)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&session_id)
            .bind(&mutation.idempotency.key)
            .bind(&mutation.idempotency.payload_hash)
            .bind(db_revision(next)?)
            .bind(receipt_kind_value(receipt_kind))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            for fact in &mutation.lifecycle_facts {
                sqlx::query(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
            }
            tx.commit().await.map_err(storage)?;
            Ok(if receipt_kind == SessionCommandReceiptKind::EventNoOp {
                SessionMutationResult::AcknowledgedNoOp { observed_revision: next }
            } else {
                SessionMutationResult::Applied { new_revision: next }
            })
        }
        .await
        .map_err(write_not_committed)
    }

    async fn preparation_candidates(
        &self,
        after: Option<&awaken_session_contract::SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.scan_reconcilable(after, true).await
    }

    async fn count_environment_phase(
        &self,
        phase: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        let rows = sqlx::query_as::<_, (String, String, i64)>(ENVIRONMENT_COUNT_SQL)
            .fetch(&self.pool)
            .map_err(storage);
        count_environment_phase(rows, phase).await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &awaken_session_types::SessionId,
        key: &str,
    ) -> Result<Option<awaken_session_contract::SessionIdempotencyReceipt>, SessionRepositoryError>
    {
        let row = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT payload_hash, committed_revision, receipt_kind \
             FROM managed_session_idempotency \
             WHERE session_id = ?1 AND idempotency_key = ?2",
        )
        .bind(session_id.as_str())
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|(hash, revision, kind)| decode_session_receipt(hash, revision, &kind))
            .transpose()
    }
}

#[async_trait::async_trait]
impl SessionReconciliation for SqliteManagedSessionRepository {
    async fn reconciliation_scan(
        &self,
        after: Option<&awaken_session_contract::SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.scan_reconcilable(after, false).await
    }

    async fn worker_demand(&self) -> Result<u64, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let (owners, initial, terminal, invalid, drift) =
            sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
                "SELECT COUNT(DISTINCT work.worker_owner), \
                        COALESCE(MAX(work.unbound_preparation), 0), \
                        COALESCE(MAX(work.unbound_terminal), 0), \
                        COALESCE(MAX(work.demand_invalid), 0), \
                        COUNT(*) FILTER (WHERE work.observed_revision <> root.revision) \
                 FROM managed_session_reconciliation_work work \
                 JOIN managed_session root ON root.session_id = work.session_id",
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
        let quarantined: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM managed_session_quarantine LIMIT 1)")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        if drift != 0 || invalid != 0 || quarantined {
            return Err(corrupt(
                "Session Worker demand index is incomplete or quarantined",
            ));
        }
        let owners = u64::try_from(owners).map_err(corrupt)?;
        owners
            .checked_add(u64::from(initial != 0 || terminal != 0))
            .ok_or_else(|| storage("Session Worker demand count overflow"))
    }
}
impl SqliteManagedSessionRepository {
    async fn scan_reconcilable(
        &self,
        after: Option<&awaken_session_contract::SessionScanCursor>,
        preparation_only: bool,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let after = after.map(|cursor| cursor.session_id().to_string());
        let mut tx = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(storage)?;
        let page_size = usize::try_from(RECOVERY_BATCH_SIZE)
            .map_err(|error| storage(format!("invalid recovery batch size: {error}")))?;
        let mut scan = SessionScan::default();
        let sql = reconciliation_scan_sql(preparation_only, after.is_some(), "?1");
        let rows = sqlx::query_as::<_, (String, String, String, i64, i64)>(&sql)
            .bind(&after)
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?
            .into_iter()
            .map(
                |(workspace_id, session_id, aggregate_json, revision, observed_revision)| {
                    (
                        workspace_id,
                        session_id,
                        EncodedSessionRow {
                            aggregate_json,
                            revision,
                        },
                        observed_revision,
                    )
                },
            )
            .collect::<Vec<_>>();
        let has_more = rows.len() > page_size;
        let mut rows = rows;
        rows.truncate(page_size);
        if has_more {
            scan.next_cursor = rows.last().map(|(_, session_id, _, _)| {
                awaken_session_contract::SessionScanCursor::after_session_id(session_id)
            });
        }
        for (workspace_id, session_id, row, observed_revision) in rows {
            if observed_revision != row.revision {
                return Err(corrupt(format!(
                    "Session reconciliation revision drift for {session_id}"
                )));
            }
            let stored_revision = row.revision;
            match decode(row) {
                Ok(session) => {
                    // Quarantine records are evidence, not an absorbing
                    // lifecycle state. A newer codec may make a known old
                    // format readable, so every scan revalidates the row
                    // and clears stale isolation before returning work.
                    sqlx::query("DELETE FROM managed_session_quarantine WHERE session_id = ?1")
                        .bind(&session_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage)?;
                    if preparation_only && !session.is_run_free_preparation_candidate() {
                        return Err(corrupt(format!(
                            "Session preparation candidate index disagrees with root {session_id}"
                        )));
                    }
                    if session.needs_reconciliation() {
                        scan.sessions.push(ScopedPersistedSession {
                            workspace_id,
                            session,
                        });
                    }
                }
                Err(error) => {
                    let reason = error.to_string();
                    sqlx::query(
                        "INSERT INTO managed_session_quarantine \
                        (session_id, reason, observed_revision) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT (session_id) DO UPDATE SET \
                        reason = excluded.reason, \
                        observed_revision = excluded.observed_revision, \
                        quarantined_at = CURRENT_TIMESTAMP",
                    )
                    .bind(&session_id)
                    .bind(reason)
                    .bind(stored_revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                }
            }
        }
        scan.quarantined = sqlx::query_as::<_, (String, String)>(
            "SELECT session_id, reason FROM managed_session_quarantine \
                 ORDER BY session_id LIMIT ?1",
        )
        .bind(RECOVERY_BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|(session_id, reason)| SessionRecoveryQuarantine {
            session_id: awaken_session_types::SessionId::from(session_id),
            reason,
        })
        .collect();
        tx.commit().await.map_err(storage)?;
        Ok(scan)
    }
}

#[async_trait]
impl LifecycleOutbox for SqliteManagedSessionRepository {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        let data = lifecycle_str(&fact);
        sqlx::query(
            "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
        )
        .bind(&fact.id)
        .bind(data)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        sqlx::query_scalar::<_, String>(
            "SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .iter()
        .map(|encoded| decode_lifecycle(encoded).map_err(corrupt))
        .collect()
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_lifecycle_outbox WHERE fact_id = ?1")
            .bind(fact_id)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }
}
