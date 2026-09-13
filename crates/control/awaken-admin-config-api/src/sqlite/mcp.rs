use super::*;
use awaken_agent_config::mcp_config::{
    McpConfigError, McpConfiguration, McpConfigurationRepository, validate_mcp_configuration_write,
};
fn storage(error: impl std::fmt::Display) -> McpConfigError {
    McpConfigError::Storage(error.to_string())
}
impl McpConfigurationRepository for SqliteAdminStore {
    fn get_mcp_configuration(
        &self,
        workspace: &str,
    ) -> Result<Option<McpConfiguration>, McpConfigError> {
        let conn = self.conn.lock().map_err(storage)?;
        let row: Option<(u64, String)> = conn
            .query_row(
                "SELECT revision,data FROM admin_mcp_configuration WHERE workspace_id=?1",
                [workspace],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(storage)?;
        row.map(|(revision, data)| {
            let value: McpConfiguration = serde_json::from_str(&data).map_err(storage)?;
            value.validate()?;
            if value.workspace_id != workspace || value.revision != revision {
                return Err(storage("MCP stored scope/revision mismatch"));
            }
            Ok(value)
        })
        .transpose()
    }
    fn put_mcp_configuration(
        &self,
        expected: u64,
        next: &McpConfiguration,
    ) -> Result<(), McpConfigError> {
        validate_mcp_configuration_write(expected, next)?;
        let data = serde_json::to_string(next).map_err(storage)?;
        let conn = self.conn.lock().map_err(storage)?;
        let changed=if expected==0 {
            conn.execute("INSERT INTO admin_mcp_configuration(workspace_id,revision,data) VALUES(?1,?2,?3) ON CONFLICT(workspace_id) DO NOTHING",params![next.workspace_id,next.revision,data])
        } else {
            conn.execute("UPDATE admin_mcp_configuration SET revision=?2,data=?3 WHERE workspace_id=?1 AND revision=?4",params![next.workspace_id,next.revision,data,expected])
        }.map_err(storage)?;
        if changed != 1 {
            return Err(McpConfigError::Conflict);
        }
        Ok(())
    }
    fn mcp_configuration_workspaces(&self) -> Result<Vec<String>, McpConfigError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut stmt = conn
            .prepare("SELECT workspace_id FROM admin_mcp_configuration ORDER BY workspace_id")
            .map_err(storage)?;
        stmt.query_map([], |row| row.get(0))
            .map_err(storage)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_config::mcp_config::McpCatalogSnapshot;
    #[test]
    fn sqlite_mcp_cas_restart_scope_and_corrupt_readback() {
        // S1 first write -> durable revision 1; S2 two writers read revision 1
        // -> exactly one revision 2; S3 reopen -> same data; S4 wrong Workspace
        // -> absent; S5 corrupt persisted scope -> fail, never relabel it.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.sqlite");
        let store = Arc::new(SqliteAdminStore::open(path.to_str().unwrap()).unwrap());
        let mut initial = McpConfiguration::new(
            "w".into(),
            McpCatalogSnapshot {
                schema_version: 1,
                revision: 1,
                servers: vec![],
            },
        );
        initial.advance(0).unwrap();
        store.put_mcp_configuration(0, &initial).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writers = (0..2)
            .map(|i| {
                let store = store.clone();
                let barrier = barrier.clone();
                let mut next = initial.clone();
                next.advance(1).unwrap();
                next.catalog_url = Some(format!("https://source-{i}.example.test/catalog"));
                std::thread::spawn(move || {
                    barrier.wait();
                    store.put_mcp_configuration(1, &next)
                })
            })
            .collect::<Vec<_>>();
        let results = writers
            .into_iter()
            .map(|w| w.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "S2");
        let current = store.get_mcp_configuration("w").unwrap().unwrap();
        assert_eq!(current.revision, 2);
        let reopened = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
        assert_eq!(reopened.get_mcp_configuration("w").unwrap(), Some(current));
        assert!(
            reopened.get_mcp_configuration("other").unwrap().is_none(),
            "S4"
        );
        reopened
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE admin_mcp_configuration SET data=json_set(data,'$.workspace_id','wrong')",
                [],
            )
            .unwrap();
        assert!(reopened.get_mcp_configuration("w").is_err(), "S5");
    }
}
