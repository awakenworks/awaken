//! Router-scoped A2A task projections, live subscribers, and push subscriptions.
//!
//! Runtime truth remains behind `ProtocolRuntime`; this state only caches the A2A
//! wire projection needed by resubscribe and webhook delivery. It is shared by all
//! routes mounted from one router and never enters the neutral runtime.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

use awaken_protocol_transport::ProtocolRuntime;

use crate::types::{PushNotificationConfig, StreamResponse, Task, TaskStatusUpdateEvent};

pub(crate) const NOTIFICATION_TOKEN_HEADER: &str = "X-A2A-Notification-Token";

#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum PushProtocolVersion {
    #[default]
    V03,
    V1,
}

#[derive(Clone)]
pub(crate) struct A2aState {
    pub runtime: Arc<dyn ProtocolRuntime>,
    inner: Arc<RwLock<Inner>>,
    client: ureq::Agent,
    delivery_queues: Arc<StdMutex<HashMap<String, mpsc::UnboundedSender<Delivery>>>>,
    persistence_path: Option<PathBuf>,
    persistence_lock: Arc<Mutex<()>>,
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, StoredTask>,
    streams: HashMap<String, broadcast::Sender<StreamResponse>>,
    configs: HashMap<String, Vec<StoredConfig>>,
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

struct Delivery {
    task_id: String,
    config: PushNotificationConfig,
    payload: serde_json::Value,
    protocol_version: PushProtocolVersion,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistentState {
    tasks: HashMap<String, StoredTask>,
    configs: HashMap<String, Vec<StoredConfig>>,
}

impl A2aState {
    pub fn new(runtime: Arc<dyn ProtocolRuntime>) -> Self {
        let persistence_path = std::env::var_os("AWAKEN_A2A_STATE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("AWAKEN_STORAGE_DIR")
                    .map(PathBuf::from)
                    .map(|directory| directory.join("a2a-state.json"))
            });
        Self::with_persistence_path(runtime, persistence_path)
    }

    fn with_persistence_path(
        runtime: Arc<dyn ProtocolRuntime>,
        persistence_path: Option<PathBuf>,
    ) -> Self {
        let persistent = persistence_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<PersistentState>(&bytes).ok())
            .unwrap_or_default();
        Self {
            runtime,
            inner: Arc::new(RwLock::new(Inner {
                tasks: persistent.tasks,
                configs: persistent.configs,
                streams: HashMap::new(),
            })),
            client: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(5))
                .build(),
            delivery_queues: Arc::new(StdMutex::new(HashMap::new())),
            persistence_path,
            persistence_lock: Arc::new(Mutex::new(())),
        }
    }

    async fn persist(&self) {
        let Some(path) = self.persistence_path.clone() else {
            return;
        };
        // Serialize snapshot creation and replacement so a slower older write
        // can never overwrite a newer snapshot.
        let _guard = self.persistence_lock.lock().await;
        let snapshot = {
            let inner = self.inner.read().await;
            PersistentState {
                tasks: inner.tasks.clone(),
                configs: inner.configs.clone(),
            }
        };
        let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let parent = path.parent().ok_or("A2A state path has no parent")?;
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let temporary = path.with_extension("json.tmp");
            let bytes = serde_json::to_vec(&snapshot).map_err(|error| error.to_string())?;
            write_private_file(&temporary, &bytes)?;
            std::fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
            Ok(())
        })
        .await;
        if let Err(error) = result.unwrap_or_else(|error| Err(error.to_string())) {
            eprintln!("failed to persist A2A state: {error}");
        }
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

    pub async fn record_task(&self, task: Task, agent_id: Option<String>) {
        let task_id = task.id.clone();
        let response = StreamResponse {
            status_update: Some(TaskStatusUpdateEvent {
                kind: "status-update".into(),
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                status: task.status.clone(),
                final_: task.status.state.is_terminal(),
                metadata: None,
            }),
            ..Default::default()
        };
        {
            let mut inner = self.inner.write().await;
            inner.tasks.insert(
                task_id.clone(),
                StoredTask {
                    task,
                    agent_id: agent_id.clone(),
                },
            );
        }
        self.persist().await;
        self.publish(&task_id, agent_id.as_deref(), response).await;
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

    pub async fn publish(&self, task_id: &str, agent_id: Option<&str>, response: StreamResponse) {
        let (configs, task) = {
            let inner = self.inner.read().await;
            if let Some(tx) = inner.streams.get(task_id) {
                let _ = tx.send(response.clone());
            }
            let configs = inner
                .configs
                .get(task_id)
                .into_iter()
                .flatten()
                .filter(|stored| owner_matches(stored.agent_id.as_deref(), agent_id))
                .cloned()
                .collect::<Vec<_>>();
            let task = inner.tasks.get(task_id).map(|stored| stored.task.clone());
            (configs, task)
        };
        for stored in configs {
            // v0.3 delivers the latest Task snapshot. A2A 1.0 changed push
            // bodies to the same StreamResponse oneof used by streaming.
            let payload = match stored.protocol_version {
                PushProtocolVersion::V03 => {
                    task.clone().map(|task| serde_json::to_value(task).unwrap())
                }
                PushProtocolVersion::V1 => Some(crate::v1::stream_value(&response)),
            };
            let Some(payload) = payload else { continue };
            self.enqueue_delivery(Delivery {
                task_id: task_id.to_string(),
                config: stored.config,
                payload,
                protocol_version: stored.protocol_version,
            });
        }
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
            let client = self.client.clone();
            let queues = Arc::clone(&self.delivery_queues);
            tokio::spawn(async move {
                while let Some(delivery) = receiver.recv().await {
                    if let Err(error) = deliver_with_retry(&client, &delivery).await {
                        eprintln!(
                            "A2A push notification delivery exhausted retries: task={}, config={:?}, error={error}",
                            delivery.task_id, delivery.config.id
                        );
                    }
                }
                queues.lock().expect("delivery queue lock").remove(&key);
            });
            sender
        });
        let _ = sender.send(delivery);
    }

    pub async fn upsert_config(
        &self,
        task_id: &str,
        agent_id: Option<&str>,
        mut config: PushNotificationConfig,
        protocol_version: PushProtocolVersion,
    ) -> Result<PushNotificationConfig, String> {
        validate_config(&config)?;
        config.id.get_or_insert_with(next_config_id);
        let mut inner = self.inner.write().await;
        let stored = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if !owner_matches(stored.agent_id.as_deref(), agent_id) {
            return Err(format!("task not found: {task_id}"));
        }
        let configs = inner.configs.entry(task_id.to_string()).or_default();
        if let Some(index) = configs.iter().position(|existing| {
            existing.config.id == config.id && existing.agent_id.as_deref() == agent_id
        }) {
            configs[index] = StoredConfig {
                config: config.clone(),
                agent_id: agent_id.map(ToOwned::to_owned),
                protocol_version,
            };
        } else {
            configs.push(StoredConfig {
                config: config.clone(),
                agent_id: agent_id.map(ToOwned::to_owned),
                protocol_version,
            });
        }
        drop(inner);
        self.persist().await;
        Ok(config)
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
    ) -> bool {
        let mut inner = self.inner.write().await;
        let Some(task) = inner.tasks.get(task_id) else {
            return false;
        };
        if !owner_matches(task.agent_id.as_deref(), agent_id) {
            return false;
        }
        let Some(configs) = inner.configs.get_mut(task_id) else {
            return false;
        };
        let before = configs.len();
        configs.retain(|stored| {
            !(stored.config.id.as_deref() == Some(config_id)
                && owner_matches(stored.agent_id.as_deref(), agent_id))
        });
        let deleted = configs.len() != before;
        drop(inner);
        if deleted {
            self.delivery_queues
                .lock()
                .expect("delivery queue lock")
                .remove(&format!("{task_id}:{config_id}"));
            self.persist().await;
        }
        deleted
    }
}

trait TerminalState {
    fn is_terminal(&self) -> bool;
}

impl TerminalState for crate::types::TaskState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }
}

fn owner_matches(owner: Option<&str>, requested: Option<&str>) -> bool {
    owner == requested
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
    let lower = config.url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("push notification URL must use http or https".into());
    }
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

fn deliver(
    client: &ureq::Agent,
    config: &PushNotificationConfig,
    payload: &serde_json::Value,
    protocol_version: PushProtocolVersion,
) -> Result<(), String> {
    let mut request = client.post(&config.url);
    if protocol_version == PushProtocolVersion::V1 {
        request = request.set("Content-Type", "application/a2a+json");
    }
    let authenticated = if let Some(authentication) = config.authentication.as_ref()
        && let Some(credentials) = authentication
            .credentials
            .as_deref()
            .filter(|credentials| !credentials.is_empty())
        && let Some(scheme) = authentication.schemes.first()
    {
        request = request.set("Authorization", &format!("{scheme} {credentials}"));
        true
    } else {
        false
    };
    if !authenticated && let Some(token) = config.token.as_deref() {
        request = request.set(NOTIFICATION_TOKEN_HEADER, token);
    }
    match request.send_json(payload.clone()) {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(status, _)) => Err(format!("webhook returned {status}")),
        Err(error) => Err(error.to_string()),
    }
}

async fn deliver_with_retry(client: &ureq::Agent, delivery: &Delivery) -> Result<(), String> {
    const DELAYS_MS: [u64; 5] = [0, 100, 500, 2_000, 5_000];
    let mut last_error = String::new();
    for delay in DELAYS_MS {
        if delay != 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let client = client.clone();
        let config = delivery.config.clone();
        let payload = delivery.payload.clone();
        let protocol_version = delivery.protocol_version;
        let result = tokio::task::spawn_blocking(move || {
            deliver(&client, &config, &payload, protocol_version)
        })
        .await
        .map_err(|error| error.to_string())?;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_agent_contract::agent::message::Message;
    use awaken_protocol_transport::{DriverError, Pending, Resume, StepOutcome};

    struct Runtime;

    #[async_trait]
    impl ProtocolRuntime for Runtime {
        async fn run(
            &self,
            _: &str,
            _: Option<String>,
            _: Vec<Message>,
        ) -> Result<StepOutcome, DriverError> {
            unreachable!()
        }
        async fn resume(&self, _: &str, _: &str, _: Resume) -> Result<StepOutcome, DriverError> {
            unreachable!()
        }
        async fn pending(&self, _: &str) -> Option<Pending> {
            None
        }
        async fn history(&self, _: &str) -> Vec<Message> {
            Vec::new()
        }
        fn model(&self) -> String {
            "test".into()
        }
    }

    #[tokio::test]
    async fn tasks_configs_and_wire_versions_survive_restart() {
        let path = std::env::temp_dir().join(format!(
            "awaken-a2a-state-{}-{}.json",
            std::process::id(),
            next_config_id()
        ));
        let state = A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone()));
        let task = Task {
            kind: Some("task".into()),
            id: "task-persisted".into(),
            context_id: "context-persisted".into(),
            status: crate::types::TaskStatus {
                state: crate::types::TaskState::Working,
                message: None,
                timestamp: Some("2026-07-19T00:00:00Z".into()),
            },
            history: Vec::new(),
            artifacts: Vec::new(),
        };
        state.record_task(task, Some("tenant-a".into())).await;
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

        let restored = A2aState::with_persistence_path(Arc::new(Runtime), Some(path.clone()));
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
}
