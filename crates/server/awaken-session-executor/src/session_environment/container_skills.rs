//! Live skill discovery cache for a Session-owned remote hand.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor, ToolOutput};
use awaken_sandbox_local::DiscoveredSkillFile;
use tokio::sync::Mutex;

use super::HandExecutorFactory;

#[derive(Default)]
pub(crate) struct ContainerSkillCache {
    dirs: std::sync::Mutex<std::collections::BTreeSet<String>>,
    files: std::sync::RwLock<std::collections::BTreeMap<String, Vec<DiscoveredSkillFile>>>,
}

impl ContainerSkillCache {
    pub(super) fn register(&self, subdir: &str) {
        self.dirs.lock().unwrap().insert(subdir.to_string());
    }

    pub(super) fn get(&self, subdir: &str) -> Vec<DiscoveredSkillFile> {
        self.register(subdir);
        self.files
            .read()
            .unwrap()
            .get(subdir)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) async fn refresh(
        &self,
        sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    ) -> Result<(), pc::SandboxError> {
        let dirs: Vec<_> = self.dirs.lock().unwrap().iter().cloned().collect();
        for subdir in dirs {
            let root = super::container_files::workspace_path(&subdir)?;
            let mut discovered = Vec::new();
            for file in sandbox.read_files(&root).await? {
                let Some((id, name)) = file.path.split_once('/') else {
                    continue;
                };
                if name != "SKILL.md" || id.is_empty() || id.contains('/') {
                    continue;
                }
                let Ok(content) = String::from_utf8(file.bytes) else {
                    continue;
                };
                discovered.push(DiscoveredSkillFile {
                    id: id.to_string(),
                    content,
                    dir: format!("{subdir}/{id}"),
                });
            }
            discovered.sort_by(|left, right| left.id.cmp(&right.id));
            self.files.write().unwrap().insert(subdir, discovered);
        }
        Ok(())
    }
}

struct HandBinding {
    process: Box<dyn pc::ProcessHandle>,
    executor: Arc<dyn ToolExecutor>,
}

struct HandLifecycle {
    binding: Mutex<Option<HandBinding>>,
    closed: AtomicBool,
    generation: AtomicU64,
    activity: tokio::sync::watch::Sender<u64>,
}

impl HandLifecycle {
    fn touch(&self) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.activity.send(generation);
    }

    async fn hibernate_if_current(
        &self,
        expected_generation: Option<u64>,
        event: &'static str,
    ) -> bool {
        let mut binding = self.binding.lock().await;
        if expected_generation
            .is_some_and(|expected| self.generation.load(Ordering::Acquire) != expected)
        {
            return false;
        }
        if let Some(binding) = binding.take() {
            if stop_hand_binding(binding, event).await {
                true
            } else {
                // The process outcome is unknown, so launching another Hand could
                // violate the one-owner invariant. Fail closed until the complete
                // Session Environment is reconstructed by its Worker owner.
                self.closed.store(true, Ordering::Release);
                self.touch();
                false
            }
        } else {
            false
        }
    }
}

const HAND_STOP_GRACE: Duration = Duration::from_secs(2);

async fn stop_hand_binding(binding: HandBinding, event: &'static str) -> bool {
    let started = std::time::Instant::now();
    if let Err(error) =
        awaken_run_executor_acp::Supervisor::reap(binding.process.as_ref(), HAND_STOP_GRACE).await
    {
        awaken_observability::record_hand_lifecycle(event, "error", started.elapsed());
        tracing::warn!(
            process = %binding.process.id(),
            error = %error,
            "failed to reap Session hand within the bounded signal ladder"
        );
        false
    } else {
        awaken_observability::add_live_hand(-1);
        awaken_observability::record_hand_lifecycle(event, "ok", started.elapsed());
        true
    }
}

/// The one Session-Environment-owned Hand binding.
///
/// Kubernetes attached-exec channels are live capabilities, not durable Session
/// identity. They may expire while a client is answering a question. This owner
/// reacquires the process/channel only when the executor proves that a request
/// never crossed the dispatch boundary; post-dispatch failures are never replayed.
pub(crate) struct RefreshingHandExecutor {
    sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
    skills: Arc<ContainerSkillCache>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    lifecycle: Arc<HandLifecycle>,
    idle_after: Duration,
}

impl RefreshingHandExecutor {
    pub(super) async fn new(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        skills: Arc<ContainerSkillCache>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        idle_after: Duration,
    ) -> Result<Self, pc::SandboxError> {
        let hand_bin = hand_bin.into();
        let operation_scope = sandbox.id().to_string();
        let binding = Self::launch(
            sandbox.as_ref(),
            factory.as_ref(),
            &hand_bin,
            &operation_scope,
        )
        .await?;
        let (activity, observed_activity) = tokio::sync::watch::channel(0);
        let executor = Self {
            sandbox,
            skills,
            factory,
            hand_bin,
            operation_scope,
            lifecycle: Arc::new(HandLifecycle {
                binding: Mutex::new(Some(binding)),
                closed: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                activity,
            }),
            idle_after,
        };
        executor.spawn_idle_hibernation(observed_activity);
        Ok(executor)
    }

    async fn launch(
        sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
        factory: &dyn HandExecutorFactory,
        hand_bin: &str,
        operation_scope: &str,
    ) -> Result<HandBinding, pc::SandboxError> {
        let started = std::time::Instant::now();
        let process = match sandbox
            .spawn_agent_process(pc::Command {
                argv: vec![hand_bin.to_owned(), "hand".into(), "--stdio".into()],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Piped,
            })
            .await
        {
            Ok(process) => process,
            Err(error) => {
                awaken_observability::record_hand_lifecycle("launch", "error", started.elapsed());
                return Err(error);
            }
        };
        awaken_observability::add_live_hand(1);
        awaken_observability::record_hand_lifecycle("launch", "ok", started.elapsed());
        Ok(HandBinding {
            executor: factory.bind(process.channel, operation_scope),
            process: process.process,
        })
    }

    fn spawn_idle_hibernation(&self, mut activity: tokio::sync::watch::Receiver<u64>) {
        if self.idle_after.is_zero() {
            return;
        }
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let idle_after = self.idle_after;
        let operation_scope = self.operation_scope.clone();
        tokio::spawn(async move {
            let mut generation = *activity.borrow_and_update();
            loop {
                tokio::select! {
                    changed = activity.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        generation = *activity.borrow_and_update();
                        if lifecycle
                            .upgrade()
                            .is_none_or(|lifecycle| lifecycle.closed.load(Ordering::Acquire))
                        {
                            break;
                        }
                    }
                    () = tokio::time::sleep(idle_after) => {
                        let Some(lifecycle) = lifecycle.upgrade() else {
                            break;
                        };
                        if lifecycle.closed.load(Ordering::Acquire) {
                            break;
                        }
                        if lifecycle
                            .hibernate_if_current(Some(generation), "idle_hibernate")
                            .await
                        {
                            tracing::info!(
                                session_environment = %operation_scope,
                                idle_after_ms = idle_after.as_millis(),
                                "hibernated idle Session hand"
                            );
                        }
                        drop(lifecycle);
                        if activity.changed().await.is_err() {
                            break;
                        }
                        generation = *activity.borrow_and_update();
                    }
                }
            }
        });
    }

    pub(super) async fn stop(&self) {
        self.lifecycle.closed.store(true, Ordering::Release);
        self.lifecycle.touch();
        let _ = self
            .lifecycle
            .hibernate_if_current(None, "terminal_stop")
            .await;
    }

    async fn replacement(&self) -> Result<HandBinding, ToolError> {
        let started = std::time::Instant::now();
        let result = Self::launch(
            self.sandbox.as_ref(),
            self.factory.as_ref(),
            &self.hand_bin,
            &self.operation_scope,
        )
        .await;
        awaken_observability::record_hand_lifecycle(
            "reacquire",
            if result.is_ok() { "ok" } else { "error" },
            started.elapsed(),
        );
        result.map_err(|error| {
            ToolError::UnavailableBeforeDispatch(format!(
                "failed to reacquire Session hand: {error}"
            ))
        })
    }
}

#[async_trait]
impl ToolExecutor for RefreshingHandExecutor {
    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if self.lifecycle.closed.load(Ordering::Acquire) {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed".into(),
            ));
        }
        self.lifecycle.touch();
        let mut binding = self.lifecycle.binding.lock().await;
        if self.lifecycle.closed.load(Ordering::Acquire) {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed".into(),
            ));
        }
        if binding.is_none() {
            *binding = Some(self.replacement().await?);
        }
        let result = binding
            .as_ref()
            .expect("binding installed above")
            .executor
            .invoke(call)
            .await;
        let result = if matches!(result, Err(ToolError::UnavailableBeforeDispatch(_))) {
            tracing::warn!(
                session_environment = %self.operation_scope,
                tool_id = %call.tool_id,
                "reacquiring expired Session hand before tool dispatch"
            );
            if let Some(expired) = binding.take()
                && !stop_hand_binding(expired, "expired_reap").await
            {
                self.lifecycle.closed.store(true, Ordering::Release);
                self.lifecycle.touch();
                return Err(ToolError::UnavailableBeforeDispatch(
                    "failed to reap expired Session hand; environment must be reconstructed".into(),
                ));
            }
            if self.lifecycle.closed.load(Ordering::Acquire) {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "Session hand binding closed during reacquisition".into(),
                ));
            }
            *binding = Some(self.replacement().await?);
            binding
                .as_ref()
                .expect("replacement binding installed above")
                .executor
                .invoke(call)
                .await
        } else {
            result
        };
        if matches!(call.tool_id.as_str(), "bash" | "write" | "edit")
            && let Err(error) = self.skills.refresh(self.sandbox.as_ref()).await
        {
            tracing::warn!(error = %error, "failed to refresh container skill catalog");
        }
        self.lifecycle.touch();
        result
    }
}
