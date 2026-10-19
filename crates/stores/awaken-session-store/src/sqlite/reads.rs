// @formal evidence session.visible_row_always_owned
//! SELECT-only implementations over the same canonical SQLite Session Store.
//! Reconciliation/quarantine maintenance stays in the parent mutation owner.

use super::{
    EncodedSessionRow, PersistedSession, ScopedPersistedSession, SessionRepositoryError,
    SessionRepositoryReads, SessionScan, SqliteManagedSessionRepository, corrupt, decode,
    referenced_mcp_credential_source_ids, storage,
};

#[async_trait::async_trait]
impl SessionRepositoryReads for SqliteManagedSessionRepository {
    async fn get(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        let raw = sqlx::query_as::<_, (String, i64)>(
            "SELECT aggregate_json, revision
                     FROM managed_session WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        let Some((aggregate_json, revision)) = raw else {
            return Err(SessionRepositoryError::NotFound);
        };
        decode(EncodedSessionRow {
            aggregate_json,
            revision,
        })
        .map_err(corrupt)
    }

    async fn owner(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<String, SessionRepositoryError> {
        sqlx::query_scalar::<_, String>(
            "SELECT scope_id FROM managed_session WHERE session_id = ?1",
        )
        .bind(session_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?
        .ok_or(SessionRepositoryError::NotFound)
    }

    async fn scan(
        &self,
        query: awaken_session_state_contract::SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        use awaken_session_state_contract::SessionScanQuery;
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
        Option<awaken_session_state_contract::DeletedSessionEventAdmissions>,
        SessionRepositoryError,
    > {
        sqlx::query_as::<_, (i64, Option<String>)>("SELECT deleted_revision, event_admissions_json FROM managed_session_tombstone WHERE session_id=?1 AND scope_id=?2")
            .bind(session_id.as_str())
            .bind(owner_scope)
            .fetch_optional(&self.pool)
            .await
            .map_err(storage)?
            .map(|(revision, data)| crate::event_admissions::decode(revision, data))
            .transpose()
    }
}

impl SqliteManagedSessionRepository {
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

    async fn scan_worker_realization_owner(
        &self,
        worker_id: &str,
    ) -> Result<SessionScan, SessionRepositoryError> {
        // Read the one root field in both current and historical encodings.
        // Invalid JSON is an error, never evidence that no Session exists.
        let rows = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT scope_id, aggregate_json, revision FROM managed_session
                     WHERE COALESCE(
                       json_extract(aggregate_json, '$.aggregate.realization.owner'),
                       json_extract(aggregate_json, '$.realization.owner')
                     ) = ?1 ORDER BY session_id",
        )
        .bind(worker_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut scan = SessionScan::default();
        for (workspace_id, aggregate_json, revision) in rows {
            let session = decode(EncodedSessionRow {
                aggregate_json,
                revision,
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
        sqlx::query_as::<_, (String, i64)>(
            "SELECT aggregate_json, revision FROM managed_session
                     WHERE scope_id = ?1 ORDER BY session_id",
        )
        .bind(owner_scope)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?
        .into_iter()
        .map(|(aggregate_json, revision)| {
            decode(EncodedSessionRow {
                aggregate_json,
                revision,
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
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT session.aggregate_json, session.revision \
             FROM managed_session_vault_reference reference \
             JOIN managed_session session ON session.session_id = reference.session_id \
             WHERE reference.vault_id = ?1 AND session.scope_id = ?2 \
             ORDER BY session.session_id",
        )
        .bind(vault_id)
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut sessions = Vec::new();
        for (aggregate_json, revision) in rows {
            let session = decode(EncodedSessionRow {
                aggregate_json,
                revision,
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
        let rows = sqlx::query_as::<_, (String, i64)>(
            "SELECT session.aggregate_json, session.revision \
             FROM managed_session_credential_source_reference reference \
             JOIN managed_session session ON session.session_id = reference.session_id \
             WHERE reference.credential_source_id = ?1 AND session.scope_id = ?2 \
             ORDER BY session.session_id",
        )
        .bind(source_id.0.as_str())
        .bind(workspace_id)
        .fetch_all(&self.pool)
        .await
        .map_err(storage)?;
        let mut sessions = Vec::new();
        for (aggregate_json, revision) in rows {
            let session = decode(EncodedSessionRow {
                aggregate_json,
                revision,
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
