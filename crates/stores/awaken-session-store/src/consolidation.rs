//! Durable Memory Consolidation job and Workspace-override adapters.

use awaken_ext_memory::{
    MemoryConsolidationJobRecord, MemoryConsolidationRepository,
    MemoryConsolidationRepositoryError, WorkspaceMemoryConsolidatorOverride,
};
use rusqlite::params;

use crate::SqliteManagedSessionRepository;

fn storage(error: impl std::fmt::Display) -> MemoryConsolidationRepositoryError {
    MemoryConsolidationRepositoryError::Storage(error.to_string())
}

impl MemoryConsolidationRepository for SqliteManagedSessionRepository {
    fn consolidation_jobs(
        &self,
    ) -> Result<Vec<MemoryConsolidationJobRecord>, MemoryConsolidationRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare("SELECT job_id, data FROM managed_memory_consolidation ORDER BY job_id")
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(MemoryConsolidationJobRecord {
                    job_id: row.get(0)?,
                    data: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn upsert_consolidation_job(
        &self,
        record: MemoryConsolidationJobRecord,
    ) -> Result<(), MemoryConsolidationRepositoryError> {
        self.conn
            .lock()
            .map_err(storage)?
            .execute(
                "INSERT INTO managed_memory_consolidation (job_id, data) VALUES (?1, ?2)
                 ON CONFLICT(job_id) DO UPDATE SET data=excluded.data",
                params![record.job_id, record.data],
            )
            .map_err(storage)?;
        Ok(())
    }

    fn memory_consolidator_overrides(
        &self,
    ) -> Result<Vec<WorkspaceMemoryConsolidatorOverride>, MemoryConsolidationRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, agent_id FROM managed_memory_consolidator_override
                 ORDER BY workspace_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(WorkspaceMemoryConsolidatorOverride {
                    workspace_id: row.get(0)?,
                    agent_id: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn set_memory_consolidator_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), MemoryConsolidationRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        match agent_id {
            Some(agent_id) => conn.execute(
                "INSERT INTO managed_memory_consolidator_override (workspace_id, agent_id)
                 VALUES (?1, ?2)
                 ON CONFLICT(workspace_id) DO UPDATE SET agent_id=excluded.agent_id",
                params![workspace_id, agent_id],
            ),
            None => conn.execute(
                "DELETE FROM managed_memory_consolidator_override WHERE workspace_id=?1",
                params![workspace_id],
            ),
        }
        .map_err(storage)?;
        Ok(())
    }
}
