// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: Static owner: the PostgreSQL binding of the Workspace telemetry configuration repository.
// @formal reason: Dynamic path: the same validated compare-and-set write and scoped readback as the
// @formal reason: SQLite binding, so one backend cannot admit a narrowing the other refuses and a row
// @formal reason: relabelled to another Workspace is never served. Evidence:
// @formal reason: product.admin_console.console_workflows plus the PostgreSQL store CAS/restart test;
// @formal reason: this is a product-requirement classification, not a direct proof claim.
use super::PostgresAdminStore;
use awaken_config_resolver::{
    TelemetryConfigError, WorkspaceTelemetryConfiguration, WorkspaceTelemetryRepository,
    validate_stored_telemetry_configuration, validate_telemetry_configuration_write,
};
use sqlx::Row;
use sqlx::types::Json;

fn storage(error: impl std::fmt::Display) -> TelemetryConfigError {
    TelemetryConfigError::Storage(error.to_string())
}

#[async_trait::async_trait]
impl WorkspaceTelemetryRepository for PostgresAdminStore {
    async fn get_telemetry_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<WorkspaceTelemetryConfiguration>, TelemetryConfigError> {
        let row = sqlx::query(
            "SELECT revision,data FROM admin_telemetry_configuration WHERE workspace_id=$1",
        )
        .bind(workspace)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|row| {
            let revision: i64 = row.try_get("revision").map_err(storage)?;
            let revision = u64::try_from(revision).map_err(storage)?;
            let Json(value): Json<WorkspaceTelemetryConfiguration> =
                row.try_get("data").map_err(storage)?;
            validate_stored_telemetry_configuration(workspace, revision, &value)?;
            Ok(value)
        })
        .transpose()
    }

    async fn put_telemetry_configuration(
        &self,
        expected: u64,
        next: &WorkspaceTelemetryConfiguration,
    ) -> Result<(), TelemetryConfigError> {
        validate_telemetry_configuration_write(expected, next)?;
        let expected = i64::try_from(expected).map_err(storage)?;
        let revision = i64::try_from(next.revision).map_err(storage)?;
        let query = if expected == 0 {
            sqlx::query(
                "INSERT INTO admin_telemetry_configuration(workspace_id,revision,data) \
                 VALUES($1,$2,$3) ON CONFLICT(workspace_id) DO NOTHING",
            )
            .bind(&next.workspace_id)
            .bind(revision)
            .bind(Json(next))
        } else {
            sqlx::query(
                "UPDATE admin_telemetry_configuration SET revision=$2,data=$3 \
                 WHERE workspace_id=$1 AND revision=$4",
            )
            .bind(&next.workspace_id)
            .bind(revision)
            .bind(Json(next))
            .bind(expected)
        };
        let result = query.execute(&self.pool).await.map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(TelemetryConfigError::Conflict);
        }
        Ok(())
    }
}
