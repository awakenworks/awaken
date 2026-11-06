// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: Static owner: the SQLite binding of the Workspace telemetry configuration repository.
// @formal reason: Dynamic path: the same validated compare-and-set write and scoped readback as the
// @formal reason: PostgreSQL binding, so one backend cannot admit a narrowing the other refuses and a row
// @formal reason: relabelled to another Workspace is never served. Evidence:
// @formal reason: product.admin_console.console_workflows plus this module's CAS/restart/corruption test;
// @formal reason: this is a product-requirement classification, not a direct proof claim.
use super::SqliteAdminStore;
use awaken_config_resolver::{
    TelemetryConfigError, WorkspaceTelemetryConfiguration, WorkspaceTelemetryRepository,
    validate_stored_telemetry_configuration, validate_telemetry_configuration_write,
};

fn storage(error: impl std::fmt::Display) -> TelemetryConfigError {
    TelemetryConfigError::Storage(error.to_string())
}

#[async_trait::async_trait]
impl WorkspaceTelemetryRepository for SqliteAdminStore {
    async fn get_telemetry_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<WorkspaceTelemetryConfiguration>, TelemetryConfigError> {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT revision,data FROM admin_telemetry_configuration WHERE workspace_id=?1",
        )
        .bind(workspace)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|(revision, data)| {
            let revision = u64::try_from(revision).map_err(storage)?;
            let value: WorkspaceTelemetryConfiguration =
                serde_json::from_str(&data).map_err(storage)?;
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
        let data = serde_json::to_string(next).map_err(storage)?;
        let revision = i64::try_from(next.revision).map_err(storage)?;
        let expected = i64::try_from(expected).map_err(storage)?;
        let query = if expected == 0 {
            sqlx::query(
                "INSERT INTO admin_telemetry_configuration(workspace_id,revision,data) \
                 VALUES(?1,?2,?3) ON CONFLICT(workspace_id) DO NOTHING",
            )
            .bind(&next.workspace_id)
            .bind(revision)
            .bind(data)
        } else {
            sqlx::query(
                "UPDATE admin_telemetry_configuration SET revision=?2,data=?3 \
                 WHERE workspace_id=?1 AND revision=?4",
            )
            .bind(&next.workspace_id)
            .bind(revision)
            .bind(data)
            .bind(expected)
        };
        let changed = query.execute(&self.pool).await.map_err(storage)?;
        if changed.rows_affected() != 1 {
            return Err(TelemetryConfigError::Conflict);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::ContentCapture;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_telemetry_cas_restart_scope_and_corrupt_readback() {
        // S1 first write -> durable revision 1; S2 two writers at revision 1 ->
        // exactly one revision 2; S3 reopen -> same row; S4 other Workspace ->
        // absent; S5 stale insert -> Conflict; S6 relabelled scope -> error.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("telemetry.sqlite");
        let store = Arc::new(
            SqliteAdminStore::open(path.to_str().unwrap())
                .await
                .unwrap(),
        );
        let initial = WorkspaceTelemetryConfiguration::new("w".into())
            .narrowed(0, Some(ContentCapture::Structured), ContentCapture::Full)
            .unwrap();
        store
            .put_telemetry_configuration(0, &initial)
            .await
            .unwrap();
        assert_eq!(
            store.put_telemetry_configuration(0, &initial).await,
            Err(TelemetryConfigError::Conflict),
            "S5"
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let writers = [ContentCapture::Off, ContentCapture::Structured]
            .into_iter()
            .map(|level| {
                let store = store.clone();
                let barrier = barrier.clone();
                let next = initial
                    .narrowed(1, Some(level), ContentCapture::Full)
                    .unwrap();
                tokio::spawn(async move {
                    barrier.wait().await;
                    store.put_telemetry_configuration(1, &next).await
                })
            })
            .collect::<Vec<_>>();
        let mut results = Vec::new();
        for writer in writers {
            results.push(writer.await.unwrap());
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "S2");
        let current = store
            .get_telemetry_configuration("w")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, 2);
        let reopened = SqliteAdminStore::open(path.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reopened.get_telemetry_configuration("w").await.unwrap(),
            Some(current),
            "S3"
        );
        assert!(
            reopened
                .get_telemetry_configuration("other")
                .await
                .unwrap()
                .is_none(),
            "S4"
        );
        sqlx::query(
            "UPDATE admin_telemetry_configuration SET data=json_set(data,'$.workspace_id','wrong')",
        )
        .execute(&reopened.pool)
        .await
        .unwrap();
        assert!(
            reopened.get_telemetry_configuration("w").await.is_err(),
            "S6"
        );
    }
}
