//! Durable Dream job and Workspace-override adapters.

use awaken_ext_memory::{
    DreamJobRecord, DreamPolicyRecord, DreamRepository, DreamRepositoryError,
    WorkspaceDreamAgentOverride,
};
use rusqlite::params;

use crate::SqliteManagedSessionRepository;

fn storage(error: impl std::fmt::Display) -> DreamRepositoryError {
    DreamRepositoryError::Storage(error.to_string())
}

impl DreamRepository for SqliteManagedSessionRepository {
    fn dream_jobs(&self) -> Result<Vec<DreamJobRecord>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare("SELECT job_id, data FROM managed_dream ORDER BY job_id")
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DreamJobRecord {
                    job_id: row.get(0)?,
                    data: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn upsert_dream_job(&self, record: DreamJobRecord) -> Result<(), DreamRepositoryError> {
        self.conn
            .lock()
            .map_err(storage)?
            .execute(
                "INSERT INTO managed_dream (job_id, data) VALUES (?1, ?2)
                 ON CONFLICT(job_id) DO UPDATE SET data=excluded.data",
                params![record.job_id, record.data],
            )
            .map_err(storage)?;
        Ok(())
    }

    fn dream_agent_overrides(
        &self,
    ) -> Result<Vec<WorkspaceDreamAgentOverride>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, agent_id FROM managed_dream_agent_override
                 ORDER BY workspace_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(WorkspaceDreamAgentOverride {
                    workspace_id: row.get(0)?,
                    agent_id: row.get(1)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn set_dream_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        match agent_id {
            Some(agent_id) => conn.execute(
                "INSERT INTO managed_dream_agent_override (workspace_id, agent_id)
                 VALUES (?1, ?2)
                 ON CONFLICT(workspace_id) DO UPDATE SET agent_id=excluded.agent_id",
                params![workspace_id, agent_id],
            ),
            None => conn.execute(
                "DELETE FROM managed_dream_agent_override WHERE workspace_id=?1",
                params![workspace_id],
            ),
        }
        .map_err(storage)?;
        Ok(())
    }

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamRepositoryError> {
        let conn = self.conn.lock().map_err(storage)?;
        let mut statement = conn
            .prepare(
                "SELECT workspace_id, memory_store_id, data FROM managed_dream_policy \
                 ORDER BY workspace_id, memory_store_id",
            )
            .map_err(storage)?;
        statement
            .query_map([], |row| {
                Ok(DreamPolicyRecord {
                    workspace_id: row.get(0)?,
                    memory_store_id: row.get(1)?,
                    data: row.get(2)?,
                })
            })
            .map_err(storage)?
            .map(|row| row.map_err(storage))
            .collect()
    }

    fn upsert_dream_policy(&self, record: DreamPolicyRecord) -> Result<(), DreamRepositoryError> {
        self.conn
            .lock()
            .map_err(storage)?
            .execute(
                "INSERT INTO managed_dream_policy (workspace_id, memory_store_id, data) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(workspace_id, memory_store_id) \
                 DO UPDATE SET data=excluded.data",
                params![record.workspace_id, record.memory_store_id, record.data],
            )
            .map_err(storage)?;
        Ok(())
    }
}
