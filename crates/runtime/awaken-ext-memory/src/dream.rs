//! Durable storage port for cross-Session Dream jobs.
//!
//! The protocol adapter owns the job aggregate and serializes it as secret-free
//! JSON. This Dream port owns only durable records and Workspace Dream Agent
//! overrides; the concrete SQLite adapter lives with the Managed Session store,
//! beside the existing extraction work repository.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamJobRecord {
    pub job_id: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDreamAgentOverride {
    pub workspace_id: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamPolicyRecord {
    pub workspace_id: String,
    pub memory_store_id: String,
    pub data: String,
}

#[derive(Debug)]
pub enum DreamRepositoryError {
    Storage(String),
}

impl std::fmt::Display for DreamRepositoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(message) => {
                write!(formatter, "Dream repository failure: {message}")
            }
        }
    }
}

impl std::error::Error for DreamRepositoryError {}

pub trait DreamRepository: Send + Sync {
    fn dream_jobs(&self) -> Result<Vec<DreamJobRecord>, DreamRepositoryError>;

    fn upsert_dream_job(&self, record: DreamJobRecord) -> Result<(), DreamRepositoryError>;

    fn dream_agent_overrides(
        &self,
    ) -> Result<Vec<WorkspaceDreamAgentOverride>, DreamRepositoryError>;

    fn set_dream_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamRepositoryError>;

    fn dream_policies(&self) -> Result<Vec<DreamPolicyRecord>, DreamRepositoryError>;

    fn upsert_dream_policy(&self, record: DreamPolicyRecord) -> Result<(), DreamRepositoryError>;
}
