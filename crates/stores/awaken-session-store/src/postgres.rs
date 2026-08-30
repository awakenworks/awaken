use super::*;
use sqlx::Connection;

const IDLE_CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

fn pool_options() -> sqlx::postgres::PgPoolOptions {
    sqlx::postgres::PgPoolOptions::new()
        .idle_timeout(IDLE_CONNECTION_TIMEOUT)
        // SQLx's built-in ping is intentionally replaced because it has no
        // independent deadline. A Kubernetes Service cannot retarget an
        // established TCP socket after primary promotion.
        .test_before_acquire(false)
        .before_acquire(|connection, _metadata| {
            Box::pin(async move {
                match tokio::time::timeout(HEALTH_CHECK_TIMEOUT, connection.ping()).await {
                    Ok(result) => result.map(|()| true),
                    Err(_) => Err(sqlx::Error::PoolTimedOut),
                }
            })
        })
}

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
        let pool = pool_options()
            .connect(url)
            .await
            .map_err(|e| e.to_string())?;
        Self::with_pool(pool).await
    }

    /// Build from an existing pool: apply the session migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, String> {
        let receipts = Self::migration_receipts(&pool).await?;
        let selected = selected_session_schema(&receipts).map_err(|error| error.to_string())?;
        debug_assert!(selected.pre_convergence.is_none() || selected.stream.is_legacy());
        awaken_scoped_migration::plan(
            &selected.complete,
            &receipts,
            awaken_scoped_migration::Dialect::Postgres,
        )
        .map_err(|error| error.to_string())?;
        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|error| error.to_string())?;
        if let Some(pre_convergence) = &selected.pre_convergence {
            runner
                .run_bundle(pre_convergence)
                .await
                .map_err(|error| error.to_string())?;
            Self::normalize_session_aggregates(&pool)
                .await
                .map_err(|error| error.to_string())?;
        }
        runner
            .run_bundle(&selected.complete)
            .await
            .map_err(|error| error.to_string())?;
        runner
            .run_bundle(&converged)
            .await
            .map_err(|error| error.to_string())?;
        Self::normalize_session_aggregates(&pool)
            .await
            .map_err(|error| error.to_string())?;
        Self::rebuild_session_indexes(&pool)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        })
    }

    /// Connect to a schema migrated by an operational command without DDL or
    /// data repair. Server startup verifies the canonical roots and every
    /// root-derived Session index; only the operational migration opener may
    /// normalize or rebuild them.
    pub async fn connect_existing(url: &str) -> Result<Self, String> {
        let pool = pool_options()
            .connect(url)
            .await
            .map_err(|e| e.to_string())?;
        let receipts = Self::migration_receipts(&pool).await?;
        let selected = selected_session_schema(&receipts).map_err(|error| error.to_string())?;
        debug_assert!(selected.pre_convergence.is_none() || selected.stream.is_legacy());
        let converged = converged_session_bundle().map_err(|error| error.to_string())?;
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .map_err(|error| error.to_string())?;
        runner
            .verify_bundle(&selected.complete)
            .await
            .map_err(|error| error.to_string())?;
        runner
            .verify_bundle(&converged)
            .await
            .map_err(|error| error.to_string())?;
        Self::verify_session_state(&pool)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            pool,
            handle: tokio::runtime::Handle::current(),
        })
    }

    async fn migration_receipts(pool: &PgPool) -> Result<BTreeMap<i64, String>, String> {
        let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(format!("{NS}_schema_migrations"))
            .fetch_one(pool)
            .await
            .map_err(|error| error.to_string())?;
        if ledger.is_none() {
            return Ok(BTreeMap::new());
        }
        sqlx::query_as::<_, (i64, String)>(&format!(
            "SELECT version,checksum FROM {NS}_schema_migrations WHERE bundle_id=$1 ORDER BY version"
        ))
        .bind(BUNDLE_ID)
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().collect())
        .map_err(|error| error.to_string())
    }

    async fn normalize_session_aggregates(pool: &PgPool) -> Result<(), SessionRepositoryError> {
        let mut tx = pool.begin().await.map_err(storage)?;
        sqlx::query("LOCK TABLE managed_session IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let rows = sqlx::query(
            "SELECT session_id,aggregate_json,revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        for row in rows {
            let stored_session_id: String = row.try_get("session_id").map_err(storage)?;
            let aggregate_json: Option<String> = row.try_get("aggregate_json").map_err(storage)?;
            if let Some(canonical) = normalize_published_row(
                &stored_session_id,
                aggregate_json,
                row.try_get("revision").map_err(storage)?,
            )
            .map_err(corrupt)?
            {
                sqlx::query("UPDATE managed_session SET aggregate_json=$2 WHERE session_id=$1")
                    .bind(stored_session_id)
                    .bind(canonical)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
            }
        }
        tx.commit().await.map_err(storage)
    }

    async fn sync_session_indexes(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_session_vault_reference WHERE session_id = $1")
            .bind(&session.session_id)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        for vault_id in referenced_vault_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_vault_reference (session_id, vault_id) \
                 VALUES ($1, $2)",
            )
            .bind(&session.session_id)
            .bind(vault_id)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        }
        Self::sync_credential_source_index(tx, session).await?;
        if session.needs_reconciliation() {
            sqlx::query(
                "INSERT INTO managed_session_reconciliation_work \
                    (session_id, observed_revision) VALUES ($1, $2) \
                 ON CONFLICT (session_id) DO UPDATE SET \
                    observed_revision = excluded.observed_revision",
            )
            .bind(&session.session_id)
            .bind(db_revision(session.revision)?)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        } else {
            sqlx::query("DELETE FROM managed_session_reconciliation_work WHERE session_id = $1")
                .bind(&session.session_id)
                .execute(&mut **tx)
                .await
                .map_err(storage)?;
        }
        Ok(())
    }

    async fn sync_credential_source_index(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query(
            "DELETE FROM managed_session_credential_source_reference WHERE session_id = $1",
        )
        .bind(&session.session_id)
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
        for source_id in referenced_mcp_credential_source_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_credential_source_reference \
                    (session_id, credential_source_id) VALUES ($1, $2)",
            )
            .bind(&session.session_id)
            .bind(source_id)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        }
        Ok(())
    }

    async fn rebuild_session_indexes(pool: &PgPool) -> Result<(), SessionRepositoryError> {
        let mut tx = pool.begin().await.map_err(storage)?;
        // Session-root writers take ROW EXCLUSIVE on managed_session before
        // synchronizing its indexes. This stronger lock serializes startup
        // rebuilds with those transactions and with other new-process rebuilds,
        // so the scan and replacement are one deterministic snapshot.
        sqlx::query("LOCK TABLE managed_session IN SHARE ROW EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        sqlx::query(
            "LOCK TABLE managed_session_reconciliation_work, \
                        managed_session_vault_reference, \
                        managed_session_credential_source_reference IN EXCLUSIVE MODE",
        )
        .execute(&mut *tx)
        .await
        .map_err(storage)?;
        sqlx::query("DELETE FROM managed_session_reconciliation_work")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        sqlx::query("DELETE FROM managed_session_vault_reference")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        sqlx::query("DELETE FROM managed_session_credential_source_reference")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let rows = sqlx::query(
            "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        for row in rows {
            let stored_session_id: String = row.try_get("session_id").map_err(storage)?;
            let session = decode(EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)?;
            if session.session_id != stored_session_id {
                return Err(corrupt(
                    "managed Session aggregate id does not match its index",
                ));
            }
            Self::sync_session_indexes(&mut tx, &session).await?;
        }
        tx.commit().await.map_err(storage)
    }

    async fn verify_session_state(pool: &PgPool) -> Result<(), SessionRepositoryError> {
        let mut tx = pool.begin().await.map_err(storage)?;
        // This is both the consistent-snapshot boundary and an executable guard
        // against accidentally reintroducing startup repair into Server mode.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;

        let rows = sqlx::query(
            "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        let mut session_ids = BTreeSet::new();
        let mut expected_reconciliation = BTreeSet::new();
        let mut expected_vaults = BTreeSet::new();
        let mut expected_credential_sources = BTreeSet::new();
        for row in rows {
            let stored_session_id: String = row.try_get("session_id").map_err(storage)?;
            if !session_ids.insert(stored_session_id.clone()) {
                return Err(corrupt("duplicate managed Session root id"));
            }
            let aggregate_json: Option<String> = row.try_get("aggregate_json").map_err(storage)?;
            let aggregate_json = aggregate_json.ok_or_else(|| {
                corrupt("published managed Session row has no canonical aggregate")
            })?;
            let revision: i64 = row.try_get("revision").map_err(storage)?;
            let session = decode(EncodedSessionRow {
                aggregate_json: aggregate_json.clone(),
                revision,
            })
            .map_err(corrupt)?;
            if session.session_id != stored_session_id {
                return Err(corrupt(
                    "managed Session aggregate id does not match its index",
                ));
            }
            if aggregate_str(&session)? != aggregate_json {
                return Err(corrupt(
                    "managed Session aggregate requires operational migration normalization",
                ));
            }
            if session.needs_reconciliation() {
                expected_reconciliation.insert((stored_session_id.clone(), revision));
            }
            expected_vaults.extend(
                referenced_vault_ids(&session)
                    .into_iter()
                    .map(|vault_id| (stored_session_id.clone(), vault_id)),
            );
            expected_credential_sources.extend(
                referenced_mcp_credential_source_ids(&session)
                    .into_iter()
                    .map(|source_id| (stored_session_id.clone(), source_id)),
            );
        }

        let actual_reconciliation = sqlx::query_as::<_, (String, i64)>(
            "SELECT session_id, observed_revision FROM managed_session_reconciliation_work \
             ORDER BY session_id, observed_revision",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        if actual_reconciliation != expected_reconciliation.into_iter().collect::<Vec<_>>() {
            return Err(corrupt(
                "managed Session reconciliation index requires operational migration rebuild",
            ));
        }

        let actual_vaults = sqlx::query_as::<_, (String, String)>(
            "SELECT session_id, vault_id FROM managed_session_vault_reference \
             ORDER BY session_id, vault_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        if actual_vaults != expected_vaults.into_iter().collect::<Vec<_>>() {
            return Err(corrupt(
                "managed Session vault index requires operational migration rebuild",
            ));
        }

        let actual_credential_sources = sqlx::query_as::<_, (String, String)>(
            "SELECT session_id, credential_source_id \
             FROM managed_session_credential_source_reference \
             ORDER BY session_id, credential_source_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        if actual_credential_sources != expected_credential_sources.into_iter().collect::<Vec<_>>()
        {
            return Err(corrupt(
                "managed Session credential-source index requires operational migration rebuild",
            ));
        }

        tx.commit().await.map_err(storage)
    }

    async fn is_tombstoned(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session_id: &str,
    ) -> Result<bool, SessionRepositoryError> {
        sqlx::query("SELECT 1 FROM managed_session_tombstone WHERE session_id = $1")
            .bind(session_id)
            .fetch_optional(&mut **tx)
            .await
            .map(|row| row.is_some())
            .map_err(storage)
    }

    async fn session_identity(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session_id: &str,
    ) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
        let rows = sqlx::query(
            "SELECT identity_kind, scope_id, revision, aggregate_json FROM (
                SELECT 0::bigint AS identity_kind, scope_id, revision, aggregate_json
                FROM managed_session WHERE session_id = $1
                UNION ALL
                SELECT 1::bigint AS identity_kind, scope_id,
                       deleted_revision AS revision, NULL::text AS aggregate_json
                FROM managed_session_tombstone WHERE session_id = $1
             ) AS identity ORDER BY identity_kind",
        )
        .bind(session_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(storage)?;
        transactional_session_identity(
            rows.into_iter()
                .map(|row| {
                    Ok(RawSessionIdentity {
                        kind: row.try_get("identity_kind").map_err(storage)?,
                        owner_scope: row.try_get("scope_id").map_err(storage)?,
                        revision: row.try_get("revision").map_err(storage)?,
                        aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                    })
                })
                .collect::<Result<Vec<_>, SessionRepositoryError>>()?,
        )
    }

    async fn create_replay(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
        missing_receipt: MissingCreateReceipt,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        let rows = sqlx::query(
            "WITH receipt AS (
                SELECT payload_hash, committed_revision
                FROM managed_session_idempotency
                WHERE session_id = $1 AND idempotency_key = $2
             ), identity AS (
                SELECT 0::bigint AS identity_kind, scope_id, revision, aggregate_json
                FROM managed_session WHERE session_id = $1
                UNION ALL
                SELECT 1::bigint AS identity_kind, scope_id,
                       deleted_revision AS revision, NULL::text AS aggregate_json
                FROM managed_session_tombstone WHERE session_id = $1
             ), snapshot AS (
                SELECT identity_kind, scope_id, revision, aggregate_json FROM identity
                UNION ALL
                SELECT -1::bigint, NULL::text, NULL::bigint, NULL::text
                WHERE NOT EXISTS (SELECT 1 FROM identity)
             )
             SELECT (SELECT payload_hash FROM receipt) AS receipt_hash,
                    (SELECT committed_revision FROM receipt) AS receipt_revision,
                    identity_kind, scope_id, revision, aggregate_json
             FROM snapshot ORDER BY identity_kind",
        )
        .bind(session_id)
        .bind(&idempotency.key)
        .fetch_all(&mut **tx)
        .await
        .map_err(storage)?;
        let first = rows
            .first()
            .ok_or_else(|| corrupt("Session create replay snapshot is empty"))?;
        let receipt_hash = first
            .try_get::<Option<String>, _>("receipt_hash")
            .map_err(storage)?;
        let receipt_revision = first
            .try_get::<Option<i64>, _>("receipt_revision")
            .map_err(storage)?;
        let receipt = match (receipt_hash, receipt_revision) {
            (None, None) => None,
            (Some(payload_hash), Some(committed_revision)) => Some(SessionIdempotencyReceipt {
                payload_hash,
                committed_revision: SessionRevision(
                    u64::try_from(committed_revision)
                        .map_err(|_| corrupt("negative committed Session revision"))?,
                ),
            }),
            _ => return Err(corrupt("incomplete Session create receipt")),
        };
        let identity = transactional_session_identity(
            rows.into_iter()
                .map(|row| {
                    Ok(RawSessionIdentity {
                        kind: row.try_get("identity_kind").map_err(storage)?,
                        owner_scope: row.try_get("scope_id").map_err(storage)?,
                        revision: row.try_get("revision").map_err(storage)?,
                        aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                    })
                })
                .filter_map(
                    |row: Result<RawSessionIdentity, SessionRepositoryError>| match row {
                        Ok(identity) if identity.kind < 0 => None,
                        other => Some(other),
                    },
                )
                .collect::<Result<Vec<_>, SessionRepositoryError>>()?,
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
impl ManagedSessionRepository for PostgresManagedSessionRepository {
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        if let Some(replayed) = Self::create_replay(
            &mut tx,
            owner_scope,
            &session.session_id,
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
            r#"INSERT INTO managed_session
                (session_id, scope_id, revision, aggregate_json)
             VALUES ($1, $2, $3, $4)
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
            if let Some(replayed) = Self::create_replay(
                &mut tx,
                owner_scope,
                &session.session_id,
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
        // PostgreSQL's ON CONFLICT wait can outlive a concurrent delete. A
        // second tombstone fence in the same transaction prevents committing a
        // resurrected live row beside the winner's tombstone.
        if Self::is_tombstoned(&mut tx, &session.session_id).await? {
            return Err(SessionRepositoryError::Conflict(
                SessionRepositoryConflict::Tombstoned,
            ));
        }
        Self::sync_session_indexes(&mut tx, &session).await?;
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
        Ok(SessionCreateResult::Applied(session))
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
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let replay = Self::create_replay(
            &mut tx,
            owner_scope,
            session_id,
            idempotency,
            MissingCreateReceipt::RejectOccupied,
        )
        .await?;
        tx.commit().await.map_err(storage)?;
        Ok(replay)
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
            let committed_revision: i64 = row.get("committed_revision");
            let committed_revision = SessionRevision(
                u64::try_from(committed_revision)
                    .map_err(|_| corrupt("negative committed Session revision"))?,
            );
            return classify_mutation_replay(
                owner_scope,
                &stored_hash,
                committed_revision,
                next,
                &mutation.idempotency.payload_hash,
                &Self::session_identity(&mut tx, &session_id).await?,
            );
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
                Self::sync_session_indexes(&mut tx, &replacement).await?;
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

    async fn list_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        sqlx::query(
            "SELECT aggregate_json, revision FROM managed_session \
             WHERE scope_id = $1 ORDER BY session_id",
        )
        .bind(owner_scope)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            decode(EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)
        })
        .collect()
    }

    async fn reconcilable_sessions_page(
        &self,
        after: Option<&awaken_session_contract::SessionRecoveryCursor>,
    ) -> Result<SessionRecoveryScan, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let page_size = usize::try_from(RECOVERY_BATCH_SIZE)
            .map_err(|error| storage(format!("invalid recovery batch size: {error}")))?;
        let mut scan = SessionRecoveryScan::default();
        let mut rows = sqlx::query(
            "SELECT session.scope_id, session.session_id, session.aggregate_json, \
                    session.revision, work.observed_revision \
             FROM managed_session_reconciliation_work work \
             JOIN managed_session session ON session.session_id = work.session_id \
             WHERE ($1::text IS NULL OR session.session_id > $1) \
             ORDER BY session.session_id LIMIT $2",
        )
        .bind(after.map(awaken_session_contract::SessionRecoveryCursor::session_id))
        .bind(RECOVERY_BATCH_SIZE + 1)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?;
        let has_more = rows.len() > page_size;
        rows.truncate(page_size);
        if has_more {
            let session_id = rows
                .last()
                .ok_or_else(|| storage("recovery lookahead produced an empty page"))?
                .try_get::<String, _>("session_id")
                .map_err(storage)?;
            scan.next_cursor =
                Some(awaken_session_contract::SessionRecoveryCursor::after_session_id(session_id));
        }
        for row in rows {
            let session_id: String = row.try_get("session_id").map_err(storage)?;
            let encoded = EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            };
            let observed_revision: i64 = row.try_get("observed_revision").map_err(storage)?;
            let stored_revision: i64 = row.try_get("revision").map_err(storage)?;
            if observed_revision != stored_revision {
                return Err(corrupt(format!(
                    "Session reconciliation revision drift for {session_id}"
                )));
            }
            match decode(encoded) {
                Ok(session) => {
                    sqlx::query("DELETE FROM managed_session_quarantine WHERE session_id = $1")
                        .bind(&session_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage)?;
                    if session.needs_reconciliation() {
                        scan.sessions.push(ScopedPersistedSession {
                            workspace_id: row.try_get("scope_id").map_err(storage)?,
                            session,
                        });
                    }
                }
                Err(error) => {
                    let reason = error.to_string();
                    sqlx::query(
                        "INSERT INTO managed_session_quarantine \
                            (session_id, reason, observed_revision) \
                         VALUES ($1, $2, $3) \
                         ON CONFLICT (session_id) DO UPDATE SET \
                            reason = excluded.reason, \
                            observed_revision = excluded.observed_revision, \
                            quarantined_at = CURRENT_TIMESTAMP",
                    )
                    .bind(&session_id)
                    .bind(&reason)
                    .bind(stored_revision)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                }
            }
        }
        for row in sqlx::query(
            "SELECT session_id, reason FROM managed_session_quarantine \
             ORDER BY session_id LIMIT $1",
        )
        .bind(RECOVERY_BATCH_SIZE)
        .fetch_all(&mut *tx)
        .await
        .map_err(storage)?
        {
            scan.quarantined.push(SessionRecoveryQuarantine {
                session_id: row.try_get("session_id").map_err(storage)?,
                reason: row.try_get("reason").map_err(storage)?,
            });
        }
        tx.commit().await.map_err(storage)?;
        Ok(scan)
    }

    async fn count_environment_phase(
        &self,
        phase: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        let rows = sqlx::query(
            "SELECT session_id, aggregate_json, revision FROM managed_session ORDER BY session_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("session_id").map_err(storage)?,
                EncodedSessionRow {
                    aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                    revision: row.try_get("revision").map_err(storage)?,
                },
            ))
        })
        .collect::<Result<Vec<_>, SessionRepositoryError>>()?;
        count_environment_phase(rows, phase)
    }

    async fn sessions_referencing_vault(
        &self,
        workspace_id: &str,
        vault_id: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let rows = sqlx::query(
            "SELECT session.aggregate_json, session.revision \
             FROM managed_session_vault_reference reference \
             JOIN managed_session session ON session.session_id = reference.session_id \
             WHERE reference.vault_id = $1 AND session.scope_id = $2 \
             ORDER BY session.session_id",
        )
        .bind(vault_id)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut sessions = Vec::new();
        for row in rows {
            let session = decode(EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)?;
            if !session.is_terminal() {
                sessions.push(session);
            }
        }
        Ok(sessions)
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        let rows = sqlx::query(
            "SELECT session.aggregate_json, session.revision \
             FROM managed_session_credential_source_reference reference \
             JOIN managed_session session ON session.session_id = reference.session_id \
             WHERE reference.credential_source_id = $1 AND session.scope_id = $2 \
             ORDER BY session.session_id",
        )
        .bind(&source_id.0)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut sessions = Vec::new();
        for row in rows {
            let session = decode(EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)?;
            if !session.is_terminal()
                && referenced_mcp_credential_source_ids(&session).contains(&source_id.0)
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

#[cfg(test)]
mod pool_policy_tests {
    use super::*;

    #[test]
    fn session_pool_bounds_stale_primary_connections() {
        // Cause/effect graph: C1 a fresh/healthy connection answers a bounded
        // ping -> E1 the Session repository reuses it; C2 an established socket
        // still targets a removed primary -> E2 the 500ms hook hard-discards it;
        // C3 it remains unused for 5s -> E3 the pool reaps it. Decision rules:
        // P1=C1=>E1; P2=C2=>E2; P3=C3=>E3. The distributed k3d promotion test is
        // the live P2 oracle; this structural case prevents an unbounded SQLx
        // ping from being placed in front of the hook.
        let options = pool_options();
        assert_eq!(
            options.get_idle_timeout(),
            Some(IDLE_CONNECTION_TIMEOUT),
            "P3/E3"
        );
        assert!(
            !options.get_test_before_acquire(),
            "P2/E2 only the bounded health hook may probe idle sockets"
        );
    }
}
