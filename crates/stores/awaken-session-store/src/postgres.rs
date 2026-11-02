// @formal entrypoint session.delete_tombstone_requires_hidden_terminal_verified_cleanup
// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session.owner_saved_atomically
// @formal entrypoint session.visible_row_always_owned
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
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
use awaken_sql_storage::postgres_pool_options as pool_options;

/// A Postgres-backed [`SessionRepository`] — the network-DB sibling over
/// the same `managed` migration scope.
pub struct PostgresManagedSessionRepository {
    pub(crate) pool: PgPool,
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
            Self::normalize_session_aggregates(&pool, false)
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
        Self::normalize_session_aggregates(&pool, true)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self { pool })
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
        Self::with_existing_pool(pool).await
    }

    /// Verify the existing scoped ledger over the composition-owned pool.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, String> {
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
        Ok(Self { pool })
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

    async fn normalize_session_aggregates(
        pool: &PgPool,
        sync_indexes: bool,
    ) -> Result<(), SessionRepositoryError> {
        // Root writers use ROW EXCLUSIVE before updating their projections.
        // This lock serializes the one normalization/index-repair transaction
        // with writers and other migrators. Pre-convergence lacks final indexes;
        // only the final pass repairs them. Server connect_existing stays read-only.
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
            let (session, canonical) = normalize_published_row(
                &stored_session_id,
                aggregate_json,
                row.try_get("revision").map_err(storage)?,
            )
            .map_err(corrupt)?;
            if let Some(canonical) = canonical {
                sqlx::query(
                    "UPDATE managed_session SET aggregate_json=$2, revision=$3 WHERE session_id=$1",
                )
                .bind(stored_session_id)
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
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        session: &PersistedSession,
    ) -> Result<(), SessionRepositoryError> {
        sqlx::query("DELETE FROM managed_session_vault_reference WHERE session_id = $1")
            .bind(session.session_id.as_str())
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        for vault_id in referenced_vault_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_vault_reference (session_id, vault_id) \
                 VALUES ($1, $2)",
            )
            .bind(session.session_id.as_str())
            .bind(vault_id)
            .execute(&mut **tx)
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
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
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
            .execute(&mut **tx)
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
            sqlx::query("DELETE FROM managed_session_reconciliation_work WHERE session_id = $1")
                .bind(session.session_id.as_str())
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
        .bind(session.session_id.as_str())
        .execute(&mut **tx)
        .await
        .map_err(storage)?;
        for source_id in referenced_mcp_credential_source_ids(session) {
            sqlx::query(
                "INSERT INTO managed_session_credential_source_reference \
                    (session_id, credential_source_id) VALUES ($1, $2)",
            )
            .bind(session.session_id.as_str())
            .bind(source_id)
            .execute(&mut **tx)
            .await
            .map_err(storage)?;
        }
        Ok(())
    }

    async fn verify_session_state(pool: &PgPool) -> Result<(), SessionRepositoryError> {
        let mut tx = pool.begin().await.map_err(storage)?;
        // This is both the consistent-snapshot boundary and an executable guard
        // against accidentally reintroducing startup repair into Server mode.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        // A complete audit remains required, but each read is one bounded
        // keyset page. The former fetch_all plus four global BTreeSets made
        // Coordinator startup memory proportional to the entire Session DB.
        let mut after: Option<String> = None;
        loop {
            let cursor = keyset_predicate("root.session_id", "$1::text", after.is_some());
            let limit = RECOVERY_BATCH_SIZE;
            let sql = format!(
                "SELECT root.session_id, root.aggregate_json, root.revision, \
                        work.observed_revision, work.worker_owner, \
                        work.preparation_candidate::bigint AS preparation_candidate, \
                        work.unbound_preparation::bigint AS unbound_preparation, \
                        work.unbound_terminal::bigint AS unbound_terminal, \
                        work.demand_invalid::bigint AS demand_invalid, \
                        ARRAY(SELECT ref.vault_id FROM managed_session_vault_reference ref \
                              WHERE ref.session_id = root.session_id ORDER BY ref.vault_id) AS vaults, \
                        ARRAY(SELECT ref.credential_source_id \
                              FROM managed_session_credential_source_reference ref \
                              WHERE ref.session_id = root.session_id \
                              ORDER BY ref.credential_source_id) AS credential_sources \
                 FROM managed_session root \
                 LEFT JOIN managed_session_reconciliation_work work \
                   ON work.session_id = root.session_id \
                 WHERE {cursor} \
                 ORDER BY root.session_id LIMIT {limit}",
            );
            let rows = sqlx::query(&sql)
                .bind(after.as_deref())
                .fetch_all(&mut *tx)
                .await
                .map_err(storage)?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let stored_session_id: String = row.try_get("session_id").map_err(storage)?;
                // PostgreSQL owns both keyset order and comparison. Rust's
                // bytewise string order may differ from the database collation.
                let aggregate_json: Option<String> =
                    row.try_get("aggregate_json").map_err(storage)?;
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
                let actual_revision: Option<i64> =
                    row.try_get("observed_revision").map_err(storage)?;
                let expected = session.needs_reconciliation().then(|| {
                    let obligation = worker_obligation_index(&session);
                    (
                        revision,
                        obligation.owner,
                        i64::from(obligation.preparation_candidate),
                        i64::from(obligation.unbound_preparation),
                        i64::from(obligation.unbound_terminal),
                        i64::from(obligation.invalid),
                    )
                });
                let actual = if let Some(observed_revision) = actual_revision {
                    Some((
                        observed_revision,
                        row.try_get::<Option<String>, _>("worker_owner")
                            .map_err(storage)?,
                        row.try_get::<Option<i64>, _>("preparation_candidate")
                            .map_err(storage)?
                            .unwrap_or(-1),
                        row.try_get::<Option<i64>, _>("unbound_preparation")
                            .map_err(storage)?
                            .unwrap_or(-1),
                        row.try_get::<Option<i64>, _>("unbound_terminal")
                            .map_err(storage)?
                            .unwrap_or(-1),
                        row.try_get::<Option<i64>, _>("demand_invalid")
                            .map_err(storage)?
                            .unwrap_or(-1),
                    ))
                } else {
                    None
                };
                if actual != expected {
                    return Err(corrupt(
                        "managed Session reconciliation index requires operational migration rebuild",
                    ));
                }
                let actual_vaults: Vec<String> = row.try_get("vaults").map_err(storage)?;
                if actual_vaults
                    != referenced_vault_ids(&session)
                        .into_iter()
                        .collect::<Vec<_>>()
                {
                    return Err(corrupt(
                        "managed Session vault index requires operational migration rebuild",
                    ));
                }
                let actual_sources: Vec<String> =
                    row.try_get("credential_sources").map_err(storage)?;
                if actual_sources
                    != referenced_mcp_credential_source_ids(&session)
                        .into_iter()
                        .collect::<Vec<_>>()
                {
                    return Err(corrupt(
                        "managed Session credential-source index requires operational migration rebuild",
                    ));
                }
                after = Some(stored_session_id);
            }
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
                SELECT payload_hash, committed_revision, receipt_kind
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
                    (SELECT receipt_kind FROM receipt) AS receipt_kind,
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
        let receipt_kind = first
            .try_get::<Option<String>, _>("receipt_kind")
            .map_err(storage)?;
        let receipt = match (receipt_hash, receipt_revision, receipt_kind) {
            (None, None, None) => None,
            (Some(payload_hash), Some(committed_revision), Some(kind)) => Some(
                decode_session_receipt(payload_hash, committed_revision, &kind)?,
            ),
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

    /// Root locks serialize commands whose accepted no-op does not advance the
    /// revision. The same receipt lookup runs before and after that lock.
    async fn mutation_replay(
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        owner_scope: &str,
        mutation: &SessionMutation,
        expected_committed_revision: SessionRevision,
    ) -> Result<Option<SessionMutationResult>, SessionRepositoryError> {
        let session_id = mutation.payload.session_id().as_str();
        let row = sqlx::query_as::<_, (String, i64, String)>(
            "SELECT payload_hash, committed_revision, receipt_kind \
             FROM managed_session_idempotency \
             WHERE session_id = $1 AND idempotency_key = $2",
        )
        .bind(session_id)
        .bind(&mutation.idempotency.key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(storage)?;
        let Some((hash, revision, kind)) = row else {
            return Ok(None);
        };
        let receipt = decode_session_receipt(hash, revision, &kind)?;
        let identity = Self::session_identity(tx, session_id).await?;
        classify_mutation_replay(
            owner_scope,
            &receipt,
            mutation,
            expected_committed_revision,
            &identity,
        )
        .map(Some)
    }
}

#[async_trait::async_trait]
impl SessionRepositoryReads for PostgresManagedSessionRepository {
    async fn get(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        let row = sqlx::query(
            "SELECT aggregate_json, revision \
             FROM managed_session WHERE session_id = $1",
        )
        .bind(session_id.as_str())
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

    async fn owner(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, SessionRepositoryError> {
        let row = sqlx::query("SELECT scope_id FROM managed_session WHERE session_id = $1")
            .bind(session_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?
            .ok_or(SessionRepositoryError::NotFound)?;
        row.try_get("scope_id").map_err(storage)
    }

    async fn scan(
        &self,
        query: awaken_session_contract::SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        use awaken_session_contract::SessionScanQuery;
        match query {
            SessionScanQuery::WorkerRealizationOwner { worker_id } => {
                self.scan_worker_realization_owner(worker_id).await
            }
            SessionScanQuery::Workspace { workspace_id } => Ok(Self::scoped_scan(
                workspace_id,
                self.scan_workspace(workspace_id).await?,
            )),
            SessionScanQuery::VaultReferences {
                workspace_id,
                vault_id,
            } => Ok(Self::scoped_scan(
                workspace_id,
                self.scan_vault_references(workspace_id, vault_id).await?,
            )),
            SessionScanQuery::CredentialSourceReferences {
                workspace_id,
                source_id,
            } => Ok(Self::scoped_scan(
                workspace_id,
                self.scan_credential_source_references(workspace_id, source_id)
                    .await?,
            )),
        }
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<
        Option<awaken_session_contract::DeletedSessionEventAdmissions>,
        SessionRepositoryError,
    > {
        let row = sqlx::query("SELECT deleted_revision, event_admissions_json FROM managed_session_tombstone WHERE session_id=$1 AND scope_id=$2")
            .bind(session_id.as_str()).bind(owner_scope).fetch_optional(&self.pool).await.map_err(storage)?;
        row.map(|row| {
            super::event_admissions::decode(
                row.try_get("deleted_revision").map_err(storage)?,
                row.try_get("event_admissions_json").map_err(storage)?,
            )
        })
        .transpose()
    }
}

#[async_trait]
impl SessionRepository for PostgresManagedSessionRepository {
    async fn create(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        async {
            validate_session_create_command(owner_scope, &session, &idempotency, &lifecycle_facts)?;
            let mut tx = self.pool.begin().await.map_err(storage)?;
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
                r#"INSERT INTO managed_session
                (session_id, scope_id, revision, aggregate_json)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (session_id) DO NOTHING"#,
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
            // PostgreSQL's ON CONFLICT wait can outlive a concurrent delete. A
            // second tombstone fence in the same transaction prevents committing a
            // resurrected live row beside the winner's tombstone.
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
             VALUES ($1, $2, $3, $4)",
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
            let mut tx = self.pool.begin().await.map_err(storage)?;
            if let Some(replay) =
                Self::mutation_replay(&mut tx, owner_scope, &mutation, next).await?
            {
                return Ok(replay);
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
            if let Some(replay) =
                Self::mutation_replay(&mut tx, owner_scope, &mutation, next).await?
            {
                return Ok(replay);
            }
            let mut deleted_admissions = validate_current_mutation(
                &mutation,
                EncodedSessionRow {
                    aggregate_json: current.try_get("aggregate_json").map_err(storage)?,
                    revision: current.try_get("revision").map_err(storage)?,
                },
            )?;
            if matches!(&mutation.payload, SessionMutationPayload::Delete(_)) {
                let noops = sqlx::query_as::<_, (String, String, i64)>(
                    "SELECT idempotency_key, payload_hash, committed_revision \
                     FROM managed_session_idempotency \
                     WHERE session_id = $1 AND receipt_kind = $2",
                )
                .bind(&session_id)
                .bind(receipt_kind_value(SessionCommandReceiptKind::EventNoOp))
                .fetch_all(&mut *tx)
                .await
                .map_err(storage)?;
                if !noops.is_empty() {
                    deleted_admissions = Some(event_admissions::merge_noops(
                        deleted_admissions
                            .as_deref()
                            .expect("delete encoded its root receipts"),
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
                        aggregate_json = $2, revision = $3
                     WHERE session_id = $1 AND scope_id = $4 AND revision = $5",
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
                        (session_id, scope_id, deleted_revision, deleted_at, event_admissions_json)
                     VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(tombstone.session_id.as_str())
                    .bind(owner_scope)
                    .bind(db_revision(tombstone.deleted_revision)?)
                    .bind(&tombstone.deleted_at)
                    .bind(deleted_admissions)
                    .execute(&mut *tx)
                    .await
                    .map_err(storage)?;
                }
                SessionMutationPayload::EventNoOp { .. } => {}
            }
            sqlx::query(
                "INSERT INTO managed_session_idempotency
                (session_id, idempotency_key, payload_hash, committed_revision, receipt_kind)
             VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&session_id)
            .bind(&mutation.idempotency.key)
            .bind(&mutation.idempotency.payload_hash)
            .bind(db_revision(next)?)
            .bind(receipt_kind_value(receipt_kind))
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
            Ok(if receipt_kind == SessionCommandReceiptKind::EventNoOp {
                SessionMutationResult::AcknowledgedNoOp {
                    observed_revision: next,
                }
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
             WHERE session_id = $1 AND idempotency_key = $2",
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
impl SessionReconciliation for PostgresManagedSessionRepository {
    async fn reconciliation_scan(
        &self,
        after: Option<&awaken_session_contract::SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        self.scan_reconcilable(after, false).await
    }

    async fn worker_demand(&self) -> Result<u64, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        let (owners, initial, terminal, invalid, drift): (i64, i64, i64, i64, i64) =
            sqlx::query_as(
                "SELECT COUNT(DISTINCT work.worker_owner)::bigint, \
                    COALESCE(MAX(work.unbound_preparation), 0)::bigint, \
                    COALESCE(MAX(work.unbound_terminal), 0)::bigint, \
                    COALESCE(MAX(work.demand_invalid), 0)::bigint, \
                    COUNT(*) FILTER (WHERE work.observed_revision <> root.revision)::bigint \
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
impl PostgresManagedSessionRepository {
    /// Unpaged, fail-closed wrapper for the Workspace-bound index queries.
    fn scoped_scan(workspace_id: &str, sessions: Vec<PersistedSession>) -> SessionScan {
        SessionScan {
            sessions: sessions
                .into_iter()
                .map(|session| ScopedPersistedSession {
                    workspace_id: workspace_id.to_string(),
                    session,
                })
                .collect(),
            ..SessionScan::default()
        }
    }

    async fn scan_reconcilable(
        &self,
        after: Option<&awaken_session_contract::SessionScanCursor>,
        preparation_only: bool,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let mut tx = self.pool.begin().await.map_err(storage)?;
        let page_size = usize::try_from(RECOVERY_BATCH_SIZE)
            .map_err(|error| storage(format!("invalid recovery batch size: {error}")))?;
        let mut scan = SessionScan::default();
        let sql = reconciliation_scan_sql(preparation_only, after.is_some(), "$1::text");
        let mut rows = sqlx::query(&sql)
            .bind(after.map(|cursor| cursor.session_id().as_str()))
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
                Some(awaken_session_contract::SessionScanCursor::after_session_id(session_id));
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
                    if preparation_only && !session.is_run_free_preparation_candidate() {
                        return Err(corrupt(format!(
                            "Session preparation candidate index disagrees with root {session_id}"
                        )));
                    }
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
                session_id: awaken_session_types::SessionId::from(
                    row.try_get::<String, _>("session_id").map_err(storage)?,
                ),
                reason: row.try_get("reason").map_err(storage)?,
            });
        }
        tx.commit().await.map_err(storage)?;
        Ok(scan)
    }

    async fn scan_worker_realization_owner(
        &self,
        worker_id: &str,
    ) -> Result<SessionScan, SessionRepositoryError> {
        // Filter by the one serialized root, including the historical unwrapped
        // encoding. A malformed row fails the read; it cannot prove an empty
        // Worker domain. The application alone classifies cleanup completion.
        let rows = sqlx::query(
            "SELECT scope_id, aggregate_json, revision FROM managed_session
             WHERE COALESCE(
               aggregate_json::jsonb #>> '{aggregate,realization,owner}',
               aggregate_json::jsonb #>> '{realization,owner}'
             ) = $1 ORDER BY session_id",
        )
        .bind(worker_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut scan = SessionScan::default();
        for row in rows {
            let workspace_id: String = row.try_get("scope_id").map_err(storage)?;
            let session = decode(EncodedSessionRow {
                aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
                revision: row.try_get("revision").map_err(storage)?,
            })
            .map_err(corrupt)?;
            if session
                .realization
                .as_ref()
                .is_none_or(|lease| lease.owner != worker_id)
            {
                return Err(corrupt(
                    "Worker realization-owner index disagrees with Session root",
                ));
            }
            scan.sessions.push(ScopedPersistedSession {
                workspace_id,
                session,
            });
        }
        Ok(scan)
    }

    async fn scan_workspace(
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

    async fn scan_vault_references(
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

    async fn scan_credential_source_references(
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
}

mod creation_intents;

#[async_trait]
impl LifecycleOutbox for PostgresManagedSessionRepository {
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
            Some(std::time::Duration::from_secs(5)),
            "P3/E3"
        );
        assert!(
            !options.get_test_before_acquire(),
            "P2/E2 only the bounded health hook may probe idle sockets"
        );
    }
}
