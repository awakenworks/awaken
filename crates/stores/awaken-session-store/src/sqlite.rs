use super::*;
use awaken_scoped_migration::{
    LedgerSchema, MigrationBundle, MigrationError, check_ledger_version,
};
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
    pub(crate) conn: awaken_sqlite_runtime::SharedSqliteConnection,
}

impl SqliteManagedSessionRepository {
    /// Open (or create) `sessions.db` at `path` and apply the schema migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::file(path)
            .open()
            .map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    /// Verify that an existing SQLite file is a readable, initialized Session
    /// authority without applying migrations or normalizing rows.
    ///
    /// This is the read-only startup/doctor companion to [`Self::open`]. It uses
    /// the same migration stream selector and plan authority, accepts a valid
    /// legacy or pending-tail prefix for `open` to migrate, and rejects a blank,
    /// corrupt, or unrelated SQLite file before it can be mistaken for an empty
    /// deployment.
    pub fn verify_existing(path: &str) -> Result<(), String> {
        // SQLite may create an empty WAL beside a clean WAL-mode database, and
        // may update an existing SHM even on a read-only connection. Snapshot
        // the complete physical read set first and confine those SQLite-owned
        // effects to the temporary copy. Copying WAL/SHM also preserves committed
        // pages not yet checkpointed into the main file.
        let snapshot = SqliteProbeSnapshot::capture(Path::new(path))?;
        let conn = Connection::open_with_flags(
            &snapshot.database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| format!("open Session SQLite read-only: {error}"))?;
        let integrity: String = conn
            .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
            .map_err(|error| format!("check Session SQLite integrity: {error}"))?;
        if integrity != "ok" {
            return Err(format!(
                "Session SQLite integrity check failed: {integrity}"
            ));
        }

        let ledger = LedgerSchema::with_prefix(NS).map_err(|error| error.to_string())?;
        Self::verify_ledger_generation(&conn, &ledger)?;
        let receipts = Self::migration_receipts_for(&conn, &ledger, BUNDLE_ID)?;
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
        Self::verify_schema_matches_receipts(&conn, &selected.complete, &receipts)?;

        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let converged_receipts = Self::migration_receipts_for(&conn, &ledger, CONVERGED_BUNDLE_ID)?;
        awaken_scoped_migration::plan(
            &converged,
            &converged_receipts,
            awaken_scoped_migration::Dialect::Sqlite,
        )
        .map_err(|error| error.to_string())?;

        Ok(())
    }

    /// An in-memory database (tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = awaken_sqlite_runtime::SqliteConnectionFactory::memory()
            .open()
            .map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    fn from_connection(mut conn: Connection) -> Result<Self, String> {
        let receipts = Self::migration_receipts(&conn)?;
        let selected = selected_session_schema(&receipts).map_err(|error| error.to_string())?;
        debug_assert!(selected.pre_convergence.is_none() || selected.stream.is_legacy());
        awaken_scoped_migration::plan(
            &selected.complete,
            &receipts,
            awaken_scoped_migration::Dialect::Sqlite,
        )
        .map_err(|error| error.to_string())?;
        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|error| error.to_string())?;
        if let Some(pre_convergence) = &selected.pre_convergence {
            // Every supported legacy prefix first reaches the last published
            // branch shape. V1/V9/V12 do not yet carry every column consumed by
            // normalization, so reading rows before this phase is invalid.
            runner
                .run_bundle(&conn, pre_convergence)
                .map_err(|error| error.to_string())?;
            Self::normalize_session_aggregates(&mut conn).map_err(|error| error.to_string())?;
        }
        runner
            .run_bundle(&conn, &selected.complete)
            .and_then(|_| runner.run_bundle(&conn, &converged))
            .map_err(|error| error.to_string())?;
        Self::normalize_session_aggregates(&mut conn).map_err(|error| error.to_string())?;
        Self::rebuild_credential_source_index(&mut conn).map_err(|e| e.to_string())?;
        Ok(Self {
            conn: awaken_sqlite_runtime::SharedSqliteConnection::new(conn),
        })
    }

    pub(crate) async fn with_connection<T, F>(
        &self,
        operation: F,
    ) -> Result<T, SessionRepositoryError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRepositoryError> + Send + 'static,
    {
        awaken_sqlite_runtime::with_connection(self.conn.clone(), operation)
            .await
            .map_err(storage)?
    }

    fn migration_receipts(conn: &Connection) -> Result<BTreeMap<i64, String>, String> {
        let ledger = LedgerSchema::with_prefix(NS).map_err(|error| error.to_string())?;
        if !Self::table_exists(conn, ledger.ledger_table())? {
            return Ok(BTreeMap::new());
        }
        Self::migration_receipts_for(conn, &ledger, BUNDLE_ID)
    }

    fn migration_receipts_for(
        conn: &Connection,
        ledger: &LedgerSchema,
        bundle_id: &str,
    ) -> Result<BTreeMap<i64, String>, String> {
        let mut statement = conn
            .prepare(&format!(
                "SELECT version,checksum FROM {} WHERE bundle_id=?1 ORDER BY version",
                ledger.ledger_table()
            ))
            .map_err(|error| error.to_string())?;
        statement
            .query_map([bundle_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| error.to_string())?
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|error| error.to_string())
    }

    fn table_exists(conn: &Connection, table: &str) -> Result<bool, String> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect Session SQLite schema: {error}"))
    }

    fn verify_ledger_generation(conn: &Connection, ledger: &LedgerSchema) -> Result<(), String> {
        ledger
            .verify_presence(
                Self::table_exists(conn, ledger.ledger_table())?,
                Self::table_exists(conn, ledger.meta_table())?,
            )
            .map_err(|error| error.to_string())?;
        let mut statement = conn
            .prepare(&format!(
                "SELECT ledger_version FROM {}",
                ledger.meta_table()
            ))
            .map_err(|error| error.to_string())?;
        let versions = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
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

    fn verify_schema_matches_receipts(
        actual: &Connection,
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
        let expected = awaken_sqlite_runtime::SqliteConnectionFactory::memory()
            .open()
            .map_err(|error| error.to_string())?;
        let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|error| error.to_string())?;
        runner
            .run_bundle(&expected, &applied)
            .map_err(|error| error.to_string())?;
        let applied_shapes = Self::schema_object_shapes(&expected)?;

        // Applying the remaining canonical tail to an empty in-memory shape is
        // also the ownership oracle for objects a forged database might create
        // before recording their receipts. Such premature objects would make
        // the real opener's later DDL fail even if every applied object matched.
        runner
            .run_bundle(&expected, bundle)
            .map_err(|error| error.to_string())?;
        let complete_shapes = Self::schema_object_shapes(&expected)?;
        let actual_shapes = Self::schema_object_shapes(actual)?;
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

    fn schema_object_shapes(
        conn: &Connection,
    ) -> Result<BTreeMap<String, SqliteSchemaObjectShape>, String> {
        let objects = {
            let mut statement = conn
                .prepare(
                    "SELECT type,name,tbl_name FROM sqlite_schema \
                     WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
                )
                .map_err(|error| error.to_string())?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        objects
            .into_iter()
            .map(|(object_type, name, table_name)| {
                let columns = if object_type == "table" {
                    let mut statement = conn
                        .prepare(
                            "SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden \
                             FROM pragma_table_xinfo(?1) ORDER BY cid",
                        )
                        .map_err(|error| error.to_string())?;
                    statement
                        .query_map([&name], |row| {
                            Ok(SqliteColumnShape {
                                id: row.get(0)?,
                                name: row.get(1)?,
                                data_type: row.get(2)?,
                                not_null: row.get(3)?,
                                default_value: row.get(4)?,
                                primary_key: row.get(5)?,
                                hidden: row.get(6)?,
                            })
                        })
                        .map_err(|error| error.to_string())?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| error.to_string())?
                } else {
                    Vec::new()
                };
                let index_columns = if object_type == "index" {
                    let mut statement = conn
                        .prepare(
                            "SELECT seqno,cid,name,desc,coll,key \
                             FROM pragma_index_xinfo(?1) ORDER BY seqno",
                        )
                        .map_err(|error| error.to_string())?;
                    statement
                        .query_map([&name], |row| {
                            Ok(SqliteIndexColumnShape {
                                sequence: row.get(0)?,
                                column_id: row.get(1)?,
                                name: row.get(2)?,
                                descending: row.get(3)?,
                                collation: row.get(4)?,
                                key: row.get(5)?,
                            })
                        })
                        .map_err(|error| error.to_string())?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|error| error.to_string())?
                } else {
                    Vec::new()
                };
                Ok((
                    name,
                    SqliteSchemaObjectShape {
                        object_type,
                        table_name,
                        columns,
                        index_columns,
                    },
                ))
            })
            .collect()
    }

    fn normalize_session_aggregates(conn: &mut Connection) -> Result<(), SessionRepositoryError> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        let rows = {
            let mut statement = tx
                .prepare(
                    "SELECT session_id,aggregate_json,revision FROM managed_session ORDER BY session_id",
                )
                .map_err(storage)?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for (stored_session_id, aggregate_json, revision) in rows {
            if let Some(canonical) =
                normalize_published_row(&stored_session_id, aggregate_json, revision)
                    .map_err(corrupt)?
            {
                tx.execute(
                    "UPDATE managed_session SET aggregate_json=?2 WHERE session_id=?1",
                    params![stored_session_id, canonical],
                )
                .map_err(storage)?;
            }
        }
        tx.commit().map_err(storage)
    }

    fn sync_session_indexes(
        tx: &rusqlite::Transaction<'_>,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        tx.execute(
            "DELETE FROM managed_session_vault_reference WHERE session_id = ?1",
            params![session.session_id],
        )
        .map_err(storage)?;
        for vault_id in referenced_vault_ids(session) {
            tx.execute(
                "INSERT INTO managed_session_vault_reference (session_id, vault_id) \
                 VALUES (?1, ?2)",
                params![session.session_id, vault_id],
            )
            .map_err(storage)?;
        }
        Self::sync_credential_source_index(tx, session)?;
        if session.needs_reconciliation() {
            tx.execute(
                "INSERT INTO managed_session_reconciliation_work \
                    (session_id, observed_revision) VALUES (?1, ?2) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                    observed_revision = excluded.observed_revision",
                params![session.session_id, db_revision(session.revision)?],
            )
            .map_err(storage)?;
        } else {
            tx.execute(
                "DELETE FROM managed_session_reconciliation_work WHERE session_id = ?1",
                params![session.session_id],
            )
            .map_err(storage)?;
        }
        Ok(())
    }

    fn sync_credential_source_index(
        tx: &rusqlite::Transaction<'_>,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        tx.execute(
            "DELETE FROM managed_session_credential_source_reference WHERE session_id = ?1",
            params![session.session_id],
        )
        .map_err(storage)?;
        for source_id in referenced_mcp_credential_source_ids(session) {
            tx.execute(
                "INSERT INTO managed_session_credential_source_reference \
                    (session_id, credential_source_id) VALUES (?1, ?2)",
                params![session.session_id, source_id],
            )
            .map_err(storage)?;
        }
        Ok(())
    }

    fn rebuild_credential_source_index(
        conn: &mut Connection,
    ) -> Result<(), SessionRepositoryError> {
        // Startup owns one full deterministic rebuild. BEGIN IMMEDIATE is both
        // the SQLite writer fence and the crash boundary: a decode failure or
        // process exit restores the preceding complete index, and the
        // constructor never returns a repository with partial dependencies.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        tx.execute(
            "DELETE FROM managed_session_credential_source_reference",
            [],
        )
        .map_err(storage)?;
        let rows = {
            let mut statement = tx
                .prepare(
                    "SELECT session_id, aggregate_json, revision FROM managed_session \
                     ORDER BY session_id",
                )
                .map_err(storage)?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        EncodedSessionRow {
                            aggregate_json: row.get(1)?,
                            revision: row.get(2)?,
                        },
                    ))
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?
        };
        for (stored_session_id, row) in rows {
            let session = decode(row).map_err(corrupt)?;
            if session.session_id != stored_session_id {
                return Err(corrupt(
                    "managed Session aggregate id does not match its index",
                ));
            }
            Self::sync_credential_source_index(&tx, &session)?;
        }
        tx.commit().map_err(storage)
    }

    fn is_tombstoned(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
    ) -> Result<bool, SessionRepositoryError> {
        tx.query_row(
            "SELECT 1 FROM managed_session_tombstone WHERE session_id = ?1",
            params![session_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(storage)
    }

    fn session_identity(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
    ) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
        let mut statement = tx
            .prepare(
                "SELECT identity_kind, scope_id, revision, aggregate_json FROM (
                    SELECT 0 AS identity_kind, scope_id, revision, aggregate_json
                    FROM managed_session WHERE session_id = ?1
                    UNION ALL
                    SELECT 1 AS identity_kind, scope_id, deleted_revision AS revision,
                           NULL AS aggregate_json
                    FROM managed_session_tombstone WHERE session_id = ?1
                 ) ORDER BY identity_kind",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(RawSessionIdentity {
                    kind: row.get(0)?,
                    owner_scope: row.get(1)?,
                    revision: row.get(2)?,
                    aggregate_json: row.get(3)?,
                })
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        transactional_session_identity(rows)
    }

    fn create_replay(
        tx: &rusqlite::Transaction<'_>,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
        missing_receipt: MissingCreateReceipt,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        let mut statement = tx
            .prepare(
                "WITH receipt AS (
                    SELECT payload_hash, committed_revision
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
                        identity_kind, scope_id, revision, aggregate_json
                 FROM snapshot ORDER BY identity_kind",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![session_id, idempotency.key], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    RawSessionIdentity {
                        kind: row.get(2)?,
                        owner_scope: row.get(3)?,
                        revision: row.get(4)?,
                        aggregate_json: row.get(5)?,
                    },
                ))
            })
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)?;
        let receipt = rows
            .first()
            .ok_or_else(|| corrupt("Session create replay snapshot is empty"))?;
        let receipt = match (&receipt.0, receipt.1) {
            (None, None) => None,
            (Some(payload_hash), Some(committed_revision)) => Some(SessionIdempotencyReceipt {
                payload_hash: payload_hash.clone(),
                committed_revision: SessionRevision(
                    u64::try_from(committed_revision)
                        .map_err(|_| corrupt("negative committed Session revision"))?,
                ),
            }),
            _ => return Err(corrupt("incomplete Session create receipt")),
        };
        let identity = transactional_session_identity(
            rows.into_iter()
                .map(|(_, _, identity)| identity)
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
impl ManagedSessionRepository for SqliteManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        if owner_scope.trim().is_empty()
            || session.session_id.trim().is_empty()
            || idempotency.key.trim().is_empty()
            || idempotency.payload_hash.trim().is_empty()
            || session.revision != SessionRevision(0)
            || lifecycle_facts
                .iter()
                .any(|fact| fact.object_id != session.session_id)
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create command".into(),
            ));
        }
        let owner_scope = owner_scope.to_string();
        self.with_connection(move |conn| {
            // Acquire the SQLite writer reservation before reading the expected
            // revision. A deferred read-then-write transaction can otherwise fail
            // its upgrade immediately when another aggregate writes concurrently.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            if let Some(replayed) = Self::create_replay(
                &tx,
                &owner_scope,
                &session.session_id,
                &idempotency,
                MissingCreateReceipt::AllowInsertFence,
            )? {
                return Ok(SessionCreateResult::Replayed(replayed));
            }
            let new_revision = SESSION_CREATE_REVISION;
            session.revision = new_revision;
            let inserted = tx
                .execute(
                    "INSERT INTO managed_session \
                        (session_id, scope_id, revision, aggregate_json) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT (session_id) DO NOTHING",
                    params![
                        session.session_id,
                        owner_scope,
                        db_revision(new_revision)?,
                        aggregate_str(&session)?,
                    ],
                )
                .map_err(storage)?;
            if inserted != 1 {
                if let Some(replayed) = Self::create_replay(
                    &tx,
                    &owner_scope,
                    &session.session_id,
                    &idempotency,
                    MissingCreateReceipt::RejectOccupied,
                )? {
                    return Ok(SessionCreateResult::Replayed(replayed));
                }
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::AlreadyExists,
                ));
            }
            // A concurrent delete can win after the first tombstone read. Recheck
            // in this same write transaction before any index, receipt, or outbox
            // row makes a deleted identity live again.
            if Self::is_tombstoned(&tx, &session.session_id)? {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned,
                ));
            }
            Self::sync_session_indexes(&tx, &session)?;
            tx.execute(
                "INSERT INTO managed_session_idempotency
                    (session_id, idempotency_key, payload_hash, committed_revision)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    session.session_id,
                    idempotency.key,
                    idempotency.payload_hash,
                    db_revision(new_revision)?,
                ],
            )
            .map_err(storage)?;
            for fact in lifecycle_facts {
                tx.execute(
                    "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                    params![fact.id, lifecycle_str(&fact)],
                )
                .map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
            Ok(SessionCreateResult::Applied(session))
        })
        .await
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        if owner_scope.trim().is_empty()
            || session_id.trim().is_empty()
            || idempotency.key.trim().is_empty()
            || idempotency.payload_hash.trim().is_empty()
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create replay query".into(),
            ));
        }
        let owner_scope = owner_scope.to_string();
        let session_id = session_id.to_string();
        let idempotency = idempotency.clone();
        self.with_connection(move |conn| {
            let tx = conn.transaction().map_err(storage)?;
            let replay = Self::create_replay(
                &tx,
                &owner_scope,
                &session_id,
                &idempotency,
                MissingCreateReceipt::RejectOccupied,
            )?;
            tx.commit().map_err(storage)?;
            Ok(replay)
        })
        .await
    }

    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let next = mutation
            .validate()
            .map_err(|error| SessionRepositoryError::InvalidMutation(error.to_string()))?;
        let session_id = mutation.payload.session_id().to_string();
        let owner_scope = owner_scope.to_string();
        self.with_connection(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            if let Some((stored_hash, committed_revision)) = tx
                .query_row(
                    "SELECT payload_hash, committed_revision FROM managed_session_idempotency
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                    params![session_id, mutation.idempotency.key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(storage)?
            {
                let committed_revision = SessionRevision(
                    u64::try_from(committed_revision)
                        .map_err(|_| corrupt("negative committed Session revision"))?,
                );
                return classify_mutation_replay(
                    &owner_scope,
                    &stored_hash,
                    committed_revision,
                    next,
                    &mutation.idempotency.payload_hash,
                    &Self::session_identity(&tx, &session_id)?,
                );
            }
            let current = tx
                .query_row(
                    "SELECT revision, scope_id, aggregate_json
                 FROM managed_session WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            EncodedSessionRow {
                                aggregate_json: row.get(2)?,
                                revision: row.get(0)?,
                            },
                        ))
                    },
                )
                .optional()
                .map_err(storage)?;
            let Some((current_revision, current_owner, current_row)) = current else {
                let tombstone_revision = tx
                .query_row(
                    "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
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
            if matches!(&mutation.payload, SessionMutationPayload::Delete(_)) {
                let current_session = decode(current_row).map_err(corrupt)?;
                let SessionMutationPayload::Delete(tombstone) = &mutation.payload else {
                    unreachable!("guarded above")
                };
                if !current_session.admits_tombstone(&session_id, tombstone.deleted_revision) {
                    return Err(SessionRepositoryError::InvalidMutation(
                    "Session tombstone requires hidden terminal disposition and completed cleanup"
                        .into(),
                ));
                }
            }
            match &mutation.payload {
                SessionMutationPayload::Replace(replacement) => {
                    let mut replacement = replacement.clone();
                    replacement.revision = next;
                    let affected = tx
                        .execute(
                            "UPDATE managed_session SET
                        aggregate_json = ?2, revision = ?3
                     WHERE session_id = ?1 AND scope_id = ?4 AND revision = ?5",
                            params![
                                replacement.session_id,
                                aggregate_str(&replacement)?,
                                db_revision(next)?,
                                owner_scope,
                                db_revision(current_revision)?,
                            ],
                        )
                        .map_err(storage)?;
                    if affected != 1 {
                        return Ok(SessionMutationResult::Conflict { current_revision });
                    }
                    Self::sync_session_indexes(&tx, &replacement)?;
                }
                SessionMutationPayload::Delete(tombstone) => {
                    let affected = tx
                        .execute(
                            "DELETE FROM managed_session
                         WHERE session_id = ?1 AND scope_id = ?2 AND revision = ?3",
                            params![session_id, owner_scope, db_revision(current_revision)?],
                        )
                        .map_err(storage)?;
                    if affected != 1 {
                        return Ok(SessionMutationResult::Conflict { current_revision });
                    }
                    tx.execute(
                        "INSERT INTO managed_session_tombstone
                        (session_id, scope_id, deleted_revision, deleted_at)
                     VALUES (?1, ?2, ?3, ?4)",
                        params![
                            tombstone.session_id,
                            owner_scope,
                            db_revision(tombstone.deleted_revision)?,
                            tombstone.deleted_at,
                        ],
                    )
                    .map_err(storage)?;
                }
            }
            tx.execute(
                "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES (?1, ?2, ?3, ?4)",
                params![
                    session_id,
                    mutation.idempotency.key,
                    mutation.idempotency.payload_hash,
                    db_revision(next)?,
                ],
            )
            .map_err(storage)?;
            for fact in mutation.lifecycle_facts {
                tx.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, lifecycle_str(&fact)],
            )
            .map_err(storage)?;
            }
            tx.commit().map_err(storage)?;
            Ok(SessionMutationResult::Applied { new_revision: next })
        })
        .await
    }

    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        let data = lifecycle_str(&fact);
        self.with_connection(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, data],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        self.with_connection(|conn| {
            let mut statement = conn
                .prepare("SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id")
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| {
                let encoded = row.map_err(storage)?;
                decode_lifecycle(&encoded).map_err(corrupt)
            })
            .collect()
        })
        .await
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        let fact_id = fact_id.to_string();
        self.with_connection(move |conn| {
            conn.execute(
                "DELETE FROM managed_lifecycle_outbox WHERE fact_id = ?1",
                params![fact_id],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError> {
        let session_id = session_id.to_string();
        self.with_connection(move |conn| {
            let raw = conn
                .query_row(
                    "SELECT aggregate_json, revision
                     FROM managed_session WHERE session_id = ?1",
                    params![session_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(storage)?;
            let Some(raw) = raw else {
                return Err(SessionRepositoryError::NotFound);
            };
            let (aggregate_json, revision) = raw;
            decode(EncodedSessionRow {
                aggregate_json,
                revision,
            })
            .map_err(corrupt)
        })
        .await
    }

    async fn list_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let owner_scope = owner_scope.to_string();
        self.with_connection(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT aggregate_json, revision FROM managed_session
                     WHERE scope_id = ?1 ORDER BY session_id",
                )
                .map_err(storage)?;
            statement
                .query_map(params![owner_scope], |row| {
                    Ok(EncodedSessionRow {
                        aggregate_json: row.get(0)?,
                        revision: row.get(1)?,
                    })
                })
                .map_err(storage)?
                .map(|row| {
                    row.map_err(storage)
                        .and_then(|row| decode(row).map_err(corrupt))
                })
                .collect()
        })
        .await
    }

    async fn reconcilable_sessions_page(
        &self,
        after: Option<&awaken_session_contract::SessionRecoveryCursor>,
    ) -> Result<SessionRecoveryScan, SessionRepositoryError> {
        let after = after.map(|cursor| cursor.session_id().to_string());
        self.with_connection(move |conn| {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            let page_size = usize::try_from(RECOVERY_BATCH_SIZE)
                .map_err(|error| storage(format!("invalid recovery batch size: {error}")))?;
            let mut scan = SessionRecoveryScan::default();
            {
                let mut statement = tx
                    .prepare(
                        "SELECT session.scope_id, session.session_id, session.aggregate_json, \
                                session.revision, work.observed_revision \
                         FROM managed_session_reconciliation_work work \
                         JOIN managed_session session ON session.session_id = work.session_id \
                         WHERE (?1 IS NULL OR session.session_id > ?1) \
                         ORDER BY session.session_id LIMIT ?2",
                    )
                    .map_err(storage)?;
                let rows = statement
                    .query_map(params![after, RECOVERY_BATCH_SIZE + 1], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            EncodedSessionRow {
                                aggregate_json: row.get(2)?,
                                revision: row.get(3)?,
                            },
                            row.get::<_, i64>(4)?,
                        ))
                    })
                    .map_err(storage)?
                    .collect::<Result<Vec<_>, rusqlite::Error>>()
                    .map_err(storage)?;
                let has_more = rows.len() > page_size;
                let mut rows = rows;
                rows.truncate(page_size);
                if has_more {
                    scan.next_cursor = rows.last().map(|(_, session_id, _, _)| {
                        awaken_session_contract::SessionRecoveryCursor::after_session_id(session_id)
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
                            tx.execute(
                                "DELETE FROM managed_session_quarantine WHERE session_id = ?1",
                                params![session_id],
                            )
                            .map_err(storage)?;
                            if session.needs_reconciliation() {
                                scan.sessions.push(ScopedPersistedSession {
                                    workspace_id,
                                    session,
                                });
                            }
                        }
                        Err(error) => {
                            let reason = error.to_string();
                            tx.execute(
                                "INSERT INTO managed_session_quarantine \
                                (session_id, reason, observed_revision) \
                             VALUES (?1, ?2, ?3) \
                             ON CONFLICT (session_id) DO UPDATE SET \
                                reason = excluded.reason, \
                                observed_revision = excluded.observed_revision, \
                                quarantined_at = CURRENT_TIMESTAMP",
                                params![session_id, reason, stored_revision],
                            )
                            .map_err(storage)?;
                        }
                    }
                }
                {
                    let mut quarantined = tx
                        .prepare(
                            "SELECT session_id, reason FROM managed_session_quarantine \
                         ORDER BY session_id LIMIT ?1",
                        )
                        .map_err(storage)?;
                    let rows = quarantined
                        .query_map(params![RECOVERY_BATCH_SIZE], |row| {
                            Ok(SessionRecoveryQuarantine {
                                session_id: row.get(0)?,
                                reason: row.get(1)?,
                            })
                        })
                        .map_err(storage)?;
                    scan.quarantined = rows
                        .collect::<Result<Vec<_>, rusqlite::Error>>()
                        .map_err(storage)?;
                }
            }
            tx.commit().map_err(storage)?;
            Ok(scan)
        })
        .await
    }

    async fn count_environment_phase(
        &self,
        phase: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        self.with_connection(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        EncodedSessionRow {
                            aggregate_json: row.get(1)?,
                            revision: row.get(2)?,
                        },
                    ))
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            count_environment_phase(rows, phase)
        })
        .await
    }

    async fn sessions_referencing_vault(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let workspace_id = workspace_id.to_string();
        let vault_id = vault_id.to_string();
        self.with_connection(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT session.aggregate_json, session.revision \
                     FROM managed_session_vault_reference reference \
                     JOIN managed_session session ON session.session_id = reference.session_id \
                     WHERE reference.vault_id = ?1 AND session.scope_id = ?2 \
                     ORDER BY session.session_id",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map(params![vault_id, workspace_id], |row| {
                    Ok(EncodedSessionRow {
                        aggregate_json: row.get(0)?,
                        revision: row.get(1)?,
                    })
                })
                .map_err(storage)?;
            let mut sessions = Vec::new();
            for row in rows {
                let session = decode(row.map_err(storage)?).map_err(corrupt)?;
                if !session.is_terminal() {
                    sessions.push(session);
                }
            }
            Ok(sessions)
        })
        .await
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let workspace_id = workspace_id.to_string();
        let source_id = source_id.clone();
        self.with_connection(move |conn| {
            let mut statement = conn
                .prepare(
                    "SELECT session.aggregate_json, session.revision \
                     FROM managed_session_credential_source_reference reference \
                     JOIN managed_session session ON session.session_id = reference.session_id \
                     WHERE reference.credential_source_id = ?1 AND session.scope_id = ?2 \
                     ORDER BY session.session_id",
                )
                .map_err(storage)?;
            let rows = statement
                .query_map(params![source_id.0.as_str(), workspace_id], |row| {
                    Ok(EncodedSessionRow {
                        aggregate_json: row.get(0)?,
                        revision: row.get(1)?,
                    })
                })
                .map_err(storage)?;
            let mut sessions = Vec::new();
            for row in rows {
                let session = decode(row.map_err(storage)?).map_err(corrupt)?;
                if !session.is_terminal()
                    && referenced_mcp_credential_source_ids(&session).contains(&source_id.0)
                {
                    sessions.push(session);
                }
            }
            Ok(sessions)
        })
        .await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<awaken_session_contract::SessionIdempotencyReceipt>, SessionRepositoryError>
    {
        let session_id = session_id.to_string();
        let key = key.to_string();
        self.with_connection(move |conn| {
            let row = conn
                .query_row(
                    "SELECT payload_hash, committed_revision FROM managed_session_idempotency
                     WHERE session_id = ?1 AND idempotency_key = ?2",
                    params![session_id, key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(storage)?;
            row.map(|(payload_hash, revision)| {
                u64::try_from(revision)
                    .map(
                        |revision| awaken_session_contract::SessionIdempotencyReceipt {
                            payload_hash,
                            committed_revision: SessionRevision(revision),
                        },
                    )
                    .map_err(|_| corrupt("negative Session idempotency revision"))
            })
            .transpose()
        })
        .await
    }

    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError> {
        let session_id = session_id.to_string();
        self.with_connection(move |conn| {
            conn.query_row(
                "SELECT scope_id FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?
            .ok_or(SessionRepositoryError::NotFound)
        })
        .await
    }
}
