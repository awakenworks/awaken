//! Durable storage port for cross-Session Memory Consolidation jobs.
//!
//! The protocol adapter owns the job aggregate and serializes it as secret-free
//! JSON. This Memory bounded-context port owns only durable records and Workspace
//! consolidator overrides; concrete SQLite/Postgres adapters live with the
//! Managed Session store, beside the existing extraction work repository.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConsolidationJobRecord {
    pub job_id: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMemoryConsolidatorOverride {
    pub workspace_id: String,
    pub agent_id: String,
}

#[derive(Debug)]
pub enum MemoryConsolidationRepositoryError {
    Storage(String),
}

impl std::fmt::Display for MemoryConsolidationRepositoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(message) => {
                write!(
                    formatter,
                    "Memory consolidation repository failure: {message}"
                )
            }
        }
    }
}

impl std::error::Error for MemoryConsolidationRepositoryError {}

pub trait MemoryConsolidationRepository: Send + Sync {
    fn consolidation_jobs(
        &self,
    ) -> Result<Vec<MemoryConsolidationJobRecord>, MemoryConsolidationRepositoryError>;

    fn upsert_consolidation_job(
        &self,
        record: MemoryConsolidationJobRecord,
    ) -> Result<(), MemoryConsolidationRepositoryError>;

    fn memory_consolidator_overrides(
        &self,
    ) -> Result<Vec<WorkspaceMemoryConsolidatorOverride>, MemoryConsolidationRepositoryError>;

    fn set_memory_consolidator_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), MemoryConsolidationRepositoryError>;
}
