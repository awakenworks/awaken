//! Coordinator-owned Dream process application.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_session_contract::{
    DREAM_MAX_INSTRUCTIONS_CHARS, DREAM_MAX_SESSIONS, DREAM_SUPPORTED_MODELS, Dream,
    DreamCreateParams, DreamError, DreamInput, DreamListParams, DreamModelConfig, DreamModelInput,
    DreamModelSpeed, DreamOutput, DreamPage, DreamPolicyApplication, DreamPolicyApplicationError,
    DreamPolicyRecord, DreamProcessFailure, DreamProcessRecord, DreamProcessStore, DreamStatus,
    DreamStatusEvent, DreamUsage,
};
pub use awaken_session_contract::{DreamPolicy, DreamPolicyConfig};
use chrono::{DateTime, Utc};

const DEFAULT_PAGE_SIZE: usize = 20;
const MAX_PAGE_SIZE: usize = 100;

pub const BUILT_IN_DREAM_AGENT_ID: &str = "awaken_builtin_dream_agent";

#[derive(Debug, Clone)]
pub struct DreamRequest {
    pub job_id: String,
    pub workspace_id: String,
    pub source_memory_store_id: String,
    pub session_ids: Vec<String>,
    pub model: DreamModelConfig,
    pub request_guidance: Option<String>,
    /// Ordinary published Agent id used by the auxiliary Session. The stable
    /// built-in id is configured through the normal Agent authoring surface;
    /// Dream owns no second Agent-selection policy.
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamPreparation {
    pub result_memory_store_id: String,
    pub session_id: String,
    /// Generated Files backing the frozen JSONL transcript inputs. They are
    /// implementation artifacts, not Dream outputs, and are deleted by the
    /// worker cleanup lifecycle.
    pub transcript_file_ids: Vec<String>,
}

type StoredDreamPolicy = DreamPolicyRecord;

#[async_trait]
pub trait DreamSessionSource: Send + Sync {
    async fn eligible_sessions(
        &self,
        workspace_id: &str,
        updated_after_ms: u64,
        limit: usize,
    ) -> Vec<String>;

    /// Read the ordinary auxiliary Session's cumulative committed usage. Dream
    /// never persists a second copy of these execution facts.
    async fn session_usage(&self, _workspace_id: &str, _session_id: &str) -> Option<DreamUsage> {
        None
    }
}

/// Workspace-aware readiness seam used before a Dream is persisted. Supported
/// model ids are a protocol contract; executable readiness is deployment state.
#[async_trait]
pub trait DreamModelReadiness: Send + Sync {
    async fn is_ready(&self, workspace_id: &str, model_id: &str) -> Result<bool, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// The one Dream execution seam. Production calls existing MemoryStore, Files, Session,
/// and Runtime authorities here; tests use a deterministic implementation.
#[async_trait]
pub trait DreamExecutor: Send + Sync {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure>;

    async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure>;

    async fn execute(
        &self,
        request: &DreamRequest,
        preparation: &DreamPreparation,
        cancellation: DreamCancellation,
    ) -> Result<(), DreamFailure>;

    async fn cleanup(
        &self,
        _request: &DreamRequest,
        _preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        Ok(())
    }

    async fn cancel(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        self.cleanup(request, preparation).await
    }
}

#[derive(Debug, Clone)]
struct DreamProcess {
    id: String,
    workspace_id: String,
    status: DreamStatus,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    agent_id: String,
    result_memory_store_id: Option<String>,
    session_id: Option<String>,
    transcript_file_ids: Vec<String>,
    cleanup_pending: bool,
    created_at: u64,
    ended_at: Option<u64>,
    archived_at: Option<u64>,
    error: Option<DreamFailure>,
    policy_key: Option<(String, String)>,
}

impl DreamProcess {
    fn request(&self) -> DreamRequest {
        DreamRequest {
            job_id: self.id.clone(),
            workspace_id: self.workspace_id.clone(),
            source_memory_store_id: self.source_memory_store_id.clone(),
            session_ids: self.session_ids.clone(),
            model: self.model.clone(),
            request_guidance: self.request_guidance.clone(),
            agent_id: self.agent_id.clone(),
        }
    }

    fn project(&self, usage: DreamUsage) -> Dream {
        // The durable terminal decision is written before resource cleanup so a
        // crash can resume cleanup. Do not publish that terminal state until the
        // output store is released and transient transcript Files are removed.
        let public_terminal_pending = self.cleanup_pending
            && matches!(self.status, DreamStatus::Completed | DreamStatus::Failed);
        Dream {
            id: self.id.clone(),
            kind: "dream",
            archived_at: self.archived_at.map(timestamp),
            created_at: timestamp(self.created_at),
            ended_at: if public_terminal_pending {
                None
            } else {
                self.ended_at.map(timestamp)
            },
            error: if public_terminal_pending {
                None
            } else {
                self.error.as_ref().map(|error| DreamError {
                    message: error.message.clone(),
                    kind: error.kind.clone(),
                })
            },
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
            status: if public_terminal_pending {
                DreamStatus::Running
            } else {
                self.status.clone()
            },
            usage,
        }
    }
}

fn process_record(process: &DreamProcess) -> DreamProcessRecord {
    DreamProcessRecord {
        process_id: process.id.clone(),
        workspace_id: process.workspace_id.clone(),
        status: process.status.clone(),
        source_memory_store_id: process.source_memory_store_id.clone(),
        session_ids: process.session_ids.clone(),
        model: process.model.clone(),
        request_guidance: process.request_guidance.clone(),
        agent_id: process.agent_id.clone(),
        result_memory_store_id: process.result_memory_store_id.clone(),
        session_id: process.session_id.clone(),
        transcript_file_ids: process.transcript_file_ids.clone(),
        cleanup_pending: process.cleanup_pending,
        created_at: process.created_at,
        ended_at: process.ended_at,
        archived_at: process.archived_at,
        error: process.error.as_ref().map(|error| DreamProcessFailure {
            kind: error.kind.clone(),
            message: error.message.clone(),
        }),
        policy_key: process.policy_key.clone(),
    }
}

fn process_from_record(record: DreamProcessRecord) -> DreamProcess {
    DreamProcess {
        id: record.process_id,
        workspace_id: record.workspace_id,
        status: record.status,
        source_memory_store_id: record.source_memory_store_id,
        session_ids: record.session_ids,
        model: record.model,
        request_guidance: record.request_guidance,
        agent_id: record.agent_id,
        result_memory_store_id: record.result_memory_store_id,
        session_id: record.session_id,
        transcript_file_ids: record.transcript_file_ids,
        cleanup_pending: record.cleanup_pending,
        created_at: record.created_at,
        ended_at: record.ended_at,
        archived_at: record.archived_at,
        error: record.error.map(|error| DreamFailure {
            kind: error.kind,
            message: error.message,
        }),
        policy_key: record.policy_key,
    }
}

fn timestamp(value: u64) -> String {
    awaken_session_contract::epoch_millis_to_rfc3339(value)
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

#[derive(Default)]
#[cfg(any(test, feature = "test-support"))]
struct InMemoryDreamRecords {
    processes: BTreeMap<String, DreamProcessRecord>,
    policies: BTreeMap<(String, String), DreamPolicyRecord>,
}

#[derive(Default)]
#[cfg(any(test, feature = "test-support"))]
pub struct InMemoryDreamProcessStore(Mutex<InMemoryDreamRecords>);

#[cfg(any(test, feature = "test-support"))]
impl DreamProcessStore for InMemoryDreamProcessStore {
    fn dream_processes(
        &self,
    ) -> Result<Vec<DreamProcessRecord>, awaken_session_contract::DreamProcessStoreError> {
        Ok(self.0.lock().unwrap().processes.values().cloned().collect())
    }

    fn compare_and_swap_dream_process(
        &self,
        expected: Option<&DreamProcessRecord>,
        record: DreamProcessRecord,
    ) -> Result<bool, awaken_session_contract::DreamProcessStoreError> {
        let mut state = self.0.lock().unwrap();
        let current = state.processes.get(&record.process_id);
        if current != expected {
            return Ok(false);
        }
        state.processes.insert(record.process_id.clone(), record);
        Ok(true)
    }

    fn dream_policies(
        &self,
    ) -> Result<Vec<DreamPolicyRecord>, awaken_session_contract::DreamProcessStoreError> {
        Ok(self.0.lock().unwrap().policies.values().cloned().collect())
    }

    fn compare_and_swap_dream_policy(
        &self,
        expected: Option<&DreamPolicyRecord>,
        record: DreamPolicyRecord,
    ) -> Result<bool, awaken_session_contract::DreamProcessStoreError> {
        let mut state = self.0.lock().unwrap();
        let key = (record.workspace_id.clone(), record.memory_store_id.clone());
        let current = state.policies.get(&key);
        if current != expected {
            return Ok(false);
        }
        state.policies.insert(key, record);
        Ok(true)
    }

    fn claim_dream_policy(
        &self,
        expected_policy: &DreamPolicyRecord,
        policy: DreamPolicyRecord,
        process: DreamProcessRecord,
    ) -> Result<bool, awaken_session_contract::DreamProcessStoreError> {
        let mut state = self.0.lock().unwrap();
        let key = (policy.workspace_id.clone(), policy.memory_store_id.clone());
        if state.policies.get(&key) != Some(expected_policy)
            || state.processes.contains_key(&process.process_id)
        {
            return Ok(false);
        }
        state.policies.insert(key, policy);
        state.processes.insert(process.process_id.clone(), process);
        Ok(true)
    }
}

pub struct DreamApplication {
    cancellations: Mutex<BTreeMap<String, DreamCancellation>>,
    executor: Arc<dyn DreamExecutor>,
    next_id: AtomicU64,
    store: Arc<dyn DreamProcessStore>,
    session_source: Mutex<Option<Arc<dyn DreamSessionSource>>>,
    model_readiness: Mutex<Option<Arc<dyn DreamModelReadiness>>>,
}

impl DreamApplication {
    pub fn with_store(
        executor: Arc<dyn DreamExecutor>,
        store: Arc<dyn DreamProcessStore>,
    ) -> Result<Self, DreamApiError> {
        let processes = store
            .dream_processes()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(|record| {
                let process = process_from_record(record);
                (process.id.clone(), process)
            })
            .collect::<BTreeMap<_, _>>();
        let next_id = processes
            .keys()
            .filter_map(|id| id.strip_prefix("dream_")?.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let cancellations = processes
            .iter()
            .filter(|(_, job)| matches!(job.status, DreamStatus::Pending | DreamStatus::Running))
            .map(|(id, _)| (id.clone(), DreamCancellation::default()))
            .collect();
        Ok(Self {
            cancellations: Mutex::new(cancellations),
            executor,
            next_id: AtomicU64::new(next_id),
            store,
            session_source: Mutex::new(None),
            model_readiness: Mutex::new(None),
        })
    }

    fn load_processes(&self) -> Result<Vec<DreamProcess>, DreamApiError> {
        Ok(self
            .store
            .dream_processes()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
            .into_iter()
            .map(process_from_record)
            .collect())
    }

    fn load_process(&self, id: &str) -> Result<DreamProcess, DreamApiError> {
        self.load_processes()?
            .into_iter()
            .find(|process| process.id == id)
            .ok_or(DreamApiError::NotFound)
    }

    fn load_policies(&self) -> Result<Vec<StoredDreamPolicy>, DreamApiError> {
        self.store
            .dream_policies()
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))
    }

    fn load_policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<Option<StoredDreamPolicy>, DreamApiError> {
        Ok(self.load_policies()?.into_iter().find(|policy| {
            policy.workspace_id == workspace_id && policy.memory_store_id == memory_store_id
        }))
    }

    pub fn bind_session_source(&self, source: Arc<dyn DreamSessionSource>) {
        *self.session_source.lock().unwrap() = Some(source);
    }

    pub fn bind_model_readiness(&self, source: Arc<dyn DreamModelReadiness>) {
        *self.model_readiness.lock().unwrap() = Some(source);
    }

    async fn project_process(&self, process: &DreamProcess) -> Dream {
        let source = self.session_source.lock().unwrap().clone();
        let usage = match (source, process.session_id.as_deref()) {
            (Some(source), Some(session_id)) => source
                .session_usage(&process.workspace_id, session_id)
                .await
                .unwrap_or_default(),
            _ => DreamUsage::default(),
        };
        process.project(usage)
    }

    /// Configure the opt-in automatic Dream policy for one Workspace-owned
    /// MemoryStore. Manual and scheduled Dreams use the same stable ordinary
    /// Agent id; the policy creates no copied Agent or selection state.
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
            || config.max_sessions > DREAM_MAX_SESSIONS
            || config.min_new_sessions > config.max_sessions
            || !DREAM_SUPPORTED_MODELS.contains(&config.model.id.as_str())
            || config.model.speed == Some(DreamModelSpeed::Fast)
            || config
                .instructions
                .as_ref()
                .is_some_and(|value| value.chars().count() > DREAM_MAX_INSTRUCTIONS_CHARS)
        {
            return Err(DreamApiError::BadRequest(
                "invalid Dream policy interval, session bounds, model, or instructions".into(),
            ));
        }
        let current = self.load_policy(workspace_id, memory_store_id)?;
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
        self.persist_policy(current.as_ref(), &policy)?;
        Ok(())
    }

    fn persist_policy(
        &self,
        expected: Option<&StoredDreamPolicy>,
        policy: &StoredDreamPolicy,
    ) -> Result<(), DreamApiError> {
        let changed = self
            .store
            .compare_and_swap_dream_policy(expected, policy.clone())
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
        if changed {
            Ok(())
        } else {
            Err(DreamApiError::Conflict(
                "Dream policy changed concurrently".into(),
            ))
        }
    }

    /// Return the configured policy or the disabled effective default. Reading a
    /// default never creates a per-Workspace row.
    pub fn policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<DreamPolicy, DreamApiError> {
        Ok(match self.load_policy(workspace_id, memory_store_id)? {
            Some(policy) => DreamPolicy {
                object_type: "dream_policy",
                memory_store_id: policy.memory_store_id,
                config: policy.config,
                next_due_at: Some(awaken_session_contract::epoch_millis_to_rfc3339(
                    policy.next_due_ms,
                )),
                last_completed_cutoff_at: (policy.last_completed_cutoff_ms > 0).then(|| {
                    awaken_session_contract::epoch_millis_to_rfc3339(
                        policy.last_completed_cutoff_ms,
                    )
                }),
            },
            None => DreamPolicy {
                object_type: "dream_policy",
                memory_store_id: memory_store_id.to_string(),
                config: DreamPolicyConfig::default(),
                next_due_at: None,
                last_completed_cutoff_at: None,
            },
        })
    }

    /// Evaluate every due policy once. This method is deterministic and public
    /// for the process startup's single Managed periodic driver. Every accepted
    /// trigger calls the ordinary `create` path.
    pub async fn tick_policies(self: &Arc<Self>, now: u64) -> Result<Vec<Dream>, DreamApiError> {
        let source = self.session_source.lock().unwrap().clone();
        let due = self
            .load_policies()?
            .into_iter()
            .filter(|policy| policy.config.enabled && policy.next_due_ms <= now)
            .collect::<Vec<_>>();
        if due.is_empty() {
            return Ok(Vec::new());
        }
        let source = source.ok_or_else(|| {
            DreamApiError::Unavailable("Dream policy Session source is not bound".into())
        })?;
        let mut created = Vec::new();
        for mut policy in due {
            let previous = policy.clone();
            let key = (policy.workspace_id.clone(), policy.memory_store_id.clone());
            let already_running = self
                .load_processes()?
                .into_iter()
                .any(|job| job.policy_key.as_ref() == Some(&key) && !job.status.is_terminal());
            policy.next_due_ms = now.saturating_add(policy.config.interval_seconds * 1_000);
            if already_running {
                self.persist_policy(Some(&previous), &policy)?;
                continue;
            }
            let workspace_id = policy.workspace_id.clone();
            let sessions = source
                .eligible_sessions(
                    &workspace_id,
                    policy.last_completed_cutoff_ms,
                    policy.config.max_sessions,
                )
                .await;
            if sessions.len() < policy.config.min_new_sessions {
                self.persist_policy(Some(&previous), &policy)?;
                continue;
            }
            match self
                .create_with_policy(
                    &workspace_id,
                    DreamCreateParams {
                        inputs: vec![
                            DreamInput::MemoryStore {
                                memory_store_id: policy.memory_store_id.clone(),
                            },
                            DreamInput::Sessions {
                                session_ids: sessions,
                            },
                        ],
                        model: DreamModelInput::Config(policy.config.model.clone()),
                        instructions: policy.config.instructions.clone(),
                    },
                    Some((key, previous, policy)),
                )
                .await
            {
                Ok(dream) => created.push(dream),
                Err(DreamApiError::Conflict(_)) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(created)
    }

    /// Re-dispatch durable non-terminal jobs after a process restart. The worker
    /// reuses deterministic snapshot/result/session identities, so preparation is
    /// idempotent at the lower authorities.
    pub fn resume_incomplete(self: &Arc<Self>) {
        let processes = match self.load_processes() {
            Ok(processes) => processes,
            Err(error) => {
                tracing::warn!(%error, "Dream recovery could not read process authority");
                return;
            }
        }
        .into_iter()
        .filter(|job| {
            matches!(job.status, DreamStatus::Pending | DreamStatus::Running)
                || (job.status.is_terminal() && job.cleanup_pending)
        })
        .map(|job| (job.id.clone(), job.status.is_terminal()))
        .collect::<Vec<_>>();
        for (id, terminal) in processes {
            if terminal {
                let state = self.clone();
                tokio::spawn(async move { state.cleanup_job(&id).await });
                continue;
            }
            if let Err(error) = self.commit_job_update(&id, |job| {
                job.status = job
                    .status
                    .transition(DreamStatusEvent::Recover)
                    .ok_or_else(|| {
                        DreamApiError::Conflict("terminal Dream cannot be recovered".into())
                    })?;
                Ok(())
            }) {
                tracing::warn!(dream_id = %id, %error, "Dream resume transition did not commit");
                continue;
            }
            let cancellation = DreamCancellation::default();
            self.cancellations
                .lock()
                .unwrap()
                .insert(id.clone(), cancellation.clone());
            let state = self.clone();
            tokio::spawn(async move { state.run_job(id, cancellation).await });
        }
    }

    fn commit_job_update(
        &self,
        id: &str,
        update: impl FnOnce(&mut DreamProcess) -> Result<(), DreamApiError>,
    ) -> Result<DreamProcess, DreamApiError> {
        let current = self.load_process(id)?;
        let mut candidate = current.clone();
        update(&mut candidate)?;
        let expected = process_record(&current);
        let changed = self
            .store
            .compare_and_swap_dream_process(Some(&expected), process_record(&candidate))
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?;
        if !changed {
            return Err(DreamApiError::Conflict("Dream changed concurrently".into()));
        }
        Ok(candidate)
    }

    fn insert_process(&self, process: DreamProcess) -> Result<bool, DreamApiError> {
        if !self
            .store
            .compare_and_swap_dream_process(None, process_record(&process))
            .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
        {
            return Ok(false);
        }
        Ok(true)
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
        policy_claim: Option<((String, String), StoredDreamPolicy, StoredDreamPolicy)>,
    ) -> Result<Dream, DreamApiError> {
        let (source_memory_store_id, session_ids) = validate_create(&params)?;
        let model = params.model.into_config();
        let readiness = self.model_readiness.lock().unwrap().clone();
        if let Some(readiness) = readiness {
            match readiness.is_ready(workspace_id, &model.id).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(DreamApiError::BadRequest(format!(
                        "Dream model `{}` is not connected or executable in this Workspace",
                        model.id
                    )));
                }
                Err(error) => return Err(DreamApiError::Unavailable(error)),
            }
        }
        let request_guidance = params.instructions;
        let mut job = DreamProcess {
            id: String::new(),
            workspace_id: workspace_id.to_string(),
            status: DreamStatus::Pending,
            source_memory_store_id,
            session_ids,
            model,
            request_guidance,
            agent_id: BUILT_IN_DREAM_AGENT_ID.to_string(),
            result_memory_store_id: None,
            session_id: None,
            transcript_file_ids: Vec::new(),
            cleanup_pending: false,
            created_at: now_ms(),
            ended_at: None,
            archived_at: None,
            error: None,
            policy_key: policy_claim.as_ref().map(|(key, _, _)| key.clone()),
        };
        job.id = format!("dream_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        self.executor
            .validate_inputs(&job.request())
            .await
            .map_err(|error| DreamApiError::BadRequest(error.message))?;
        if let Some((key, previous, policy)) = policy_claim {
            let claimed = loop {
                if self
                    .store
                    .claim_dream_policy(&previous, policy.clone(), process_record(&job))
                    .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
                {
                    break true;
                }
                let policy_was_claimed = self
                    .store
                    .dream_policies()
                    .map_err(|error| DreamApiError::Unavailable(error.to_string()))?
                    .into_iter()
                    .find(|record| record.workspace_id == key.0 && record.memory_store_id == key.1)
                    .is_none_or(|record| record != previous);
                if policy_was_claimed {
                    break false;
                }
                // The exact policy is unchanged, so the transaction lost only
                // the process-local numeric id. Allocate another and retry.
                job.id = format!("dream_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
            };
            if !claimed {
                return Err(DreamApiError::Conflict(
                    "Dream policy was claimed concurrently".into(),
                ));
            }
        } else {
            while !self.insert_process(job.clone())? {
                job.id = format!("dream_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
            }
        }
        let id = job.id.clone();
        let projected = self.project_process(&job).await;
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
        let running_job = match self.commit_job_update(&id, |job| {
            job.status = job
                .status
                .transition(DreamStatusEvent::Start)
                .ok_or_else(|| DreamApiError::Conflict("only a pending Dream can start".into()))?;
            Ok(())
        }) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(dream_id = %id, %error, "Dream running transition did not commit");
                return;
            }
        };
        let request = running_job.request();
        let preparation = match self.executor.prepare(&request).await {
            Ok(preparation) => preparation,
            Err(error) => {
                let cleanup_failed = self.executor.cleanup(&request, None).await.is_err();
                if let Err(persist_error) = self.fail_if_active(&id, error) {
                    tracing::warn!(dream_id = %id, %persist_error, "Dream failure transition did not commit");
                }
                if cleanup_failed {
                    let _ = self.commit_job_update(&id, |job| {
                        job.cleanup_pending = true;
                        Ok(())
                    });
                }
                return;
            }
        };
        let prepared_job = match self.commit_job_update(&id, |job| {
            job.result_memory_store_id = Some(preparation.result_memory_store_id.clone());
            job.session_id = Some(preparation.session_id.clone());
            job.transcript_file_ids = preparation.transcript_file_ids.clone();
            Ok(())
        }) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(dream_id = %id, %error, "Dream preparation transition did not commit");
                let _ = self.executor.cleanup(&request, Some(&preparation)).await;
                return;
            }
        };
        let canceled_after_prepare =
            prepared_job.status == DreamStatus::Canceled || cancellation.is_canceled();
        if canceled_after_prepare {
            let _ = self.executor.cancel(&request, Some(&preparation)).await;
            self.cancellations.lock().unwrap().remove(&id);
            return;
        }
        let result = match self
            .executor
            .execute(&request, &preparation, cancellation.clone())
            .await
        {
            Ok(()) => self.executor.validate_inputs(&request).await,
            Err(error) => Err(error),
        };
        let terminal_job = match self.commit_job_update(&id, |job| {
            match &result {
                Ok(()) => {
                    if job.status != DreamStatus::Canceled && !cancellation.is_canceled() {
                        job.status = job
                            .status
                            .transition(DreamStatusEvent::Complete)
                            .ok_or_else(|| {
                                DreamApiError::Conflict("only a running Dream can complete".into())
                            })?;
                        job.ended_at = Some(now_ms());
                    }
                }
                Err(error) => {
                    if job.status != DreamStatus::Canceled && !cancellation.is_canceled() {
                        job.status =
                            job.status
                                .transition(DreamStatusEvent::Fail)
                                .ok_or_else(|| {
                                    DreamApiError::Conflict("only a running Dream can fail".into())
                                })?;
                        job.error = Some(error.clone());
                        job.ended_at = Some(now_ms());
                    }
                }
            }
            job.cleanup_pending = true;
            Ok(())
        }) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(dream_id = %id, %error, "Dream terminal transition did not commit");
                return;
            }
        };
        if terminal_job.status == DreamStatus::Completed
            && let Some(key) = &terminal_job.policy_key
        {
            let previous = self.load_policy(&key.0, &key.1).ok().flatten();
            if let Some(mut policy) = previous.clone() {
                policy.last_completed_cutoff_ms =
                    policy.last_completed_cutoff_ms.max(terminal_job.created_at);
                if let Err(error) = self.persist_policy(previous.as_ref(), &policy) {
                    tracing::warn!(dream_id = %id, %error, "Dream policy cutoff did not commit");
                }
            }
        }
        self.cleanup_job(&id).await;
        self.cancellations.lock().unwrap().remove(&id);
    }

    async fn cleanup_job(&self, id: &str) {
        let Ok(job) = self.load_process(id) else {
            return;
        };
        let preparation = match (&job.result_memory_store_id, &job.session_id) {
            (Some(result_memory_store_id), Some(session_id)) => Some(DreamPreparation {
                result_memory_store_id: result_memory_store_id.clone(),
                session_id: session_id.clone(),
                transcript_file_ids: job.transcript_file_ids.clone(),
            }),
            _ => None,
        };
        if let Err(error) = self
            .executor
            .cleanup(&job.request(), preparation.as_ref())
            .await
        {
            tracing::warn!(dream_id = %id, message = %error.message, "Dream resource cleanup remains pending");
            return;
        }
        if let Err(error) = self.commit_job_update(id, |job| {
            job.cleanup_pending = false;
            job.transcript_file_ids.clear();
            Ok(())
        }) {
            tracing::warn!(dream_id = %id, %error, "Dream cleanup transition did not commit");
        }
    }

    fn fail_if_active(&self, id: &str, error: DreamFailure) -> Result<(), DreamApiError> {
        self.commit_job_update(id, |job| {
            if let Some(failed) = job.status.transition(DreamStatusEvent::Fail) {
                job.status = failed;
                job.error = Some(error);
                job.ended_at = Some(now_ms());
            }
            Ok(())
        })?;
        self.cancellations.lock().unwrap().remove(id);
        Ok(())
    }

    pub async fn retrieve(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        let process = self.load_process(id)?;
        if process.workspace_id != workspace_id {
            return Err(DreamApiError::NotFound);
        }
        Ok(self.project_process(&process).await)
    }

    pub async fn list(
        &self,
        workspace_id: &str,
        params: DreamListParams,
    ) -> Result<DreamPage, DreamApiError> {
        let after = parse_bound(params.created_after.as_deref())?;
        let before = parse_bound(params.created_before.as_deref())?;
        let statuses = params.statuses.into_iter().collect::<BTreeSet<_>>();
        let mut jobs = self
            .load_processes()?
            .into_iter()
            .filter(|job| job.workspace_id == workspace_id)
            .filter(|job| params.include_archived || job.archived_at.is_none())
            .filter(|job| statuses.is_empty() || statuses.contains(&job.status))
            .filter(|job| after.is_none_or(|bound| job.created_at > bound))
            .filter(|job| before.is_none_or(|bound| job.created_at < bound))
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
        let mut data = Vec::with_capacity(selected.len());
        for job in selected {
            data.push(self.project_process(job).await);
        }
        Ok(DreamPage { data, next_page })
    }

    pub async fn cancel(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        let current = self.load_process(id)?;
        if current.workspace_id != workspace_id {
            return Err(DreamApiError::NotFound);
        }
        if current.status == DreamStatus::Canceled {
            return Ok(self.project_process(&current).await);
        }
        if current.status.is_terminal() {
            return Err(DreamApiError::BadRequest(
                "only pending or running Dreams can be canceled".into(),
            ));
        }
        let canceled_job = self.commit_job_update(id, |job| {
            job.status = job
                .status
                .transition(DreamStatusEvent::Cancel)
                .ok_or_else(|| {
                    DreamApiError::BadRequest(
                        "only pending or running Dreams can be canceled".into(),
                    )
                })?;
            job.ended_at = Some(now_ms());
            job.cleanup_pending = true;
            Ok(())
        })?;
        if let Some(cancellation) = self.cancellations.lock().unwrap().get(id) {
            cancellation.cancel();
        }
        let preparation = match (
            canceled_job.result_memory_store_id.clone(),
            canceled_job.session_id.clone(),
        ) {
            (Some(result_memory_store_id), Some(session_id)) => Some(DreamPreparation {
                result_memory_store_id,
                session_id,
                transcript_file_ids: canceled_job.transcript_file_ids.clone(),
            }),
            _ => None,
        };
        self.executor
            .cancel(&canceled_job.request(), preparation.as_ref())
            .await
            .map_err(|error| DreamApiError::Unavailable(error.message))?;
        self.commit_job_update(id, |job| {
            job.cleanup_pending = false;
            job.transcript_file_ids.clear();
            Ok(())
        })?;
        self.retrieve(workspace_id, id).await
    }

    pub async fn archive(&self, workspace_id: &str, id: &str) -> Result<Dream, DreamApiError> {
        let current = self.load_process(id)?;
        if current.workspace_id != workspace_id {
            return Err(DreamApiError::NotFound);
        }
        if !current.status.is_terminal() {
            return Err(DreamApiError::BadRequest(
                "only terminal Dreams can be archived".into(),
            ));
        }
        if current.archived_at.is_some() {
            return Ok(self.project_process(&current).await);
        }
        let job = self.commit_job_update(id, |job| {
            job.archived_at = Some(now_ms());
            Ok(())
        })?;
        Ok(self.project_process(&job).await)
    }
}

impl DreamPolicyApplication for DreamApplication {
    fn policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<DreamPolicy, DreamPolicyApplicationError> {
        DreamApplication::policy(self, workspace_id, memory_store_id).map_err(Into::into)
    }

    fn set_policy(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config: DreamPolicyConfig,
    ) -> Result<(), DreamPolicyApplicationError> {
        DreamApplication::set_policy(self, workspace_id, memory_store_id, config)
            .map_err(Into::into)
    }
}

impl From<DreamApiError> for DreamPolicyApplicationError {
    fn from(error: DreamApiError) -> Self {
        match error {
            DreamApiError::BadRequest(message) => Self::BadRequest(message),
            DreamApiError::NotFound => Self::NotFound,
            DreamApiError::Conflict(message) => Self::Conflict(message),
            DreamApiError::Unavailable(message) => Self::Unavailable(message),
        }
    }
}

#[cfg(test)]
mod product_readiness_tests {
    #[test]
    fn volatile_dream_authority_is_opt_in() {
        // Cause/effect graph: C1 default product build; C2 explicit test-support.
        // Effects: E1 no process-local Dream authority is exported; E2 fixtures can
        // exercise the same application state machine. C1 and C2 are exclusive.
        //
        // | Rule | product default | test-support | in-memory store |
        // | T1   | yes             | no           | absent          |
        // | T2   | no              | yes          | present         |
        //
        // Default/all-feature compiler checks complete T1/T2; this fitness test
        // locks the feature selector and both source gates against silent drift.
        let manifest = include_str!("../Cargo.toml");
        let source = include_str!("lib.rs");
        assert!(manifest.contains("test-support = []"), "T2 selector");
        assert!(!manifest.contains("default = [\"test-support\"]"), "T1");
        assert!(
            source.contains(
                "#[cfg(any(test, feature = \"test-support\"))]\npub struct InMemoryDreamProcessStore"
            ),
            "T1/T2 gates"
        );
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
        DreamModelInput::Id(id) => id,
        DreamModelInput::Config(config) => &config.id,
    };
    if model.is_empty() || model.chars().count() > 256 {
        return Err(DreamApiError::BadRequest(
            "model id must contain 1 to 256 characters".into(),
        ));
    }
    if !DREAM_SUPPORTED_MODELS.contains(&model.as_str()) {
        return Err(DreamApiError::BadRequest(format!(
            "unsupported Dream model `{model}`"
        )));
    }
    if matches!(
        &params.model,
        DreamModelInput::Config(DreamModelConfig {
            speed: Some(DreamModelSpeed::Fast),
            ..
        })
    ) {
        return Err(DreamApiError::BadRequest(
            "Dream model speed `fast` is not supported by this deployment".into(),
        ));
    }
    if params
        .instructions
        .as_ref()
        .is_some_and(|value| value.chars().count() > DREAM_MAX_INSTRUCTIONS_CHARS)
    {
        return Err(DreamApiError::BadRequest(format!(
            "instructions may contain at most {DREAM_MAX_INSTRUCTIONS_CHARS} characters"
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
    if sessions.is_empty() || sessions.len() > DREAM_MAX_SESSIONS {
        return Err(DreamApiError::BadRequest(format!(
            "sessions input must contain 1 to {DREAM_MAX_SESSIONS} session ids"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_params() -> DreamCreateParams {
        DreamCreateParams {
            inputs: vec![
                DreamInput::MemoryStore {
                    memory_store_id: "memory-a".into(),
                },
                DreamInput::Sessions {
                    session_ids: vec!["session-a".into(), "session-b".into()],
                },
            ],
            model: DreamModelInput::Id("claude-sonnet-5".into()),
            instructions: None,
        }
    }

    #[test]
    fn create_validation_covers_the_input_cause_effect_graph() {
        // Causes: C1 exactly one non-empty MemoryStore; C2 one bounded unique
        // Session set; C3 supported model/speed; C4 instructions within bound.
        // Effects: E1 accept exact identities; otherwise E2 reject before an
        // executor, process record, output store, or auxiliary Session is made.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1   | T  | T  | T  | T  | E1 |
        // | R2   | F  | *  | *  | *  | E2 |
        // | R3   | T  | F  | *  | *  | E2 |
        // | R4   | T  | T  | F  | *  | E2 |
        // | R5   | T  | T  | T  | F  | E2 |
        let valid = valid_params();
        assert_eq!(
            validate_create(&valid).unwrap(),
            (
                "memory-a".into(),
                vec!["session-a".into(), "session-b".into()]
            ),
            "R1"
        );

        let mut missing_memory = valid_params();
        missing_memory.inputs.remove(0);
        assert!(validate_create(&missing_memory).is_err(), "R2");

        let mut duplicate_session = valid_params();
        duplicate_session.inputs[1] = DreamInput::Sessions {
            session_ids: vec!["session-a".into(), "session-a".into()],
        };
        assert!(validate_create(&duplicate_session).is_err(), "R3");

        let mut unsupported = valid_params();
        unsupported.model = DreamModelInput::Config(DreamModelConfig {
            id: "claude-sonnet-5".into(),
            speed: Some(DreamModelSpeed::Fast),
        });
        assert!(validate_create(&unsupported).is_err(), "R4");

        let mut long_instructions = valid_params();
        long_instructions.instructions = Some("x".repeat(DREAM_MAX_INSTRUCTIONS_CHARS + 1));
        assert!(validate_create(&long_instructions).is_err(), "R5");
    }

    #[test]
    fn terminal_projection_is_hidden_until_cleanup_commits() {
        // Cause/effect rules: T1 terminal + cleanup pending -> public Running,
        // with no ended/error disclosure; T2 cleanup committed -> exact terminal
        // status, end time, and error become visible. This prevents consumers from
        // treating resources as released before the durable cleanup boundary.
        let mut process = DreamProcess {
            id: "dream-1".into(),
            workspace_id: "workspace".into(),
            status: DreamStatus::Failed,
            source_memory_store_id: "memory".into(),
            session_ids: vec!["session".into()],
            model: DreamModelConfig {
                id: "claude-sonnet-5".into(),
                speed: None,
            },
            request_guidance: None,
            agent_id: BUILT_IN_DREAM_AGENT_ID.into(),
            result_memory_store_id: Some("result".into()),
            session_id: Some("auxiliary".into()),
            transcript_file_ids: vec!["transcript".into()],
            cleanup_pending: true,
            created_at: 1,
            ended_at: Some(2),
            archived_at: None,
            error: Some(DreamFailure::new("execution", "failed")),
            policy_key: None,
        };
        let pending = process.project(DreamUsage::default());
        assert_eq!(pending.status, DreamStatus::Running, "T1");
        assert!(pending.ended_at.is_none() && pending.error.is_none(), "T1");

        process.cleanup_pending = false;
        let released = process.project(DreamUsage::default());
        assert_eq!(released.status, DreamStatus::Failed, "T2");
        assert!(
            released.ended_at.is_some() && released.error.is_some(),
            "T2"
        );
    }
}
