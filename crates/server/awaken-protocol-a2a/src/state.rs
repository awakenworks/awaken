//! Router-scoped A2A task projections, live subscribers, and push subscriptions.
//!
//! Runtime truth remains behind `RunApplication`; this state only caches the A2A
//! wire projection needed by resubscribe and webhook delivery. It is shared by all
//! routes mounted from one router and never enters the neutral runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use awaken_outbound_http::{GuardedHttpSender, HttpSender};
use awaken_session_contract::RunApplication;

use crate::types::{PushNotificationConfig, StreamResponse, Task, TaskStatusUpdateEvent};

pub(crate) const NOTIFICATION_TOKEN_HEADER: &str = "X-A2A-Notification-Token";
pub(crate) const DELIVERY_ID_HEADER: &str = "X-A2A-Delivery-Id";

#[cfg(not(test))]
const PROJECTION_RETRY_DELAY: Duration = Duration::from_secs(5);
#[cfg(test)]
const PROJECTION_RETRY_DELAY: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const DELIVERY_RETRY_DELAYS_MS: [u64; 5] = [0, 100, 500, 2_000, 5_000];
#[cfg(test)]
const DELIVERY_RETRY_DELAYS_MS: [u64; 5] = [0, 1, 2, 3, 4];

#[derive(Debug, thiserror::Error)]
pub(crate) enum StateError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("A2A projection persistence failed: {0}")]
    Storage(String),
}

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum PushProtocolVersion {
    #[default]
    V03,
    V1,
}

#[derive(Clone)]
pub(crate) struct A2aState {
    pub runtime: Arc<dyn RunApplication>,
    inner: Arc<RwLock<Inner>>,
    push_sender: Arc<dyn HttpSender>,
    delivery_queues: Arc<StdMutex<HashMap<String, mpsc::UnboundedSender<Delivery>>>>,
    persistence_path: Option<PathBuf>,
    persistence_lock: Arc<Mutex<()>>,
    delivery_recovery_started: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, StoredTask>,
    streams: HashMap<String, broadcast::Sender<StreamResponse>>,
    configs: HashMap<String, Vec<StoredConfig>>,
    deliveries: HashMap<String, Delivery>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredTask {
    task: Task,
    agent_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredConfig {
    config: PushNotificationConfig,
    agent_id: Option<String>,
    #[serde(default)]
    protocol_version: PushProtocolVersion,
}

#[derive(Clone, Serialize, Deserialize)]
struct Delivery {
    id: String,
    task_id: String,
    config: PushNotificationConfig,
    payload: serde_json::Value,
    protocol_version: PushProtocolVersion,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct PersistentState {
    tasks: HashMap<String, StoredTask>,
    configs: HashMap<String, Vec<StoredConfig>>,
    #[serde(default)]
    deliveries: HashMap<String, Delivery>,
}

impl A2aState {
    pub fn new(
        runtime: Arc<dyn RunApplication>,
        persistence_path: Option<PathBuf>,
    ) -> Result<Self, StateError> {
        Self::with_persistence_path(runtime, persistence_path)
    }

    fn with_persistence_path(
        runtime: Arc<dyn RunApplication>,
        persistence_path: Option<PathBuf>,
    ) -> Result<Self, StateError> {
        let persistent = match persistence_path.as_ref().map(std::fs::read) {
            None => PersistentState::default(),
            Some(Ok(bytes)) => serde_json::from_slice::<PersistentState>(&bytes)
                .map_err(|error| StateError::Storage(error.to_string()))?,
            Some(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                PersistentState::default()
            }
            Some(Err(error)) => return Err(StateError::Storage(error.to_string())),
        };
        let state = Self {
            runtime,
            inner: Arc::new(RwLock::new(Inner {
                tasks: persistent.tasks,
                configs: persistent.configs,
                deliveries: persistent.deliveries,
                streams: HashMap::new(),
            })),
            #[cfg(not(test))]
            push_sender: Arc::new(GuardedHttpSender::guarded()),
            #[cfg(test)]
            push_sender: Arc::new(GuardedHttpSender::with_timeout(Duration::from_secs(1))),
            delivery_queues: Arc::new(StdMutex::new(HashMap::new())),
            persistence_path,
            persistence_lock: Arc::new(Mutex::new(())),
            delivery_recovery_started: Arc::new(AtomicBool::new(false)),
        };
        state.start_delivery_recovery();
        Ok(state)
    }

    async fn write_snapshot(&self, snapshot: PersistentState) -> Result<(), StateError> {
        let Some(path) = self.persistence_path.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let parent = path.parent().ok_or("A2A state path has no parent")?;
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let temporary = path.with_extension("json.tmp");
            let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
            write_private_file(&temporary, &bytes)?;
            std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
            Ok(())
        })
        .await
        .map_err(|error| StateError::Storage(error.to_string()))?
        .map_err(StateError::Storage)
    }

    /// Serialize every durable mutation, write the next snapshot first, and only
    /// then publish it to in-process readers. A failed write therefore cannot
    /// produce a successful response or a volatile state that disagrees with the
    /// restart image.
    async fn commit<R>(
        &self,
        mutate: impl FnOnce(&mut PersistentState) -> Result<(R, bool), StateError>,
    ) -> Result<R, StateError> {
        let _guard = self.persistence_lock.lock().await;
        let mut next = {
            let inner = self.inner.read().await;
            PersistentState {
                tasks: inner.tasks.clone(),
                configs: inner.configs.clone(),
                deliveries: inner.deliveries.clone(),
            }
        };
        let (result, changed) = mutate(&mut next)?;
        if changed {
            self.write_snapshot(next.clone()).await?;
            let mut inner = self.inner.write().await;
            inner.tasks = next.tasks;
            inner.configs = next.configs;
            inner.deliveries = next.deliveries;
        }
        Ok(result)
    }

    pub async fn task(&self, task_id: &str, agent_id: Option<&str>) -> Option<Task> {
        self.inner
            .read()
            .await
            .tasks
            .get(task_id)
            .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id))
            .map(|stored| stored.task.clone())
    }

    pub async fn tasks(&self, agent_id: Option<&str>) -> Vec<Task> {
        let mut tasks = self
            .inner
            .read()
            .await
            .tasks
            .values()
            .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id))
            .map(|stored| stored.task.clone())
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.id.cmp(&right.id));
        tasks
    }

    pub async fn record_task(
        &self,
        task: Task,
        agent_id: Option<String>,
    ) -> Result<(), StateError> {
        self.record_task_with_config(task, agent_id, None, PushProtocolVersion::V03)
            .await
    }

    /// Atomically publish the initial Working projection and its inline push
    /// subscription. A failed request cannot leave only one half installed.
    pub async fn record_task_with_config(
        &self,
        task: Task,
        agent_id: Option<String>,
        mut config: Option<PushNotificationConfig>,
        protocol_version: PushProtocolVersion,
    ) -> Result<(), StateError> {
        if let Some(config) = config.as_mut() {
            validate_config(config).map_err(StateError::Invalid)?;
            config.id.get_or_insert_with(next_config_id);
        }
        let task_id = task.id.clone();
        let response = if config.is_some() {
            StreamResponse::Task(task.clone())
        } else {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                kind: crate::types::TaskStatusUpdateKind::StatusUpdate,
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                status: task.status.clone(),
                final_: task.status.state.is_terminal(),
                metadata: None,
            })
        };
        let deliveries = self
            .commit(|persistent| {
                persistent.tasks.insert(
                    task_id.clone(),
                    StoredTask {
                        task: task.clone(),
                        agent_id: agent_id.clone(),
                    },
                );
                if let Some(config) = config {
                    upsert_config_snapshot(
                        persistent,
                        &task_id,
                        agent_id.as_deref(),
                        config,
                        protocol_version,
                    )?;
                }
                let deliveries =
                    stage_deliveries(persistent, &task_id, agent_id.as_deref(), &response);
                Ok((deliveries, true))
            })
            .await?;
        self.broadcast(&task_id, response).await;
        self.enqueue_deliveries(deliveries);
        Ok(())
    }

    /// Background sends have no response channel on which a later projection
    /// write can fail. Keep the already-durable Working task visible and retry
    /// the terminal replacement until the injected store recovers.
    pub async fn record_task_reliably(&self, task: Task, agent_id: Option<String>) {
        loop {
            match self.record_task(task.clone(), agent_id.clone()).await {
                Ok(()) => return,
                Err(error) => {
                    eprintln!(
                        "A2A task projection remains pending until storage recovers: task={}, error={error}",
                        task.id
                    );
                    tokio::time::sleep(PROJECTION_RETRY_DELAY).await;
                }
            }
        }
    }

    pub async fn subscribe(
        &self,
        task_id: &str,
        agent_id: Option<&str>,
    ) -> Option<(Task, broadcast::Receiver<StreamResponse>)> {
        let mut inner = self.inner.write().await;
        let task = inner
            .tasks
            .get(task_id)
            .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id))?
            .task
            .clone();
        let tx = inner.streams.entry(task_id.to_string()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(128);
            tx
        });
        Some((task, tx.subscribe()))
    }

    pub async fn publish(
        &self,
        task_id: &str,
        agent_id: Option<&str>,
        response: StreamResponse,
    ) -> Result<(), StateError> {
        self.start_delivery_recovery();
        let deliveries = self
            .commit(|persistent| {
                let deliveries = stage_deliveries(persistent, task_id, agent_id, &response);
                let changed = !deliveries.is_empty();
                Ok((deliveries, changed))
            })
            .await?;
        self.broadcast(task_id, response).await;
        self.enqueue_deliveries(deliveries);
        Ok(())
    }

    async fn broadcast(&self, task_id: &str, response: StreamResponse) {
        if let Some(tx) = self.inner.read().await.streams.get(task_id) {
            let _ = tx.send(response);
        }
    }

    fn enqueue_deliveries(&self, deliveries: Vec<Delivery>) {
        for delivery in deliveries {
            self.enqueue_delivery(delivery);
        }
    }

    fn start_delivery_recovery(&self) {
        if tokio::runtime::Handle::try_current().is_err()
            || self.delivery_recovery_started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let deliveries = state
                .inner
                .read()
                .await
                .deliveries
                .values()
                .cloned()
                .collect();
            state.enqueue_deliveries(deliveries);
        });
    }

    fn enqueue_delivery(&self, delivery: Delivery) {
        let key = format!(
            "{}:{}",
            delivery.task_id,
            delivery.config.id.as_deref().unwrap_or_default()
        );
        let mut queues = self.delivery_queues.lock().expect("delivery queue lock");
        let sender = queues.entry(key.clone()).or_insert_with(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel::<Delivery>();
            let state = self.clone();
            let queues = Arc::clone(&self.delivery_queues);
            tokio::spawn(async move {
                while let Some(delivery) = receiver.recv().await {
                    loop {
                        if !state.delivery_is_pending(&delivery.id).await {
                            break;
                        }
                        match deliver_with_retry(state.push_sender.as_ref(), &delivery).await {
                            Ok(()) => {
                                state.settle_delivery_reliably(&delivery.id, "delivered").await;
                                break;
                            }
                            Err(DeliveryFailure::Permanent(error)) => {
                                eprintln!(
                                    "A2A push notification was permanently rejected: delivery={}, task={}, config={:?}, error={error}",
                                    delivery.id, delivery.task_id, delivery.config.id
                                );
                                state.settle_delivery_reliably(&delivery.id, "rejected").await;
                                break;
                            }
                            Err(DeliveryFailure::Retryable(error)) => {
                                eprintln!(
                                    "A2A push notification remains pending after retry window: delivery={}, task={}, config={:?}, error={error}",
                                    delivery.id, delivery.task_id, delivery.config.id
                                );
                                tokio::time::sleep(Duration::from_secs(30)).await;
                            }
                        }
                    }
                }
                queues.lock().expect("delivery queue lock").remove(&key);
            });
            sender
        });
        let _ = sender.send(delivery);
    }

    async fn delivery_is_pending(&self, delivery_id: &str) -> bool {
        self.inner.read().await.deliveries.contains_key(delivery_id)
    }

    async fn complete_delivery(&self, delivery_id: &str) -> Result<(), StateError> {
        self.commit(|persistent| {
            let changed = persistent.deliveries.remove(delivery_id).is_some();
            Ok(((), changed))
        })
        .await
    }

    async fn settle_delivery_reliably(&self, delivery_id: &str, outcome: &str) {
        while self.delivery_is_pending(delivery_id).await {
            if let Err(error) = self.complete_delivery(delivery_id).await {
                eprintln!(
                    "A2A {outcome} push could not be settled durably: delivery={delivery_id}, error={error}"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }

    pub async fn upsert_config(
        &self,
        task_id: &str,
        agent_id: Option<&str>,
        mut config: PushNotificationConfig,
        protocol_version: PushProtocolVersion,
    ) -> Result<PushNotificationConfig, StateError> {
        validate_config(&config).map_err(StateError::Invalid)?;
        config.id.get_or_insert_with(next_config_id);
        self.commit(|persistent| {
            upsert_config_snapshot(
                persistent,
                task_id,
                agent_id,
                config.clone(),
                protocol_version,
            )?;
            Ok((config.clone(), true))
        })
        .await
    }

    pub async fn configs(
        &self,
        task_id: &str,
        agent_id: Option<&str>,
    ) -> Option<Vec<PushNotificationConfig>> {
        let inner = self.inner.read().await;
        let task = inner.tasks.get(task_id)?;
        if !owner_matches(task.agent_id.as_deref(), agent_id) {
            return None;
        }
        Some(
            inner
                .configs
                .get(task_id)
                .into_iter()
                .flatten()
                .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id))
                .map(|stored| stored.config.redacted())
                .collect(),
        )
    }

    pub async fn config(
        &self,
        task_id: &str,
        config_id: &str,
        agent_id: Option<&str>,
    ) -> Option<PushNotificationConfig> {
        self.configs(task_id, agent_id)
            .await?
            .into_iter()
            .find(|config| config.id.as_deref() == Some(config_id))
    }

    pub async fn delete_config(
        &self,
        task_id: &str,
        config_id: &str,
        agent_id: Option<&str>,
    ) -> Result<bool, StateError> {
        let deleted = self
            .commit(|persistent| {
                let Some(task) = persistent.tasks.get(task_id) else {
                    return Ok((false, false));
                };
                if !owner_matches(task.agent_id.as_deref(), agent_id) {
                    return Ok((false, false));
                }
                let Some(configs) = persistent.configs.get_mut(task_id) else {
                    return Ok((false, false));
                };
                let before = configs.len();
                configs.retain(|stored| {
                    !(stored.config.id.as_deref() == Some(config_id)
                        && owner_matches(stored.agent_id.as_deref(), agent_id))
                });
                let deleted = configs.len() != before;
                if deleted {
                    persistent.deliveries.retain(|_, delivery| {
                        !(delivery.task_id == task_id
                            && delivery.config.id.as_deref() == Some(config_id))
                    });
                }
                Ok((deleted, deleted))
            })
            .await?;
        if deleted {
            self.delivery_queues
                .lock()
                .expect("delivery queue lock")
                .remove(&format!("{task_id}:{config_id}"));
        }
        Ok(deleted)
    }
}

fn owner_matches(owner: Option<&str>, requested: Option<&str>) -> bool {
    owner == requested
}

fn upsert_config_snapshot(
    persistent: &mut PersistentState,
    task_id: &str,
    agent_id: Option<&str>,
    config: PushNotificationConfig,
    protocol_version: PushProtocolVersion,
) -> Result<(), StateError> {
    let stored = persistent
        .tasks
        .get(task_id)
        .ok_or_else(|| StateError::NotFound(format!("task not found: {task_id}")))?;
    if !owner_matches(stored.agent_id.as_deref(), agent_id) {
        return Err(StateError::NotFound(format!("task not found: {task_id}")));
    }
    let configs = persistent.configs.entry(task_id.to_string()).or_default();
    let stored = StoredConfig {
        config: config.clone(),
        agent_id: agent_id.map(ToOwned::to_owned),
        protocol_version,
    };
    if let Some(index) = configs.iter().position(|existing| {
        existing.config.id == config.id && existing.agent_id.as_deref() == agent_id
    }) {
        configs[index] = stored;
    } else {
        configs.push(stored);
    }
    Ok(())
}

/// Freeze each outbound body and credential projection in the same snapshot as
/// the event that caused it. Delivery is therefore at-least-once across crashes;
/// the stable delivery id lets receivers deduplicate retries.
fn stage_deliveries(
    persistent: &mut PersistentState,
    task_id: &str,
    agent_id: Option<&str>,
    response: &StreamResponse,
) -> Vec<Delivery> {
    let task = persistent.tasks.get(task_id).map(|stored| &stored.task);
    let configs = persistent
        .configs
        .get(task_id)
        .into_iter()
        .flatten()
        .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id));
    let mut deliveries = Vec::new();
    for stored in configs {
        // v0.3 delivers the latest Task snapshot. A2A 1.0 changed push
        // bodies to the same StreamResponse oneof used by streaming.
        let payload = match stored.protocol_version {
            PushProtocolVersion::V03 => task.and_then(|task| serde_json::to_value(task).ok()),
            PushProtocolVersion::V1 => Some(crate::v1::stream_value(response)),
        };
        let Some(payload) = payload else { continue };
        let delivery = Delivery {
            id: next_delivery_id(),
            task_id: task_id.to_string(),
            config: stored.config.clone(),
            payload,
            protocol_version: stored.protocol_version,
        };
        persistent
            .deliveries
            .insert(delivery.id.clone(), delivery.clone());
        deliveries.push(delivery);
    }
    deliveries
}

fn next_config_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    format!(
        "push-{:x}-{nanos:x}-{:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn next_delivery_id() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    format!(
        "delivery-{:x}-{nanos:x}-{:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn validate_config(config: &PushNotificationConfig) -> Result<(), String> {
    awaken_outbound_http::validate_endpoint_url(&config.url).map_err(|error| error.to_string())?;
    if let Some(authentication) = config.authentication.as_ref()
        && (authentication.schemes.is_empty()
            || authentication
                .schemes
                .iter()
                .any(|scheme| scheme.trim().is_empty()))
    {
        return Err("authentication scheme must not be empty".into());
    }
    Ok(())
}

enum DeliveryFailure {
    Retryable(String),
    Permanent(String),
}

async fn deliver(sender: &dyn HttpSender, delivery: &Delivery) -> Result<(), DeliveryFailure> {
    let config = &delivery.config;
    let protocol_version = delivery.protocol_version;
    let mut headers = vec![(DELIVERY_ID_HEADER.to_string(), delivery.id.clone())];
    if protocol_version == PushProtocolVersion::V1 {
        headers.push(("Content-Type".into(), "application/a2a+json".into()));
    }
    let authenticated = if let Some(authentication) = config.authentication.as_ref()
        && let Some(credentials) = authentication
            .credentials
            .as_deref()
            .filter(|credentials| !credentials.is_empty())
        && let Some(scheme) = authentication.schemes.first()
    {
        headers.push(("Authorization".into(), format!("{scheme} {credentials}")));
        true
    } else {
        false
    };
    if !authenticated && let Some(token) = config.token.as_deref() {
        headers.push((NOTIFICATION_TOKEN_HEADER.into(), token.into()));
    }
    let status = sender
        .post(
            &config.url,
            headers,
            serde_json::to_string(&delivery.payload)
                .map_err(|error| DeliveryFailure::Permanent(error.to_string()))?,
        )
        .await
        .map_err(DeliveryFailure::Retryable)?;
    if (200..300).contains(&status) {
        Ok(())
    } else if awaken_outbound_http::status_is_retryable(status) {
        Err(DeliveryFailure::Retryable(format!(
            "push endpoint returned {status}"
        )))
    } else {
        Err(DeliveryFailure::Permanent(format!(
            "push endpoint returned {status}"
        )))
    }
}

async fn deliver_with_retry(
    sender: &dyn HttpSender,
    delivery: &Delivery,
) -> Result<(), DeliveryFailure> {
    let mut last_error = String::new();
    for delay in DELIVERY_RETRY_DELAYS_MS {
        if delay != 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match deliver(sender, delivery).await {
            Ok(()) => return Ok(()),
            Err(DeliveryFailure::Permanent(error)) => {
                return Err(DeliveryFailure::Permanent(error));
            }
            Err(DeliveryFailure::Retryable(error)) => last_error = error,
        }
    }
    Err(DeliveryFailure::Retryable(last_error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::Message;
    use awaken_session_contract::{Pending, RunApplicationError, RunResume, StepOutcome};

    struct Runtime;

    #[async_trait]
    impl RunApplication for Runtime {
        async fn run(
            &self,
            _operation_id: &str,
            _: &str,
            _: Option<String>,
            _: Vec<Message>,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }
        async fn resume(
            &self,
            _operation_id: &str,
            _: &str,
            _: &str,
            _: RunResume,
        ) -> Result<StepOutcome, RunApplicationError> {
            unreachable!()
        }
        async fn pending(&self, _: &str) -> Result<Option<Pending>, RunApplicationError> {
            Ok(None)
        }
        async fn history(&self, _: &str) -> Result<Vec<Message>, RunApplicationError> {
            Ok(Vec::new())
        }
        fn model(&self) -> String {
            "test".into()
        }
    }

    fn working_task(id: &str) -> Task {
        Task {
            kind: crate::types::TaskKind::Task,
            id: id.into(),
            context_id: format!("context-{id}"),
            status: crate::types::TaskStatus {
                state: crate::types::TaskState::Working,
                message: None,
                timestamp: Some("2026-07-19T00:00:00Z".into()),
            },
            history: Vec::new(),
            artifacts: Vec::new(),
            metadata: None,
        }
    }

    fn unique_state_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "awaken-a2a-{label}-{}-{}.json",
            std::process::id(),
            next_config_id()
        ))
    }

    #[tokio::test]
    async fn tasks_configs_and_wire_versions_survive_restart() {
        // Cause/effect graph:
        // C1 an explicit persistence path is injected -> E1 task/config state
        // survives construction of a new adapter; C2 protocol version and owner
        // are persisted with the config -> E2 both remain available to the same
        // tenant after restart.
        //
        // Decision rule R1: C1 && C2 => E1 && E2. The complementary no-path
        // rule is represented structurally by `router`, which injects `None`
        // and therefore creates no persistence side effect.
        let path = unique_state_path("restart");
        let state = A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone())).unwrap();
        state
            .record_task(working_task("task-persisted"), Some("tenant-a".into()))
            .await
            .unwrap();
        state
            .upsert_config(
                "task-persisted",
                Some("tenant-a"),
                PushNotificationConfig {
                    id: Some("push-persisted".into()),
                    url: "https://example.invalid/hook".into(),
                    token: None,
                    authentication: None,
                },
                PushProtocolVersion::V1,
            )
            .await
            .unwrap();
        drop(state);

        let restored =
            A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone())).unwrap();
        assert!(
            restored
                .task("task-persisted", Some("tenant-a"))
                .await
                .is_some()
        );
        assert_eq!(
            restored
                .configs("task-persisted", Some("tenant-a"))
                .await
                .unwrap()
                .len(),
            1
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn corrupt_or_unwritable_projection_never_becomes_empty_or_volatile_success() {
        // Cause/effect graph and decision table:
        // C1 persisted bytes are valid; C2 the next snapshot is writable.
        // E1 construction succeeds; E2 mutation returns success; E3 the
        // task+inline config+initial delivery are atomically visible in memory
        // and after restart.
        // R1 C1 && C2 => E1 && E2 && E3 (covered by the restart test above).
        // R2 !C1 => !E1: corrupt durable state is a startup failure, never an
        // empty projection. R3 C1 && !C2 => E1 && !E2 && !E3: write failure
        // cannot publish any volatile task/config/delivery half-state.
        let corrupt_path = unique_state_path("corrupt");
        std::fs::write(&corrupt_path, b"not-json").unwrap();
        assert!(matches!(
            A2aState::with_persistence_path(Arc::new(Runtime), Some(corrupt_path.clone())),
            Err(StateError::Storage(_))
        ));
        std::fs::remove_file(corrupt_path).unwrap();

        let unwritable_path = unique_state_path("unwritable");
        let state =
            A2aState::with_persistence_path(Arc::new(Runtime), Some(unwritable_path.clone()))
                .unwrap();
        std::fs::create_dir(&unwritable_path).unwrap();
        assert!(matches!(
            state
                .record_task_with_config(
                    working_task("task-volatile"),
                    None,
                    Some(PushNotificationConfig {
                        id: Some("config-volatile".into()),
                        url: "https://push.example.com/hook".into(),
                        token: None,
                        authentication: None,
                    }),
                    PushProtocolVersion::V1,
                )
                .await,
            Err(StateError::Storage(_))
        ));
        assert!(state.task("task-volatile", None).await.is_none());
        assert!(state.inner.read().await.configs.is_empty());
        assert!(state.inner.read().await.deliveries.is_empty());

        let temporary = unwritable_path.with_extension("json.tmp");
        if temporary.exists() {
            std::fs::remove_file(temporary).unwrap();
        }
        std::fs::remove_dir(unwritable_path).unwrap();
    }

    #[tokio::test]
    async fn asynchronous_terminal_projection_converges_after_storage_recovers() {
        // Cause/effect graph and decision table:
        // C1 a return-immediately run already has a durable Working task; C2 its
        // terminal write fails; C3 storage later recovers. E1 the caller never
        // receives a false terminal success; E2 Working remains the last durable
        // state during failure; E3 retry eventually replaces it with terminal.
        // R1 C1&&C2&&!C3 => E1&&E2. R2 C1&&C2&&C3 => E1&&E2&&E3.
        let path = unique_state_path("async-retry");
        let state = A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone())).unwrap();
        std::fs::create_dir(&path).unwrap();
        let retry_state = state.clone();
        let retry = tokio::spawn(async move {
            retry_state
                .record_task_reliably(working_task("task-eventual"), None)
                .await;
        });
        let temporary = path.with_extension("json.tmp");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !temporary.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first durable write was attempted");
        assert!(state.task("task-eventual", None).await.is_none());
        std::fs::remove_dir(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(2), retry)
            .await
            .expect("projection converged after recovery")
            .unwrap();
        assert!(state.task("task-eventual", None).await.is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn push_delivery_intent_is_staged_recovered_acknowledged_or_canceled_durably() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Cause/effect graph:
        // C1 a matching push config exists; C2 a Task/stream fact is published;
        // C3 the process restarts with a pending delivery; C4 the receiver ACKs;
        // C5 the config is deleted before delivery.
        // Effects: E1 body+credentials+stable delivery id are staged in the same
        // durable projection; E2 restart redelivers; E3 ACK durably removes the
        // intent; E4 config deletion removes its pending intents.
        // Decision table: R1 C1&&C2&&!C4&&!C5 => E1 (failed/unconfirmed delivery
        // remains pending); R2 C3&&C4 => E2&&E3; R3 C1&&C2&&C5 => E4. A config
        // deletion and receiver ACK are mutually exclusive terminal settlements.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let path = unique_state_path("delivery-recovery");
        let delivery_id = "delivery-recovered".to_string();
        let persistent = PersistentState {
            deliveries: HashMap::from([(
                delivery_id.clone(),
                Delivery {
                    id: delivery_id.clone(),
                    task_id: "task-recovered".into(),
                    config: PushNotificationConfig {
                        id: Some("config-recovered".into()),
                        url: format!("http://{address}/push"),
                        token: None,
                        authentication: None,
                    },
                    payload: serde_json::json!({"kind": "task"}),
                    protocol_version: PushProtocolVersion::V03,
                },
            )]),
            ..PersistentState::default()
        };
        write_private_file(&path, &serde_json::to_vec(&persistent).unwrap()).unwrap();

        let receiver = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let read = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..read]);
            assert!(request.contains("POST /push HTTP/1.1"));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-a2a-delivery-id: delivery-recovered")
            );
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let restored =
            A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone())).unwrap();
        tokio::time::timeout(Duration::from_secs(5), receiver)
            .await
            .expect("recovered delivery reached receiver")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while restored.delivery_is_pending(&delivery_id).await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("receiver ACK was persisted");
        let acknowledged: PersistentState =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(acknowledged.deliveries.is_empty());
        std::fs::remove_file(path).unwrap();

        let cancel_path = unique_state_path("delivery-cancel");
        let state =
            A2aState::with_persistence_path(Arc::new(Runtime), Some(cancel_path.clone())).unwrap();
        state
            .record_task(working_task("task-cancel-delivery"), None)
            .await
            .unwrap();
        state
            .upsert_config(
                "task-cancel-delivery",
                None,
                PushNotificationConfig {
                    id: Some("config-cancel-delivery".into()),
                    url: "https://push.example.invalid/unavailable".into(),
                    token: None,
                    authentication: None,
                },
                PushProtocolVersion::V1,
            )
            .await
            .unwrap();
        state
            .record_task(working_task("task-cancel-delivery"), None)
            .await
            .unwrap();
        assert_eq!(state.inner.read().await.deliveries.len(), 1);
        assert!(
            state
                .delete_config("task-cancel-delivery", "config-cancel-delivery", None)
                .await
                .unwrap()
        );
        assert!(state.inner.read().await.deliveries.is_empty());
        let canceled: PersistentState =
            serde_json::from_slice(&std::fs::read(&cancel_path).unwrap()).unwrap();
        assert!(canceled.deliveries.is_empty());
        std::fs::remove_file(cancel_path).unwrap();
    }

    #[test]
    fn push_url_admission_reuses_the_production_ssrf_policy() {
        // Cause/effect graph: C1 scheme is HTTPS; C2 host is a public DNS name
        // or globally-routable literal. E1 config is admitted. Decision table:
        // R1 C1&&C2 => E1; R2 !C1 => reject before persistence; R3 C1&&!C2 =>
        // reject loopback/private/metadata before persistence. Delivery-time DNS
        // rebinding is independently closed by the reused guarded sender.
        let config = |url: &str| PushNotificationConfig {
            id: None,
            url: url.into(),
            token: None,
            authentication: None,
        };
        assert!(validate_config(&config("https://push.example.com/hook")).is_ok());
        for url in [
            "http://push.example.com/hook",
            "https://127.0.0.1/admin",
            "https://localhost/admin",
            "https://169.254.169.254/latest/meta-data",
        ] {
            assert!(validate_config(&config(url)).is_err(), "accepted {url}");
        }
    }

    struct StatusSender {
        status: u16,
        calls: AtomicU64,
    }

    #[async_trait]
    impl HttpSender for StatusSender {
        async fn post(&self, _: &str, _: Vec<(String, String)>, _: String) -> Result<u16, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.status)
        }
    }

    #[tokio::test]
    async fn push_status_classification_bounds_permanent_rejection_retries() {
        // Cause/effect graph: C1 response is 2xx; C2 is retryable
        // 408/425/429/5xx; C3 is another 3xx/4xx. Effects: E1 settle delivered
        // after one; E2 exhaust the bounded attempt window and retain intent;
        // E3 stop after one and settle rejected. C1/C2/C3 are mutually exclusive.
        // Decision rules R1 C1=>E1, R2 C2=>E2, R3 C3=>E3.
        let delivery = Delivery {
            id: "delivery-classification".into(),
            task_id: "task".into(),
            config: PushNotificationConfig {
                id: Some("config".into()),
                url: "https://push.example.com/hook".into(),
                token: None,
                authentication: None,
            },
            payload: serde_json::json!({}),
            protocol_version: PushProtocolVersion::V1,
        };
        for (status, expected_calls, expected) in [
            (204, 1, "ok"),
            (429, 5, "retryable"),
            (500, 5, "retryable"),
            (302, 1, "permanent"),
            (404, 1, "permanent"),
        ] {
            let sender = StatusSender {
                status,
                calls: AtomicU64::new(0),
            };
            let result = deliver_with_retry(&sender, &delivery).await;
            assert_eq!(sender.calls.load(Ordering::SeqCst), expected_calls);
            match expected {
                "ok" => assert!(result.is_ok()),
                "retryable" => assert!(matches!(result, Err(DeliveryFailure::Retryable(_)))),
                "permanent" => assert!(matches!(result, Err(DeliveryFailure::Permanent(_)))),
                _ => unreachable!(),
            }
        }
    }
}
