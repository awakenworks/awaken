use super::*;

pub struct SqliteManagedSessionRepository {
    pub(crate) conn: Arc<Mutex<Connection>>,
}

impl SqliteManagedSessionRepository {
    /// Open (or create) `sessions.db` at `path` and apply the schema migrations.
    pub fn open(path: &str) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    /// An in-memory database (tests).
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, String> {
        let conn = Connection::open_in_memory().map_err(|e| e.to_string())?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, String> {
        // Embedded compositions intentionally colocate several aggregate stores
        // in one WAL database. A busy timeout is connection-local, so the
        // bootstrap connection cannot configure this repository's connection.
        conn.busy_timeout(SQLITE_WRITE_WAIT)
            .map_err(|e| e.to_string())?;
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&conn, &bundle)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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
    ) -> Result<SessionRevision, SessionRepositoryError> {
        if session.session_id.trim().is_empty()
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
        let mut conn = self.conn.lock().map_err(storage)?;
        // Acquire the SQLite writer reservation before reading the expected
        // revision. A deferred read-then-write transaction can otherwise fail
        // its upgrade immediately when another aggregate writes concurrently.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if let Some((stored_hash, committed_revision)) = tx
            .query_row(
                "SELECT payload_hash, committed_revision FROM managed_session_idempotency
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![session.session_id, idempotency.key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(storage)?
        {
            if stored_hash != idempotency.payload_hash {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::IdempotencyMismatch,
                ));
            }
            return u64::try_from(committed_revision)
                .map(SessionRevision)
                .map_err(|_| corrupt("negative committed Session revision"));
        }
        let tombstoned = tx
            .query_row(
                "SELECT 1 FROM managed_session_tombstone WHERE session_id = ?1",
                params![session.session_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(storage)?
            .is_some();
        if tombstoned {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        let new_revision = SessionRevision(1);
        session.revision = new_revision;
        let inserted = tx
            .execute(
                r#"INSERT OR IGNORE INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json,
                 scope_id, status, archived_at, effective_inputs_json, environment_binding,
                 runtime_json, revision, aggregate_json)
             VALUES (?1, '', '', NULL, '{}', '', '[]', ?2, 'aggregate', NULL,
                     '{"inputs":[]}', NULL,
                     '{"mcp_servers":[],"runtime":null,"deny_egress":false,"sandbox":null}',
                     ?3, ?4)"#,
                params![
                    session.session_id,
                    owner_scope,
                    db_revision(new_revision)?,
                    aggregate_str(&session)?,
                ],
            )
            .map_err(storage)?;
        if inserted != 1 {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::AlreadyExists,
            ));
        }
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
        Ok(new_revision)
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
        let mut conn = self.conn.lock().map_err(storage)?;
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
            if stored_hash != mutation.idempotency.payload_hash {
                return Ok(SessionMutationResult::IdempotencyMismatch);
            }
            let committed_revision = SessionRevision(
                u64::try_from(committed_revision)
                    .map_err(|_| corrupt("negative committed Session revision"))?,
            );
            return Ok(SessionMutationResult::Replayed {
                new_revision: committed_revision,
            });
        }
        let current = tx
            .query_row(
                "SELECT revision, scope_id FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(storage)?;
        let Some((current_revision, current_owner)) = current else {
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
    }

    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        let data = lifecycle_str(&fact);
        self.conn
            .lock()
            .map_err(storage)?
            .execute(
                "INSERT OR IGNORE INTO managed_lifecycle_outbox (fact_id, data) VALUES (?1, ?2)",
                params![fact.id, data],
            )
            .map_err(storage)?;
        Ok(())
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
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
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        self.conn
            .lock()
            .map_err(storage)?
            .execute(
                "DELETE FROM managed_lifecycle_outbox WHERE fact_id = ?1",
                params![fact_id],
            )
            .map_err(storage)?;
        Ok(())
    }

    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let raw = conn
            .query_row(
                "SELECT aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision
                 FROM managed_session WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                    ))
                },
            )
            .optional()
            .map_err(storage)?;
        let Some(raw) = raw else {
            return Err(SessionRepositoryError::NotFound);
        };
        let (
            aggregate_json,
            agent_id,
            model,
            title,
            metadata_json,
            environment_id,
            status,
            archived_at,
            effective_inputs_json,
            environment_binding,
            runtime_json,
            revision,
        ) = raw;
        decode(EncodedSessionRow {
            aggregate_json,
            session_id: session_id.to_string(),
            agent_id,
            model,
            title,
            metadata_json,
            environment_id,
            effective_inputs_json,
            environment_binding,
            runtime_json,
            status,
            archived_at,
            revision,
        })
        .map_err(corrupt)
    }

    async fn reconcilable_sessions(
        &self,
    ) -> Result<Vec<ScopedPersistedSession>, SessionRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        (|| -> Result<Vec<_>, SessionRepositoryError> {
            let mut statement = conn.prepare(
                "SELECT scope_id, session_id, aggregate_json, agent_id, model, title, metadata_json, environment_id, status, archived_at, effective_inputs_json, environment_binding, runtime_json, revision
                 FROM managed_session ORDER BY session_id",
            ).map_err(storage)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        EncodedSessionRow {
                            session_id: row.get(1)?,
                            aggregate_json: row.get(2)?,
                            agent_id: row.get(3)?,
                            model: row.get(4)?,
                            title: row.get(5)?,
                            metadata_json: row.get(6)?,
                            environment_id: row.get(7)?,
                            status: row.get(8)?,
                            archived_at: row.get(9)?,
                            effective_inputs_json: row.get(10)?,
                            environment_binding: row.get(11)?,
                            runtime_json: row.get(12)?,
                            revision: row.get(13)?,
                        },
                    ))
                })
                .map_err(storage)?;
            rows.map(|row| {
                let (workspace_id, row) = row.map_err(storage)?;
                Ok(ScopedPersistedSession {
                    workspace_id,
                    session: decode(row).map_err(corrupt)?,
                })
            })
            .collect::<Result<Vec<_>, SessionRepositoryError>>()
        })()
        .map(|records| {
            records
                .into_iter()
                .filter(|record| record.session.needs_reconciliation())
                .collect()
        })
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<awaken_session_contract::SessionIdempotencyReceipt>, SessionRepositoryError>
    {
        let conn = self.conn.lock().map_err(storage)?;
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
    }

    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        conn.query_row(
            "SELECT scope_id FROM managed_session WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage)?
        .ok_or(SessionRepositoryError::NotFound)
    }
}
