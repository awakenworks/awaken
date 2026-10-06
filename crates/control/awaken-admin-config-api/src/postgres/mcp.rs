// @formal surface requirement=product.admin_console.console_workflows
// @formal reason: Static owner: the PostgreSQL binding of the Workspace MCP configuration repository.
// @formal reason: Dynamic path: a validated configuration is written and read back whole, so a partially
// @formal reason: applied definition set cannot become the Workspace's answer; credentials remain
// @formal reason: references this store never materializes. Evidence:
// @formal reason: product.admin_console.console_workflows plus the shared MCP configuration repository
// @formal reason: conformance; this is a product-requirement classification, not a direct proof claim.
use super::*;
use awaken_agent_config::mcp_config::{
    McpConfigError, McpConfiguration, McpConfigurationRepository, validate_mcp_configuration_write,
};
fn storage(error: impl std::fmt::Display) -> McpConfigError {
    McpConfigError::Storage(error.to_string())
}
#[async_trait::async_trait]
impl McpConfigurationRepository for PostgresAdminStore {
    async fn get_mcp_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<McpConfiguration>, McpConfigError> {
        let workspace = workspace.to_string();
        let pool = &self.pool;
        let row =
            sqlx::query("SELECT revision,data FROM admin_mcp_configuration WHERE workspace_id=$1")
                .bind(&workspace)
                .fetch_optional(pool)
                .await
                .map_err(storage)?;
        row.map(|row| {
            let revision: i64 = row.try_get("revision").map_err(storage)?;
            let Json(value): Json<McpConfiguration> = row.try_get("data").map_err(storage)?;
            value.validate()?;
            if value.workspace_id != workspace
                || i64::try_from(value.revision).ok() != Some(revision)
            {
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
        let pool = &self.pool;
        let expected = i64::try_from(expected).map_err(storage)?;
        let revision = i64::try_from(next.revision).map_err(storage)?;
        let sql = if expected == 0 {
            "INSERT INTO admin_mcp_configuration(workspace_id,revision,data) VALUES($1,$2,$3) ON CONFLICT(workspace_id) DO NOTHING"
        } else {
            "UPDATE admin_mcp_configuration SET revision=$2,data=$3 WHERE workspace_id=$1 AND revision=$4"
        };
        let query = sqlx::query(sql)
            .bind(&next.workspace_id)
            .bind(revision)
            .bind(Json(next));
        let result = if expected == 0 {
            query.execute(pool).await
        } else {
            query.bind(expected).execute(pool).await
        }
        .map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(McpConfigError::Conflict);
        }
        Ok(())
    }
    async fn mcp_configuration_workspaces(&self) -> Result<Vec<String>, McpConfigError> {
        let pool = &self.pool;
        sqlx::query_scalar("SELECT workspace_id FROM admin_mcp_configuration ORDER BY workspace_id")
            .fetch_all(pool)
            .await
            .map_err(storage)
    }
}
