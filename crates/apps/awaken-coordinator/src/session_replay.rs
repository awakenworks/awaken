// @formal surface requirement=analytics.observability.projection_only
// @formal reason: Reopens committed Session and Runtime history at a fixed point; bookmarks retain read coordinates only.
//! PostgreSQL adapter for a fixed Managed Session replay point.

use awaken_agent_contract::ThreadId;
use awaken_dispatch_contract::run_dispatch::ArchivedRunActivation;
use awaken_session_application::{
    SessionAccess,
    replay::{
        ReplayPointId, ReplayRunActivation, SessionReplayError, SessionReplayInputs,
        SessionReplayPoint, SessionReplayReads,
    },
};
use awaken_session_types::{SessionId, SessionRevision};
use sqlx::{PgPool, Postgres, Transaction};

mod causality;
pub(crate) mod timeline;

pub(crate) async fn archived_admission(
    pool: &PgPool,
    session: &str,
    thread: &ThreadId,
    run: &awaken_agent_contract::RunId,
) -> Result<Option<ArchivedRunActivation>, sqlx::Error> {
    let mut connection = pool.acquire().await?;
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('managed_session_revision') IS NOT NULL")
            .fetch_one(&mut *connection)
            .await?;
    if !installed {
        return Ok(None);
    }
    let row: Option<sqlx::types::Json<ArchivedRunActivation>> = sqlx::query_scalar(
        "SELECT a.data FROM runtime_run_activation a JOIN managed_session s USING (scope_id, session_id)
         WHERE a.session_id = $1 AND a.thread_id = $2 AND a.run_id = $3",
    ).bind(session).bind(&thread.0).bind(&run.0).fetch_optional(&mut *connection).await?;
    Ok(row.map(|sqlx::types::Json(archive)| archive))
}

pub struct PostgresSessionReplay {
    pool: PgPool,
}

impl PostgresSessionReplay {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn state_at(
        &self,
        access: &SessionAccess,
        point: &ReplayPointId,
    ) -> Result<SessionReplayInputs, SessionReplayError> {
        self.load_at(access, point).await
    }

    pub async fn capture_point(
        &self,
        access: &SessionAccess,
    ) -> Result<SessionReplayPoint, SessionReplayError> {
        self.capture_for(access.owner_scope(), access.session_id())
            .await
    }

    pub(super) async fn capture_for(
        &self,
        scope: &str,
        session_id: &SessionId,
    ) -> Result<SessionReplayPoint, SessionReplayError> {
        let mut tx = self.begin().await?;
        let session = session_id.as_str();
        let (revision, retained): (i64, bool) = sqlx::query_as(
            "SELECT s.revision, r.revision IS NOT NULL FROM managed_session s
             LEFT JOIN managed_session_revision r USING (scope_id, session_id, revision)
             WHERE s.scope_id = $1 AND s.session_id = $2",
        )
        .bind(scope)
        .bind(session)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?
        .ok_or(SessionReplayError::NotFound)?;
        if !retained {
            return Err(history("current Session revision was not archived"));
        }
        let through = awaken_store_postgres::replay_head(&mut tx)
            .await
            .map_err(unavailable)?;
        let id = ReplayPointId(uuid::Uuid::now_v7().to_string());
        sqlx::query("INSERT INTO managed_session_replay_point (scope_id, session_id, point_id, revision, runtime_through) VALUES ($1, $2, $3, $4, $5)")
            .bind(scope).bind(session).bind(&id.0).bind(revision).bind(through)
            .execute(&mut *tx).await.map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(SessionReplayPoint {
            id,
            revision: SessionRevision(positive(revision)?),
        })
    }

    async fn begin(&self) -> Result<Transaction<'_, Postgres>, SessionReplayError> {
        let mut tx = self.pool.begin().await.map_err(unavailable)?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        Ok(tx)
    }
}

#[async_trait::async_trait]
impl SessionReplayReads for PostgresSessionReplay {
    async fn contains_thread(
        &self,
        access: &SessionAccess,
        thread: &ThreadId,
    ) -> Result<bool, SessionReplayError> {
        sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1 FROM managed_session s
                WHERE s.scope_id = $1 AND s.session_id = $2 AND (
                    EXISTS (SELECT 1 FROM runtime_dispatch_session_membership m
                      WHERE m.scope_id = s.scope_id AND m.session_id = s.session_id AND m.thread_id = $3)
                    OR EXISTS (SELECT 1 FROM runtime_run_activation a
                      WHERE a.scope_id = s.scope_id AND a.session_id = s.session_id AND a.thread_id = $3)
                )
            )",
        )
        .bind(access.owner_scope()).bind(access.session_id().as_str()).bind(&thread.0)
        .fetch_one(&self.pool).await.map_err(unavailable)
    }

    async fn timeline(
        &self,
        access: &SessionAccess,
        query: &awaken_session_application::replay::TimelineQuery,
        cursor: Option<&awaken_session_application::replay::TimelineCursor>,
    ) -> Result<awaken_session_application::replay::TimelinePage, SessionReplayError> {
        timeline::timeline(
            self,
            access.owner_scope(),
            access.session_id(),
            query,
            cursor,
        )
        .await
    }

    async fn run_evidence_at(
        &self,
        access: &SessionAccess,
        run: &awaken_agent_contract::RunId,
        snapshot: &awaken_session_application::replay::TimelineSnapshot,
    ) -> Result<awaken_session_application::replay::RunEvidence, SessionReplayError> {
        timeline::run_evidence_at(
            self,
            access.owner_scope(),
            access.session_id(),
            run,
            snapshot,
        )
        .await
    }

    async fn load_at(
        &self,
        access: &SessionAccess,
        point: &ReplayPointId,
    ) -> Result<SessionReplayInputs, SessionReplayError> {
        self.load_for(access.owner_scope(), access.session_id(), point)
            .await
    }
}

impl PostgresSessionReplay {
    pub(super) async fn load_for(
        &self,
        scope: &str,
        session_id: &SessionId,
        point: &ReplayPointId,
    ) -> Result<SessionReplayInputs, SessionReplayError> {
        let mut tx = self.begin().await?;
        let (revision, through): (i64, i64) = sqlx::query_as(
            "SELECT p.revision, p.runtime_through FROM managed_session_replay_point p
             JOIN managed_session s USING (scope_id, session_id)
             WHERE p.scope_id = $1 AND p.session_id = $2 AND p.point_id = $3",
        )
        .bind(scope)
        .bind(session_id.as_str())
        .bind(&point.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(unavailable)?
        .ok_or(SessionReplayError::NotFound)?;
        let session = awaken_session_store::session_revision_at(
            &mut tx,
            scope,
            session_id.as_str(),
            revision,
        )
        .await
        .map_err(unavailable)?
        .ok_or_else(|| history("point's Session revision was not archived"))?;
        let root = awaken_store_postgres::replay_thread_prefix(
            &mut tx,
            &ThreadId(session_id.to_string()),
            through,
        )
        .await
        .map_err(unavailable)?;
        let mut activations = Vec::new();
        for (thread, run, sequence, sqlx::types::Json(data)) in
            awaken_store_postgres::read_run_archives(&mut tx, scope, session_id.as_str(), through)
                .await
                .map_err(unavailable)?
        {
            let configuration: ArchivedRunActivation =
                serde_json::from_value(data).map_err(history)?;
            if configuration.activation.thread_id.0 != thread
                || configuration.activation.run_id.0 != run
            {
                return Err(history("activation archive differs from its Run key"));
            }
            activations.push(ReplayRunActivation {
                first_commit: positive(sequence)?,
                configuration,
            });
        }
        let (child_threads, mut messages) =
            causality::project_sends(session_id, &root, &activations)?;
        let mut threads = vec![root];
        for thread in child_threads {
            threads.push(
                awaken_store_postgres::replay_thread_prefix(&mut tx, &thread, through)
                    .await
                    .map_err(unavailable)?,
            );
        }
        causality::complete_messages(session_id, &threads, &activations, &mut messages)?;
        activations.retain(|archive| {
            threads
                .iter()
                .any(|thread| thread.thread_id == archive.configuration.activation.thread_id)
        });
        tx.commit().await.map_err(unavailable)?;
        Ok(SessionReplayInputs {
            point: SessionReplayPoint {
                id: point.clone(),
                revision: SessionRevision(positive(revision)?),
            },
            session,
            threads,
            activations,
            messages,
        })
    }
}

fn positive(value: i64) -> Result<u64, SessionReplayError> {
    u64::try_from(value).map_err(history)
}

fn unavailable(error: impl std::fmt::Display) -> SessionReplayError {
    SessionReplayError::Unavailable(error.to_string())
}

fn history(error: impl std::fmt::Display) -> SessionReplayError {
    SessionReplayError::HistoryUnavailable(error.to_string())
}
