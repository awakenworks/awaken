// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: Static owner: the SQLite binding of the same Workspace MCP configuration repository.
// @formal reason: Dynamic path: identical validated whole-configuration writes and reads as the PostgreSQL
// @formal reason: binding, exercised by the same conformance so one backend cannot admit a configuration
// @formal reason: the other refuses. Evidence: product.admin_console.console_workflows plus the shared MCP
// @formal reason: configuration repository conformance; this is a product-requirement classification, not
// @formal reason: a direct proof claim.
use super::*;
use awaken_agent_config::mcp_config::{
    McpConfigError, McpConfiguration, McpConfigurationRepository, validate_mcp_configuration_write,
};
fn storage(error: impl std::fmt::Display) -> McpConfigError {
    McpConfigError::Storage(error.to_string())
}
#[async_trait::async_trait]
impl McpConfigurationRepository for SqliteAdminStore {
    async fn get_mcp_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<McpConfiguration>, McpConfigError> {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT revision,data FROM admin_mcp_configuration WHERE workspace_id=?1",
        )
        .bind(workspace)
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|(revision, data)| {
            let revision = u64::try_from(revision).map_err(storage)?;
            let value: McpConfiguration = serde_json::from_str(&data).map_err(storage)?;
            value.validate()?;
            if value.workspace_id != workspace || value.revision != revision {
                return Err(storage("MCP stored scope/revision mismatch"));
            }
            Ok(value)
        })
        .transpose()
    }
    async fn put_mcp_configuration(
        &self,
        expected: u64,
        next: &McpConfiguration,
    ) -> Result<(), McpConfigError> {
        validate_mcp_configuration_write(expected, next)?;
        let data = serde_json::to_string(next).map_err(storage)?;
        let revision = i64::try_from(next.revision).map_err(storage)?;
        let expected = i64::try_from(expected).map_err(storage)?;
        let query = if expected == 0 {
            sqlx::query("INSERT INTO admin_mcp_configuration(workspace_id,revision,data) VALUES(?1,?2,?3) ON CONFLICT(workspace_id) DO NOTHING")
                .bind(&next.workspace_id)
                .bind(revision)
                .bind(data)
        } else {
            sqlx::query("UPDATE admin_mcp_configuration SET revision=?2,data=?3 WHERE workspace_id=?1 AND revision=?4")
                .bind(&next.workspace_id)
                .bind(revision)
                .bind(data)
                .bind(expected)
        };
        let changed = query.execute(&self.pool).await.map_err(storage)?;
        if changed.rows_affected() != 1 {
            return Err(McpConfigError::Conflict);
        }
        Ok(())
    }
    async fn mcp_configuration_workspaces(&self) -> Result<Vec<String>, McpConfigError> {
        sqlx::query_scalar("SELECT workspace_id FROM admin_mcp_configuration ORDER BY workspace_id")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_config::mcp_config::McpCatalogSnapshot;
    use std::sync::Arc;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_mcp_cas_restart_scope_and_corrupt_readback() {
        // S1 first write -> durable revision 1; S2 two writers read revision 1
        // -> exactly one revision 2; S3 reopen -> same data; S4 wrong Workspace
        // -> absent; S5 corrupt persisted scope -> fail, never relabel it.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.sqlite");
        let store = Arc::new(
            SqliteAdminStore::open(path.to_str().unwrap())
                .await
                .unwrap(),
        );
        let mut initial = McpConfiguration::new(
            "w".into(),
            McpCatalogSnapshot {
                schema_version: 1,
                revision: 1,
                servers: vec![],
            },
        );
        initial.advance(0).unwrap();
        store.put_mcp_configuration(0, &initial).await.unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let writers = (0..2)
            .map(|i| {
                let store = store.clone();
                let barrier = barrier.clone();
                let mut next = initial.clone();
                next.advance(1).unwrap();
                next.catalog_url = Some(format!("https://source-{i}.example.test/catalog"));
                tokio::spawn(async move {
                    barrier.wait().await;
                    store.put_mcp_configuration(1, &next).await
                })
            })
            .collect::<Vec<_>>();
        let mut results = Vec::new();
        for writer in writers {
            results.push(writer.await.unwrap());
        }
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "S2");
        let current = store.get_mcp_configuration("w").await.unwrap().unwrap();
        assert_eq!(current.revision, 2);
        let reopened = SqliteAdminStore::open(path.to_str().unwrap())
            .await
            .unwrap();
        assert_eq!(
            reopened.get_mcp_configuration("w").await.unwrap(),
            Some(current)
        );
        assert!(
            reopened
                .get_mcp_configuration("other")
                .await
                .unwrap()
                .is_none(),
            "S4"
        );
        sqlx::query(
            "UPDATE admin_mcp_configuration SET data=json_set(data,'$.workspace_id','wrong')",
        )
        .execute(&reopened.pool)
        .await
        .unwrap();
        assert!(reopened.get_mcp_configuration("w").await.is_err(), "S5");
    }
}
