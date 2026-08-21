use super::*;

/// A Postgres-backed [`ManagedSessionRepository`] — the network-DB sibling over
/// the same `managed` migration scope. The Session port is async; the retained
/// synchronous Dream port uses the canonical store-runtime bridge on `handle`.
pub struct PostgresManagedSessionRepository {
    pub(crate) pool: PgPool,
    pub(crate) handle: tokio::runtime::Handle,
}

impl PostgresManagedSessionRepository {
    /// Connect and apply the session migrations under the `managed` namespace.
    pub async fn connect(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the session migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .run_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        })
    }

    /// Connect to a schema migrated by an operational command without DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = PgPool::connect(url).await.map_err(|e| e.to_string())?;
        let bundle = session_bundle().map_err(|e| e.to_string())?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|e| e.to_string())?
            .verify_bundle(&bundle)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        })
    }
}

#[async_trait]
impl ManagedSessionRepository for PostgresManagedSessionRepository {
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if let Some(row) = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(&session.session_id)
        .bind(&idempotency.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            let stored_hash: String = row.get("payload_hash");
            if stored_hash != idempotency.payload_hash {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::IdempotencyMismatch,
                ));
            }
            let committed_revision: i64 = row.get("committed_revision");
            return u64::try_from(committed_revision)
                .map(SessionRevision)
                .map_err(|_| corrupt("negative committed Session revision"));
        }
        if sqlx::query("SELECT 1 FROM managed_session_tombstone WHERE session_id = $1")
            .bind(&session.session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?
            .is_some()
        {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        let new_revision = SessionRevision(1);
        session.revision = new_revision;
        let inserted = sqlx::query(
            r#"INSERT INTO managed_session
                (session_id, agent_id, model, title, metadata_json, environment_id, mcp_json,
                 scope_id, status, archived_at, effective_inputs_json, environment_binding,
                 runtime_json, revision, aggregate_json)
             VALUES ($1, '', '', NULL, '{}', '', '[]', $2, 'aggregate', NULL,
                     '{"inputs":[]}', NULL,
                     '{"mcp_servers":[],"runtime":null,"deny_egress":false,"sandbox":null}',
                     $3, $4)
             ON CONFLICT (session_id) DO NOTHING"#,
        )
        .bind(&session.session_id)
        .bind(owner_scope)
        .bind(db_revision(new_revision)?)
        .bind(aggregate_str(&session)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?
        .rows_affected();
        if inserted != 1 {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::AlreadyExists,
            ));
        }
        sqlx::query(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&session.session_id)
        .bind(&idempotency.key)
        .bind(&idempotency.payload_hash)
        .bind(db_revision(new_revision)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for fact in lifecycle_facts {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
                 ON CONFLICT (fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if let Some(row) = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(&session_id)
        .bind(&mutation.idempotency.key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?
        {
            let stored_hash: String = row.get("payload_hash");
            if stored_hash != mutation.idempotency.payload_hash {
                return Ok(SessionMutationResult::IdempotencyMismatch);
            }
            let committed_revision: i64 = row.get("committed_revision");
            return Ok(SessionMutationResult::Replayed {
                new_revision: SessionRevision(
                    u64::try_from(committed_revision)
                        .map_err(|_| corrupt("negative committed Session revision"))?,
                ),
            });
        }
        let current = sqlx::query(
            "SELECT revision, scope_id, aggregate_json
             FROM managed_session WHERE session_id = $1 FOR UPDATE",
        )
        .bind(&session_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(storage)?;
        let Some(current) = current else {
            let tombstone = sqlx::query(
                "SELECT deleted_revision FROM managed_session_tombstone WHERE session_id = $1",
            )
            .bind(&session_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(storage)?;
            let revision = tombstone.map_or(0, |row| row.get::<i64, _>("deleted_revision"));
            let revision = u64::try_from(revision)
                .map_err(|_| corrupt("negative deleted Session revision"))?;
            return Ok(SessionMutationResult::Conflict {
                current_revision: SessionRevision(revision),
            });
        };
        let current_revision = SessionRevision(
            u64::try_from(current.get::<i64, _>("revision"))
                .map_err(|_| corrupt("negative managed Session revision"))?,
        );
        let current_owner: String = current.get("scope_id");
        if current_owner != owner_scope || current_revision != mutation.expected_revision {
            return Ok(SessionMutationResult::Conflict { current_revision });
        }
        if matches!(&mutation.payload, SessionMutationPayload::Delete(_)) {
            let current_session = decode(EncodedSessionRow {
                aggregate_json: current.try_get("aggregate_json").map_err(storage)?,
                revision: current.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)?;
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
                let affected = sqlx::query(
                    "UPDATE managed_session SET
                        aggregate_json = $2, revision = $3
                     WHERE session_id = $1 AND scope_id = $4 AND revision = $5",
                )
                .bind(&replacement.session_id)
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
            }
            SessionMutationPayload::Delete(tombstone) => {
                let affected = sqlx::query(
                    "DELETE FROM managed_session
                     WHERE session_id = $1 AND scope_id = $2 AND revision = $3",
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
                        (session_id, scope_id, deleted_revision, deleted_at)
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(&tombstone.session_id)
                .bind(owner_scope)
                .bind(db_revision(tombstone.deleted_revision)?)
                .bind(&tombstone.deleted_at)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            }
        }
        sqlx::query(
            "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&session_id)
        .bind(&mutation.idempotency.key)
        .bind(&mutation.idempotency.payload_hash)
        .bind(db_revision(next)?)
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        for fact in mutation.lifecycle_facts {
            sqlx::query(
                "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
                 ON CONFLICT (fact_id) DO NOTHING",
            )
            .bind(&fact.id)
            .bind(lifecycle_str(&fact))
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        }
        tx.commit().await.map_err(storage)?;
        Ok(SessionMutationResult::Applied { new_revision: next })
    }

    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, $2)
             ON CONFLICT (fact_id) DO NOTHING",
        )
        .bind(&fact.id)
        .bind(lifecycle_str(&fact))
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        let rows =
            sqlx::query("SELECT data FROM managed_lifecycle_outbox ORDER BY created_at, fact_id")
                .fetch_all(&self.pool)
                .await
                .map_err(storage)?;
        rows.into_iter()
            .map(|row| {
                row.try_get::<String, _>("data")
                    .map_err(storage)
                    .and_then(|data| decode_lifecycle(&data).map_err(corrupt))
            })
            .collect()
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_lifecycle_outbox WHERE fact_id = $1")
            .bind(fact_id)
            .execute(&self.pool)
            .await
            .map_err(storage)?;
        Ok(())
    }

    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError> {
        let row = sqlx::query(
            "SELECT aggregate_json, revision \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some(row) = row else {
            return Err(SessionRepositoryError::NotFound);
        };
        decode(EncodedSessionRow {
            aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
            revision: row.try_get("revision").map_err(storage)?,
        })
        .map_err(corrupt)
    }

    async fn reconcilable_sessions(&self) -> Result<SessionRecoveryScan, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let mut scan = SessionRecoveryScan::default();
        for row in sqlx::query(
            "SELECT session_id, reason FROM managed_session_quarantine ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        {
            scan.quarantined.push(SessionRecoveryQuarantine {
                session_id: row.try_get("session_id").map_err(storage)?,
                reason: row.try_get("reason").map_err(storage)?,
            });
        }
        let rows = sqlx::query(
            "SELECT scope_id, session_id, aggregate_json, revision \
             FROM managed_session session \
             WHERE NOT EXISTS (SELECT 1 FROM managed_session_quarantine quarantine \
                               WHERE quarantine.session_id = session.session_id) \
             ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        for row in rows {
            let session_id: String = row.try_get("session_id").map_err(storage)?;
            let encoded = EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            };
            match decode(encoded) {
                Ok(session) if session.needs_reconciliation() => {
                    scan.sessions.push(ScopedPersistedSession {
                        workspace_id: row.try_get("scope_id").map_err(storage)?,
                        session,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    let reason = error.to_string();
                    sqlx::query(
                        "INSERT INTO managed_session_quarantine (session_id, reason) \
                         VALUES ($1, $2) ON CONFLICT (session_id) DO NOTHING",
                    )
                    .bind(&session_id)
                    .bind(&reason)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                    scan.quarantined
                        .push(SessionRecoveryQuarantine { session_id, reason });
                }
            }
        }
        tx.commit().await.map_err(storage)?;
        Ok(scan)
    }

    async fn sessions_referencing_vault(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let rows = sqlx::query(
            "SELECT session_id FROM managed_session WHERE scope_id = $1 ORDER BY session_id",
        )
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut sessions = Vec::new();
        for row in rows {
            let session_id: String = row.try_get("session_id").map_err(storage)?;
            let session = self.get(&session_id).await?;
            if !session.is_terminal()
                && session.frozen_baseline().is_some_and(|baseline| {
                    baseline
                        .mcp_authoring
                        .ordered_vault_ids
                        .iter()
                        .any(|id| id == vault_id)
                })
            {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<awaken_session_contract::SessionIdempotencyReceipt>, SessionRepositoryError>
    {
        let row = sqlx::query(
            "SELECT payload_hash, committed_revision FROM managed_session_idempotency
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(session_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            let revision: i64 = row.try_get("committed_revision").map_err(storage)?;
            Ok(awaken_session_contract::SessionIdempotencyReceipt {
                payload_hash: row.try_get("payload_hash").map_err(storage)?,
                committed_revision: SessionRevision(
                    u64::try_from(revision)
                        .map_err(|_| corrupt("negative Session idempotency revision"))?,
                ),
            })
        })
        .transpose()
    }

    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError> {
        let row = sqlx::query("SELECT scope_id FROM managed_session WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?
            .ok_or(SessionRepositoryError::NotFound)?;
        row.try_get("scope_id").map_err(storage)
    }
}
