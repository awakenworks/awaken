// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Retains exact Session revisions within the Session transaction and scopes their reads and deletion.
use awaken_scoped_migration::{Migration, MigrationBundle};
use awaken_session_contract::SessionUpdateHistory;

use super::{
    EncodedSessionRow, PersistedSession, SessionRepositoryError, aggregate_str, db_revision,
    decode, storage,
};
use sqlx::{PgConnection, Row};

pub(super) fn replay_bundle() -> Result<MigrationBundle, String> {
    let migration = Migration::new(
        1,
        "retain managed Session versions and replay points",
        include_str!("migrations/V0001__session_replay.sql").trim(),
    )
    .map_err(|error| error.to_string())?;
    let diagnostics = Migration::new(
        2,
        "record scoped Session diagnostic coordinates and cursor key",
        include_str!("migrations/V0002__session_diagnostics.sql").trim(),
    )
    .map_err(|error| error.to_string())?;
    MigrationBundle::new(
        "awaken.managed_session_replay_pg",
        vec![migration, diagnostics],
    )
    .map_err(|error| error.to_string())
}

pub(super) async fn insert_session_revision(
    tx: &mut PgConnection,
    owner: &str,
    session: &PersistedSession,
) -> Result<(), SessionRepositoryError> {
    let update = SessionUpdateHistory::for_session(session)
        .and_then(|history| history.facts().last())
        .filter(|fact| fact.committed_revision() == session.revision);
    let recorded_at = update
        .and_then(|fact| fact.recorded_at_unix_ms())
        .map(i64::try_from)
        .transpose()
        .map_err(storage)?;
    sqlx::query(
        "INSERT INTO managed_session_revision
        (scope_id, session_id, revision, aggregate_json, recorded_at_ms, configuration_changes)
        VALUES ($1, $2, $3, $4,
            COALESCE($5, FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint), $6)",
    )
    .bind(owner)
    .bind(session.session_id.as_str())
    .bind(db_revision(session.revision)?)
    .bind(aggregate_str(session)?)
    .bind(recorded_at)
    .bind(update.map(|fact| sqlx::types::Json(fact.changes())))
    .execute(tx)
    .await
    .map_err(storage)?;
    Ok(())
}

pub async fn session_revision_at(
    tx: &mut PgConnection,
    owner: &str,
    session: &str,
    revision: i64,
) -> Result<Option<PersistedSession>, SessionRepositoryError> {
    let row = sqlx::query("SELECT revision, aggregate_json FROM managed_session_revision WHERE scope_id = $1 AND session_id = $2 AND revision = $3")
        .bind(owner)
        .bind(session)
        .bind(revision)
        .fetch_optional(tx)
        .await
        .map_err(storage)?;
    row.map(|row| {
        decode(EncodedSessionRow {
            revision: row.try_get("revision").map_err(storage)?,
            aggregate_json: row.try_get("aggregate_json").map_err(storage)?,
        })
        .map_err(super::corrupt)
    })
    .transpose()
}

/// Fence an activation archive against deletion of its owning Session.
pub async fn lock_session_for_run_archive(
    tx: &mut PgConnection,
    owner: &str,
    session: &str,
) -> Result<bool, sqlx::Error> {
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('managed_session_revision') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await?;
    if !installed {
        return Ok(false);
    }
    Ok(sqlx::query_scalar::<_, i64>("SELECT revision FROM managed_session WHERE scope_id = $1 AND session_id = $2 FOR KEY SHARE")
        .bind(owner).bind(session).fetch_optional(tx).await?.is_some())
}

pub(super) async fn delete_run_archives(
    tx: &mut PgConnection,
    owner: &str,
    session: &str,
) -> Result<(), SessionRepositoryError> {
    let installed: bool =
        sqlx::query_scalar("SELECT to_regclass('runtime_run_activation') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
    let has_membership: bool =
        sqlx::query_scalar("SELECT to_regclass('runtime_dispatch_session_membership') IS NOT NULL")
            .fetch_one(&mut *tx)
            .await
            .map_err(storage)?;
    if has_membership {
        sqlx::query("DELETE FROM runtime_dispatch_session_membership WHERE scope_id = $1 AND session_id = $2")
            .bind(owner).bind(session).execute(&mut *tx).await.map_err(storage)?;
    }
    if installed {
        sqlx::query("DELETE FROM runtime_run_activation WHERE scope_id = $1 AND session_id = $2")
            .bind(owner)
            .bind(session)
            .execute(tx)
            .await
            .map_err(storage)?;
    }
    Ok(())
}
