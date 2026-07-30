//! Dream application service behind the public Dreams adapter.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_ext_memory::{DreamJobRecord, DreamPolicyRecord, DreamRepository};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{
    Dream, DreamCreateParams, DreamError, DreamInput, DreamListParams, DreamModelConfig,
    DreamModelInput, DreamOutput, DreamPage, DreamStatus, DreamUsage,
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

pub const BUILT_IN_DREAM_AGENT_ID: &str = "awaken_builtin_dream_agent";

fn default_dream_agent_id() -> String {
    BUILT_IN_DREAM_AGENT_ID.into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamAgentSelection {
    pub agent_id: String,
}

fn default_dream_agent_selection() -> DreamAgentSelection {
    DreamAgentSelection {
        agent_id: default_dream_agent_id(),
    }
}

#[derive(Debug, Clone)]
pub struct DreamRequest {
    pub job_id: String,
    pub workspace_id: String,
    pub source_memory_store_id: String,
    pub session_ids: Vec<String>,
    pub model: DreamModelConfig,
    pub request_guidance: Option<String>,
    pub agent_selection: DreamAgentSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamPreparation {
    pub result_memory_store_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamPolicyConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub min_new_sessions: usize,
    pub max_sessions: usize,
    pub model: DreamModelConfig,
    pub instructions: Option<String>,
}

/// Awaken extension projection for the opt-in automatic policy of one
/// Workspace-owned MemoryStore. Absence projects the disabled effective default
/// without creating a durable row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DreamPolicy {
    #[serde(rename = "type")]
    pub object_type: &'static str,
    pub memory_store_id: String,
    #[serde(flatten)]
    pub config: DreamPolicyConfig,
    pub next_due_at: Option<String>,
    pub last_completed_cutoff_at: Option<String>,
}

impl Default for DreamPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 24 * 60 * 60,
            min_new_sessions: 5,
            max_sessions: MAX_SESSIONS,
            model: DreamModelConfig {
                id: "claude-sonnet-5".into(),
                speed: None,
            },
            instructions: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredDreamPolicy {
    workspace_id: String,
    memory_store_id: String,
    config: DreamPolicyConfig,
    next_due_ms: u64,
    last_completed_cutoff_ms: u64,
}

#[async_trait]
pub trait DreamSessionSource: Send + Sync {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Vec<String>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DreamFailure {
    pub kind: String,
    pub message: String,
}

impl DreamFailure {
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct DreamCancellation(Arc<AtomicBool>);

impl DreamCancellation {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_canceled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The one Dream execution seam. Production composes existing MemoryStore, Files, Session,
/// and Runtime authorities here; tests use a deterministic implementation.
#[async_trait]
pub trait DreamWorker: Send + Sync {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure>;

    async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure>;

    async fn execute(
        &self,
        request: &DreamRequest,
        preparation: &DreamPreparation,
        cancellation: DreamCancellation,
    ) -> Result<DreamUsage, DreamFailure>;

    async fn cancel(&self, _session_id: Option<&str>) -> Result<(), DreamFailure> {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DreamJob {
    id: String,
    workspace_id: String,
    status: DreamStatus,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    #[serde(default = "default_dream_agent_selection")]
    agent_selection: DreamAgentSelection,
    result_memory_store_id: Option<String>,
    session_id: Option<String>,
    created_at: u64,
    ended_at: Option<u64>,
    archived_at: Option<u64>,
    error: Option<DreamFailure>,
    usage: DreamUsage,
    #[serde(default)]
    policy_key: Option<(String, String)>,
}

impl DreamJob {
    fn request(&self) -> DreamRequest {
        DreamRequest {
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
pub enum DreamApiError {
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
    jobs: Mutex<BTreeMap<String, DreamJob>>,
    cancellations: Mutex<BTreeMap<String, DreamCancellation>>,
    worker: Arc<dyn DreamWorker>,
    next_id: AtomicU64,
    durable: Option<Arc<dyn DreamRepository>>,
    workspace_agent_overrides: Mutex<BTreeMap<String, String>>,
    policies: Mutex<BTreeMap<(String, String), StoredDreamPolicy>>,
    session_source: Mutex<Option<Arc<dyn DreamSessionSource>>>,
}

impl DreamState {
    #[must_use]
    pub fn new(worker: Arc<dyn DreamWorker>) -> Self {
        Self {
            jobs: Mutex::new(BTreeMap::new()),
            cancellations: Mutex::new(BTreeMap::new()),
            worker,
            next_id: AtomicU64::new(1),
            durable: None,
            workspace_agent_overrides: Mutex::new(BTreeMap::new()),
            policies: Mutex::new(BTreeMap::new()),
            session_source: Mutex::new(None),
        }
    }

    pub fn with_repository(
        worker: Arc<dyn DreamWorker>,
        repository: Arc<dyn DreamRepository>,
    ) -> Result<Self, DreamApiError> {
        let jobs = repository
            .dream_jobs()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| {
                serde_json::from_str::<DreamJob>(&record.data)
                    .map_err(|error| DreamApiError::Unavailable(error.to_string()))
                    .and_then(|job| {
                        if job.id == record.job_id {
                            Ok((job.id.clone(), job))
                        } else {
                            Err(DreamApiError::Unavailable(
                                "Dream job record identity mismatch".into(),
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
            .dream_agent_overrides()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| (record.workspace_id, record.agent_id))
            .collect();
        let policies = repository
            .dream_policies()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| {
                let policy: StoredDreamPolicy = serde_json::from_str(&record.data)
                    .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
                if policy.workspace_id != record.workspace_id
                    || policy.memory_store_id != record.memory_store_id
                {
                    return Err(DreamApiError::Unavailable(
                        "Dream policy identity mismatch".into(),
                    ));
                }
                Ok(((record.workspace_id, record.memory_store_id), policy))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let cancellations = jobs
            .iter()
            .filter(|(_, job)| matches!(job.status, DreamStatus::Pending | DreamStatus::Running))
            .map(|(id, _)| (id.clone(), DreamCancellation::default()))
            .collect();
        Ok(Self {
            jobs: Mutex::new(jobs),
            cancellations: Mutex::new(cancellations),
            worker,
            next_id: AtomicU64::new(next_id),
            durable: Some(repository),
            workspace_agent_overrides: Mutex::new(workspace_agent_overrides),
            policies: Mutex::new(policies),
            session_source: Mutex::new(None),
        })
    }

    /// Configure or clear the exact published Agent used for future
    /// Dreams in one Workspace. Absence selects the built-in effective
    /// default; no default Agent row is copied per Workspace. A job freezes the
    /// resolved id at create time, so later policy edits cannot alter retries.
    pub fn set_workspace_agent_override(
        &self,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<(), DreamApiError> {
        if workspace_id.trim().is_empty()
            || agent_id.is_some_and(|agent_id| agent_id.trim().is_empty())
        {
            return Err(DreamApiError::BadRequest(
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
                .set_dream_agent_override(workspace_id, agent_id)
                .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
        }
        Ok(())
    }

    pub fn bind_session_source(&self, source: Arc<dyn DreamSessionSource>) {
        *self.session_source.lock().unwrap() = Some(source);
    }

    /// Configure the opt-in automatic Dream policy for one Workspace-owned
    /// MemoryStore. The effective Dream Agent remains the same built-in/override
    /// selection used by manual Dreams; the policy creates no copied Agent.
    pub fn set_policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config: DreamPolicyConfig,
    ) -> Result<(), DreamApiError> {
        if workspace_id.trim().is_empty() || memory_store_id.trim().is_empty() {
            return Err(DreamApiError::BadRequest(
                "Workspace and MemoryStore ids must be non-empty".into(),
            ));
        }
        if config.interval_seconds < 60
            || config.min_new_sessions == 0
            || config.max_sessions == 0
            || config.max_sessions > MAX_SESSIONS
            || config.min_new_sessions > config.max_sessions
            || !SUPPORTED_MODELS.contains(&config.model.id.as_str())
            || config
                .instructions
                .as_ref()
                .is_some_and(|value| value.chars().count() > MAX_INSTRUCTIONS_CHARS)
        {
            return Err(DreamApiError::BadRequest(
                "invalid Dream policy interval, session bounds, model, or instructions".into(),
            ));
        }
        let key = (workspace_id.to_string(), memory_store_id.to_string());
        let current = self.policies.lock().unwrap().get(&key).cloned();
        let policy = StoredDreamPolicy {
            workspace_id: workspace_id.to_string(),
            memory_store_id: memory_store_id.to_string(),
            next_due_ms: current
                .as_ref()
                .map_or_else(now_ms, |value| value.next_due_ms),
            last_completed_cutoff_ms: current
                .as_ref()
                .map_or(0, |value| value.last_completed_cutoff_ms),
            config,
        };
        self.persist_policy(&policy)?;
        self.policies.lock().unwrap().insert(key, policy);
        Ok(())
    }

    fn persist_policy(&self, policy: &StoredDreamPolicy) -> Result<(), DreamApiError> {
        let Some(repository) = &self.durable else {
            return Ok(());
        };
        repository
            .upsert_dream_policy(DreamPolicyRecord {
                workspace_id: policy.workspace_id.clone(),
                memory_store_id: policy.memory_store_id.clone(),
                data: serde_json::to_string(policy)
                    .map_err(|error| DreamApiError::Unavailable(error.to_string()))?,
            })
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))
    }

    /// Return the configured policy or the disabled effective default. Reading a
    /// default never creates a per-Workspace row.
    pub fn policy(&self, workspace_id: &str, memory_store_id: &str) -> DreamPolicy {
        let stored = self
            .policies
            .lock()
            .unwrap()
            .get(&(workspace_id.to_string(), memory_store_id.to_string()))
            .cloned();
        match stored {
            Some(policy) => DreamPolicy {
                object_type: "dream_policy",
                memory_store_id: policy.memory_store_id,
                config: policy.config,
                next_due_at: Some(crate::cron::to_rfc3339(policy.next_due_ms)),
                last_completed_cutoff_at: (policy.last_completed_cutoff_ms > 0)
                    .then(|| crate::cron::to_rfc3339(policy.last_completed_cutoff_ms)),
            },
            None => DreamPolicy {
                object_type: "dream_policy",
                memory_store_id: memory_store_id.to_string(),
                config: DreamPolicyConfig::default(),
                next_due_at: None,
                last_completed_cutoff_at: None,
            },
        }
    }

    /// Evaluate every due policy once. This method is deterministic and public
    /// for the composition root's single Managed periodic driver. Every accepted
    /// trigger calls the ordinary `create` path.
    pub async fn tick_policies(self: &Arc<Self>, now: u64) -> Result<Vec<Dream>, DreamApiError> {
        let source = self.session_source.lock().unwrap().clone();
        let due = self
            .policies
            .lock()
            .unwrap()
            .values()
            .filter(|policy| policy.config.enabled && policy.next_due_ms <= now)
            .cloned()
            .collect::<Vec<_>>();
        if due.is_empty() {
            return Ok(Vec::new());
        }
        let source = source.ok_or_else(|| {
            DreamApiError::Unavailable("Dream policy Session source is not bound".into())
        })?;
        let mut created = Vec::new();
        for mut policy in due {
            let key = (policy.workspace_id.clone(), policy.memory_store_id.clone());
            let already_running = self
                .jobs
                .lock()
                .unwrap()
                .values()
                .any(|job| job.policy_key.as_ref() == Some(&key) && !job.status.is_terminal());
            policy.next_due_ms = now.saturating_add(policy.config.interval_seconds * 1_000);
            if already_running {
                self.persist_policy(&policy)?;
                self.policies.lock().unwrap().insert(key, policy);
                continue;
            }
            let sessions = source
                .eligible_sessions(
                    &policy.workspace_id,
                    policy.last_completed_cutoff_ms,
                    policy.config.max_sessions,
                )
                .await;
            self.persist_policy(&policy)?;
            self.policies
                .lock()
                .unwrap()
                .insert(key.clone(), policy.clone());
            if sessions.len() < policy.config.min_new_sessions {
                continue;
            }
            created.push(
                self.create_with_policy(
                    &policy.workspace_id,
                    DreamCreateParams {
                        inputs: vec![
                            DreamInput::MemoryStore {
                                memory_store_id: policy.memory_store_id,
                            },
                            DreamInput::Sessions {
                                session_ids: sessions,
                            },
                        ],
                        model: DreamModelInput::Config(policy.config.model),
                        instructions: policy.config.instructions,
                    },
                    Some(key),
                )
                .await?,
            );
        }
        Ok(created)
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
            let cancellation = DreamCancellation::default();
            self.cancellations
                .lock()
                .unwrap()
                .insert(id.clone(), cancellation.clone());
            let state = self.clone();
            tokio::spawn(async move { state.run_job(id, cancellation).await });
        }
    }

    fn persist(&self, job: &DreamJob) -> Result<(), DreamApiError> {
        let Some(repository) = &self.durable else {
            return Ok(());
        };
        let data = serde_json::to_string(job)
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
        repository
            .upsert_dream_job(DreamJobRecord {
                job_id: job.id.clone(),
                data,
            })
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
        Ok(())
    }

    pub async fn create(
        self: &Arc<Self>,
        workspace_id: &str,
        params: DreamCreateParams,
    ) -> Result<Dream, DreamApiError> {
        self.create_with_policy(workspace_id, params, None).await
    }

    async fn create_with_policy(
        self: &Arc<Self>,
        workspace_id: &str,
        params: DreamCreateParams,
        policy_key: Option<(String, String)>,
    ) -> Result<Dream, DreamApiError> {
        let (source_memory_store_id, session_ids) = validate_create(&params)?;
        let id = format!("dream_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let job = DreamJob {
            id: id.clone(),
            workspace_id: workspace_id.to_string(),
            status: DreamStatus::Pending,
            source_memory_store_id,
            session_ids,
            model: params.model.into_config(),
            request_guidance: params.instructions,
            agent_selection: DreamAgentSelection {
                agent_id: self
                    .workspace_agent_overrides
                    .lock()
                    .unwrap()
                    .get(workspace_id)
                    .cloned()
                    .unwrap_or_else(default_dream_agent_id),
            },
            result_memory_store_id: None,
            session_id: None,
            created_at: now_ms(),
            ended_at: None,
            archived_at: None,
            error: None,
            usage: DreamUsage::default(),
            policy_key,
        };
        self.worker
            .validate_inputs(&job.request())
            .await
            .map_err(|error| DreamApiError::BadRequest(error.message))?;
        let projected = job.project();
        self.persist(&job)?;
        self.jobs.lock().unwrap().insert(id.clone(), job);
        let cancellation = DreamCancellation::default();
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

    async fn run_job(&self, id: String, cancellation: DreamCancellation) {
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
        if terminal_job.status == DreamStatus::Completed
            && let Some(key) = &terminal_job.policy_key
        {
            let updated = {
                let mut policies = self.policies.lock().unwrap();
                policies.get_mut(key).map(|policy| {
                    policy.last_completed_cutoff_ms =
                        policy.last_completed_cutoff_ms.max(terminal_job.created_at);
                    policy.clone()
                })
            };
            if let Some(policy) = updated {
                let _ = self.persist_policy(&policy);
            }
        }
        self.cancellations.lock().unwrap().remove(&id);
    }

    fn fail_if_active(&self, id: &str, error: DreamFailure) {
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

    pub fn retrieve(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        self.jobs
            .lock()
            .unwrap()
            .get(id)
            .filter(|job| job.workspace_id == workspace_id)
            .map(DreamJob::project)
            .ok_or(DreamApiError::NotFound)
    }

    pub fn list(
        &self,
        workspace_id: &str,
        params: DreamListParams,
    ) -> Result<DreamPage, DreamApiError> {
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
                .ok_or_else(|| DreamApiError::BadRequest("unknown page cursor".into()))?,
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

    pub async fn cancel(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        let (session_id, canceled_job) = {
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs
                .get_mut(id)
                .filter(|job| job.workspace_id == workspace_id)
                .ok_or(DreamApiError::NotFound)?;
            match job.status {
                DreamStatus::Pending | DreamStatus::Running => {
                    job.status = DreamStatus::Canceled;
                    job.ended_at = Some(now_ms());
                }
                DreamStatus::Canceled => return Ok(job.project()),
                DreamStatus::Completed | DreamStatus::Failed => {
                    return Err(DreamApiError::BadRequest(
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
            .map_err(|error| DreamApiError::Unavailable(error.message))?;
        self.retrieve(workspace_id, id)
    }

    pub fn archive(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        let mut jobs = self.jobs.lock().unwrap();
        let job = jobs
            .get_mut(id)
            .filter(|job| job.workspace_id == workspace_id)
            .ok_or(DreamApiError::NotFound)?;
        if !job.status.is_terminal() {
            return Err(DreamApiError::BadRequest(
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

fn parse_bound(value: Option<&str>) -> Result<Option<u64>, DreamApiError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc).timestamp_millis().max(0) as u64)
                .map_err(|_| DreamApiError::BadRequest("invalid RFC 3339 timestamp".into()))
        })
        .transpose()
}

fn validate_create(params: &DreamCreateParams) -> Result<(String, Vec<String>), DreamApiError> {
    if params.inputs.len() != 2 {
        return Err(DreamApiError::BadRequest(
            "inputs must contain exactly one memory_store and one sessions input".into(),
        ));
    }
    let model = match &params.model {
        crate::types::DreamModelInput::Id(id) => id,
        crate::types::DreamModelInput::Config(config) => &config.id,
    };
    if model.is_empty() || model.chars().count() > 256 {
        return Err(DreamApiError::BadRequest(
            "model id must contain 1 to 256 characters".into(),
        ));
    }
    if !SUPPORTED_MODELS.contains(&model.as_str()) {
        return Err(DreamApiError::BadRequest(format!(
            "unsupported Dream model `{model}`"
        )));
    }
    if params
        .instructions
        .as_ref()
        .is_some_and(|value| value.chars().count() > MAX_INSTRUCTIONS_CHARS)
    {
        return Err(DreamApiError::BadRequest(format!(
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
                return Err(DreamApiError::BadRequest(
                    "inputs must contain exactly one non-empty memory_store and one sessions input"
                        .into(),
                ));
            }
        }
    }
    let sessions =
        sessions.ok_or_else(|| DreamApiError::BadRequest("sessions input is required".into()))?;
    if sessions.is_empty() || sessions.len() > MAX_SESSIONS {
        return Err(DreamApiError::BadRequest(format!(
            "sessions input must contain 1 to {MAX_SESSIONS} session ids"
        )));
    }
    let unique = sessions.iter().collect::<BTreeSet<_>>();
    if unique.len() != sessions.len() || sessions.iter().any(|id| id.trim().is_empty()) {
        return Err(DreamApiError::BadRequest(
            "session ids must be non-empty and unique".into(),
        ));
    }
    Ok((memory.expect("validated memory input"), sessions))
}

#[async_trait]
impl DreamSessionSource for crate::ManagedState {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Vec<String> {
        let mut sessions = self
            .list_sessions_scoped(workspace_id)
            .into_iter()
            .filter(|session| session.status != "running")
            .filter(|session| {
                session
                    .metadata
                    .get("awaken.session.origin")
                    .is_none_or(|origin| origin != "dream")
            })
            .filter_map(|session| {
                let updated = DateTime::parse_from_rfc3339(&session.updated_at)
                    .ok()?
                    .timestamp_millis()
                    .max(0) as u64;
                (updated > updated_after_ms).then_some((updated, session.id))
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| left.cmp(right));
        sessions.into_iter().take(limit).map(|(_, id)| id).collect()
    }
}
