//! Memory-consolidation application service behind the public Dreams adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_ext_memory::{MemoryConsolidationJobRecord, MemoryConsolidationRepository};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{
    Dream, DreamCreateParams, DreamError, DreamInput, DreamListParams, DreamModelConfig,
    DreamOutput, DreamPage, DreamStatus, DreamUsage,
};

const MAX_INSTRUCTIONS_CHARS: usize = 4096;
const MAX_SESSIONS: usize = 100;
const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 100;
const SUPPORTED_MODELS: &[&str] = &[
    "claude-fable-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
];

pub const BUILT_IN_MEMORY_CONSOLIDATOR_AGENT_ID: &str = "awaken_builtin_memory_consolidator";

fn default_memory_consolidator_agent_id() -> String {
    BUILT_IN_MEMORY_CONSOLIDATOR_AGENT_ID.into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsolidationAgentSelection {
    pub agent_id: String,
}

fn default_memory_consolidator_agent_selection() -> MemoryConsolidationAgentSelection {
    MemoryConsolidationAgentSelection {
        agent_id: default_memory_consolidator_agent_id(),
    }
}

#[derive(Debug, Clone)]
pub struct MemoryConsolidationRequest {
    pub job_id: String,
    pub workspace_id: String,
    pub source_memory_store_id: String,
    pub session_ids: Vec<String>,
    pub model: DreamModelConfig,
    pub request_guidance: Option<String>,
    pub agent_selection: MemoryConsolidationAgentSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConsolidationPreparation {
    pub result_memory_store_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryConsolidationFailure {
    pub kind: String,
    pub message: String,
}

impl MemoryConsolidationFailure {
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct MemoryConsolidationCancellation(Arc<AtomicBool>);

impl MemoryConsolidationCancellation {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The one execution seam. Production composes existing Memory, Files, Session,
/// and Runtime authorities here; tests use a deterministic implementation.
#[async_trait]
pub trait MemoryConsolidationWorker: Send + Sync {
    async fn validate_inputs(
        &self,
        request: &MemoryConsolidationRequest,
    ) -> Result<(), MemoryConsolidationFailure>;

    async fn prepare(
        &self,
        request: &MemoryConsolidationRequest,
    ) -> Result<MemoryConsolidationPreparation, MemoryConsolidationFailure>;

    async fn execute(
        &self,
        request: &MemoryConsolidationRequest,
        preparation: &MemoryConsolidationPreparation,
        cancellation: MemoryConsolidationCancellation,
    ) -> Result<DreamUsage, MemoryConsolidationFailure>;

    async fn cancel(&self, _session_id: Option<&str>) -> Result<(), MemoryConsolidationFailure> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryConsolidationJob {
    id: String,
    workspace_id: String,
    status: DreamStatus,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    #[serde(default = "default_memory_consolidator_agent_selection")]
    agent_selection: MemoryConsolidationAgentSelection,
    result_memory_store_id: Option<String>,
    session_id: Option<String>,
    created_at: u64,
    ended_at: Option<u64>,
    archived_at: Option<u64>,
    error: Option<MemoryConsolidationFailure>,
    usage: DreamUsage,
}

impl MemoryConsolidationJob {
    fn request(&self) -> MemoryConsolidationRequest {
        MemoryConsolidationRequest {
            job_id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            source_memory_store_id: self.source_memory_store_id.clone(),
            session_ids: self.session_ids.clone(),
            model: self.model.clone(),
            request_guidance: self.request_guidance.clone(),
            agent_selection: self.agent_selection.clone(),
        }
    }

    fn project(&self) -> Dream {
        Dream {
            id: self.id.clone(),
            kind: "dream",
            archived_at: self.archived_at.map(timestamp),
            created_at: timestamp(self.created_at),
            ended_at: self.ended_at.map(timestamp),
            error: self.error.as_ref().map(|error| DreamError {
                message: error.message.clone(),
                kind: error.kind.clone(),
            }),
            inputs: vec![
                DreamInput::MemoryStore {
                    memory_store_id: self.source_memory_store_id.clone(),
                },
                DreamInput::Sessions {
                    session_ids: self.session_ids.clone(),
                },
            ],
            instructions: self.request_guidance.clone(),
            model: self.model.clone(),
            outputs: self
                .result_memory_store_id
                .iter()
                .map(|id| DreamOutput {
                    memory_store_id: id.clone(),
                    kind: "memory_store",
                })
                .collect(),
            session_id: self.session_id.clone(),
            status: self.status.clone(),
            usage: self.usage.clone(),
        }
    }
}

fn timestamp(value: u64) -> String {
    crate::cron::to_rfc3339(value)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryConsolidationApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("Dream was not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
}

pub struct DreamState {
    jobs: Mutex<BTreeMap<String, MemoryConsolidationJob>>,
    cancellations: Mutex<BTreeMap<String, MemoryConsolidationCancellation>>,
    worker: Arc<dyn MemoryConsolidationWorker>,
    next_id: AtomicU64,
    durable: Option<Arc<dyn MemoryConsolidationRepository>>,
    workspace_agent_overrides: Mutex<BTreeMap<String, String>>,
}

impl DreamState {
    #[must_use]
    pub fn new(worker: Arc<dyn MemoryConsolidationWorker>) -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            cancellations: Mutex::new(BTreeMap::new()),
            worker,
            next_id: AtomicU64::new(1),
            durable: None,
            workspace_agent_overrides: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_repository(
        worker: Arc<dyn MemoryConsolidationWorker>,
        repository: Arc<dyn MemoryConsolidationRepository>,
    ) -> Result<Self, MemoryConsolidationApiError> {
        let jobs = repository
            .consolidation_jobs()
            .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| {
                serde_json::from_str::<MemoryConsolidationJob>(&record.data)
                    .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))
                    .and_then(|job| {
                        if job.id == record.job_id {
                            Ok((job.id.clone(), job))
                        } else {
                            Err(MemoryConsolidationApiError::Unavailable(
                                "Memory Consolidation job record identity mismatch".into(),
                            ))
                        }
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let next_id = jobs
            .keys()
            .filter_map(|id| id.strip_prefix("dream_")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let workspace_agent_overrides = repository
            .memory_consolidator_overrides()
            .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| (record.workspace_id, record.agent_id))
            .collect();
        let cancellations = jobs
            .iter()
            .filter(|(_, job)| matches!(job.status, DreamStatus::Pending | DreamStatus::Running))
            .map(|(id, _)| (id.clone(), MemoryConsolidationCancellation::default()))
            .collect();
        Ok(Self {
            jobs: Mutex::new(jobs),
            cancellations: Mutex::new(cancellations),
            worker,
            next_id: AtomicU64::new(next_id),
            durable: Some(repository),
            workspace_agent_overrides: Mutex::new(workspace_agent_overrides),
        })
    }

    /// Configure or clear the exact published Agent used for future
    /// consolidations in one Workspace. Absence selects the built-in effective
    /// default; no default Agent row is copied per Workspace. A job freezes the
    /// resolved id at create time, so later policy edits cannot alter retries.
    pub fn set_workspace_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), MemoryConsolidationApiError> {
        if workspace_id.trim().is_empty()
            || agent_id.is_some_and(|agent_id| agent_id.trim().is_empty())
        {
            return Err(MemoryConsolidationApiError::BadRequest(
                "Workspace and Agent ids must be non-empty".into(),
            ));
        }
        let mut overrides = self.workspace_agent_overrides.lock().unwrap();
        match agent_id {
            Some(agent_id) => {
                overrides.insert(workspace_id.into(), agent_id.into());
            }
            None => {
                overrides.remove(workspace_id);
            }
        }
        if let Some(repository) = &self.durable {
            repository
                .set_memory_consolidator_override(workspace_id, agent_id)
                .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))?;
        }
        Ok(())
    }

    /// Re-dispatch durable non-terminal jobs after a process restart. The worker
    /// reuses deterministic snapshot/result/session identities, so preparation is
    /// idempotent at the lower authorities.
    pub fn resume_incomplete(self: &Arc<Self>) {
        let resumable = self
            .jobs
            .lock()
            .unwrap()
            .values_mut()
            .filter(|job| matches!(job.status, DreamStatus::Pending | DreamStatus::Running))
            .map(|job| {
                job.status = DreamStatus::Pending;
                job.id.clone()
            })
            .collect::<Vec<_>>();
        for id in resumable {
            let cancellation = MemoryConsolidationCancellation::default();
            self.cancellations
                .lock()
                .unwrap()
                .insert(id.clone(), cancellation.clone());
            let state = self.clone();
            tokio::spawn(async move { state.run_job(id, cancellation).await });
        }
    }

    fn persist(&self, job: &MemoryConsolidationJob) -> Result<(), MemoryConsolidationApiError> {
        let Some(repository) = &self.durable else {
            return Ok(());
        };
        let data = serde_json::to_string(job)
            .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))?;
        repository
            .upsert_consolidation_job(MemoryConsolidationJobRecord {
                job_id: job.id.clone(),
                data,
            })
            .map_err(|error| MemoryConsolidationApiError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn create(
        self: &Arc<Self>,
        workspace_id: &str,
        params: DreamCreateParams,
    ) -> Result<Dream, MemoryConsolidationApiError> {
        let (source_memory_store_id, session_ids) = validate_create(&params)?;
        let id = format!("dream_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let job = MemoryConsolidationJob {
            id: id.clone(),
            workspace_id: workspace_id.to_string(),
            status: DreamStatus::Pending,
            source_memory_store_id,
            session_ids,
            model: params.model.into_config(),
            request_guidance: params.instructions,
            agent_selection: MemoryConsolidationAgentSelection {
                agent_id: self
                    .workspace_agent_overrides
                    .lock()
                    .unwrap()
                    .get(workspace_id)
                    .cloned()
                    .unwrap_or_else(default_memory_consolidator_agent_id),
            },
            result_memory_store_id: None,
            session_id: None,
            created_at: now_ms(),
            ended_at: None,
            archived_at: None,
            error: None,
            usage: DreamUsage::default(),
        };
        self.worker
            .validate_inputs(&job.request())
            .await
            .map_err(|error| MemoryConsolidationApiError::BadRequest(error.message))?;
        let projected = job.project();
        self.persist(&job)?;
        self.jobs.lock().unwrap().insert(id.clone(), job);
        let cancellation = MemoryConsolidationCancellation::default();
        self.cancellations
            .lock()
            .unwrap()
            .insert(id.clone(), cancellation.clone());
        let state = self.clone();
        tokio::spawn(async move {
            state.run_job(id, cancellation).await;
        });
        Ok(projected)
    }

    async fn run_job(&self, id: String, cancellation: MemoryConsolidationCancellation) {
        let (request, running_job) = {
            let mut jobs = self.jobs.lock().unwrap();
            let Some(job) = jobs.get_mut(&id) else { return };
            if job.status == DreamStatus::Canceled {
                return;
            }
            job.status = DreamStatus::Running;
            (job.request(), job.clone())
        };
        let _ = self.persist(&running_job);
        let preparation = match self.worker.prepare(&request).await {
            Ok(preparation) => preparation,
            Err(error) => {
                self.fail_if_active(&id, error);
                return;
            }
        };
        let (canceled_after_prepare, prepared_job) = {
            let mut jobs = self.jobs.lock().unwrap();
            let Some(job) = jobs.get_mut(&id) else { return };
            job.result_memory_store_id = Some(preparation.result_memory_store_id.clone());
            job.session_id = Some(preparation.session_id.clone());
            (
                job.status == DreamStatus::Canceled || cancellation.is_canceled(),
                job.clone(),
            )
        };
        let _ = self.persist(&prepared_job);
        if canceled_after_prepare {
            let _ = self.worker.cancel(Some(&preparation.session_id)).await;
            self.cancellations.lock().unwrap().remove(&id);
            return;
        }
        let result = self
            .worker
            .execute(&request, &preparation, cancellation.clone())
            .await;
        let terminal_job = {
            let mut jobs = self.jobs.lock().unwrap();
            let Some(job) = jobs.get_mut(&id) else { return };
            match result {
                Ok(usage) => {
                    job.usage = usage;
                    if job.status != DreamStatus::Canceled && !cancellation.is_canceled() {
                        job.status = DreamStatus::Completed;
                        job.ended_at = Some(now_ms());
                    }
                }
                Err(error) => {
                    if job.status != DreamStatus::Canceled && !cancellation.is_canceled() {
                        job.status = DreamStatus::Failed;
                        job.error = Some(error);
                        job.ended_at = Some(now_ms());
                    }
                }
            }
            job.clone()
        };
        let _ = self.persist(&terminal_job);
        self.cancellations.lock().unwrap().remove(&id);
    }

    fn fail_if_active(&self, id: &str, error: MemoryConsolidationFailure) {
        let failed = {
            let mut jobs = self.jobs.lock().unwrap();
            jobs.get_mut(id).and_then(|job| {
                if job.status == DreamStatus::Canceled {
                    None
                } else {
                    job.status = DreamStatus::Failed;
                    job.error = Some(error);
                    job.ended_at = Some(now_ms());
                    Some(job.clone())
                }
            })
        };
        if let Some(job) = failed {
            let _ = self.persist(&job);
        }
        self.cancellations.lock().unwrap().remove(id);
    }

    pub fn retrieve(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Dream, MemoryConsolidationApiError> {
        self.jobs
            .lock()
            .unwrap()
            .get(id)
            .filter(|job| job.workspace_id == workspace_id)
            .map(MemoryConsolidationJob::project)
            .ok_or(MemoryConsolidationApiError::NotFound)
    }

    pub fn list(
        &self,
        workspace_id: &str,
        params: DreamListParams,
    ) -> Result<DreamPage, MemoryConsolidationApiError> {
        let after = parse_bound(params.created_after.as_deref())?;
        let before = parse_bound(params.created_before.as_deref())?;
        let statuses = params.statuses.into_iter().collect::<BTreeSet<_>>();
        let mut jobs = self
            .jobs
            .lock()
            .unwrap()
            .values()
            .filter(|job| job.workspace_id == workspace_id)
            .filter(|job| params.include_archived || job.archived_at.is_none())
            .filter(|job| statuses.is_empty() || statuses.contains(&job.status))
            .filter(|job| after.is_none_or(|bound| job.created_at > bound))
            .filter(|job| before.is_none_or(|bound| job.created_at < bound))
            .cloned()
            .collect::<Vec<_>>();
        jobs.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        let start = match params.page {
            Some(cursor) => jobs
                .iter()
                .position(|job| job.id == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    MemoryConsolidationApiError::BadRequest("unknown page cursor".into())
                })?,
            None => 0,
        };
        let limit = params
            .limit
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_PAGE_SIZE);
        let selected = jobs.iter().skip(start).take(limit).collect::<Vec<_>>();
        let next_page = (start + selected.len() < jobs.len())
            .then(|| selected.last().map(|job| job.id.clone()))
            .flatten();
        Ok(DreamPage {
            data: selected.into_iter().map(|job| job.project()).collect(),
            next_page,
        })
    }

    pub async fn cancel(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Dream, MemoryConsolidationApiError> {
        let (session_id, canceled_job) = {
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs
                .get_mut(id)
                .filter(|job| job.workspace_id == workspace_id)
                .ok_or(MemoryConsolidationApiError::NotFound)?;
            match job.status {
                DreamStatus::Pending | DreamStatus::Running => {
                    job.status = DreamStatus::Canceled;
                    job.ended_at = Some(now_ms());
                }
                DreamStatus::Canceled => return Ok(job.project()),
                DreamStatus::Completed | DreamStatus::Failed => {
                    return Err(MemoryConsolidationApiError::BadRequest(
                        "only pending or running Dreams can be canceled".into(),
                    ));
                }
            }
            (job.session_id.clone(), job.clone())
        };
        self.persist(&canceled_job)?;
        if let Some(cancellation) = self.cancellations.lock().unwrap().get(id) {
            cancellation.cancel();
        }
        self.worker
            .cancel(session_id.as_deref())
            .await
            .map_err(|error| MemoryConsolidationApiError::Unavailable(error.message))?;
        self.retrieve(workspace_id, id)
    }

    pub fn archive(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Dream, MemoryConsolidationApiError> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id)
            .filter(|job| job.workspace_id == workspace_id)
            .ok_or(MemoryConsolidationApiError::NotFound)?;
        if !job.status.is_terminal() {
            return Err(MemoryConsolidationApiError::BadRequest(
                "only terminal Dreams can be archived".into(),
            ));
        }
        if job.archived_at.is_none() {
            job.archived_at = Some(now_ms());
        }
        let projected = job.project();
        let persisted = job.clone();
        drop(jobs);
        self.persist(&persisted)?;
        Ok(projected)
    }
}

fn parse_bound(value: Option<&str>) -> Result<Option<u64>, MemoryConsolidationApiError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc).timestamp_millis().max(0) as u64)
                .map_err(|_| {
                    MemoryConsolidationApiError::BadRequest("invalid RFC 3339 timestamp".into())
                })
        })
        .transpose()
}

fn validate_create(
    params: &DreamCreateParams,
) -> Result<(String, Vec<String>), MemoryConsolidationApiError> {
    if params.inputs.len() != 2 {
        return Err(MemoryConsolidationApiError::BadRequest(
            "inputs must contain exactly one memory_store and one sessions input".into(),
        ));
    }
    let model = match &params.model {
        crate::types::DreamModelInput::Id(id) => id,
        crate::types::DreamModelInput::Config(config) => &config.id,
    };
    if model.is_empty() || model.chars().count() > 256 {
        return Err(MemoryConsolidationApiError::BadRequest(
            "model id must contain 1 to 256 characters".into(),
        ));
    }
    if !SUPPORTED_MODELS.contains(&model.as_str()) {
        return Err(MemoryConsolidationApiError::BadRequest(format!(
            "unsupported Dream model `{model}`"
        )));
    }
    if params
        .instructions
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_INSTRUCTIONS_CHARS)
    {
        return Err(MemoryConsolidationApiError::BadRequest(format!(
            "instructions may contain at most {MAX_INSTRUCTIONS_CHARS} characters"
        )));
    }
    let mut memory = None;
    let mut sessions = None;
    for input in &params.inputs {
        match input {
            DreamInput::MemoryStore { memory_store_id }
                if memory.is_none() && !memory_store_id.trim().is_empty() =>
            {
                memory = Some(memory_store_id.clone());
            }
            DreamInput::Sessions { session_ids } if sessions.is_none() => {
                sessions = Some(session_ids.clone());
            }
            _ => {
                return Err(MemoryConsolidationApiError::BadRequest(
                    "inputs must contain exactly one non-empty memory_store and one sessions input"
                        .into(),
                ));
            }
        }
    }
    let sessions = sessions.ok_or_else(|| {
        MemoryConsolidationApiError::BadRequest("sessions input is required".into())
    })?;
    if sessions.is_empty() || sessions.len() > MAX_SESSIONS {
        return Err(MemoryConsolidationApiError::BadRequest(format!(
            "sessions input must contain 1 to {MAX_SESSIONS} session ids"
        )));
    }
    let unique = sessions.iter().collect::<BTreeSet<_>>();
    if unique.len() != sessions.len() || sessions.iter().any(|id| id.trim().is_empty()) {
        return Err(MemoryConsolidationApiError::BadRequest(
            "session ids must be non-empty and unique".into(),
        ));
    }
    Ok((memory.expect("validated memory input"), sessions))
}
