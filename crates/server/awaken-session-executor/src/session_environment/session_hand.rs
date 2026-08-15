//! The single Hand lifecycle shared by every tool-transparent Session environment.
//!
//! Namespace and Container providers differ only in how they open a channel. Tool
//! dispatch, retry fencing, idle hibernation, and process ownership remain one
//! Runtime-Host responsibility.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::Sandbox as _;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability};
use awaken_sandbox_local::NamespaceSandbox;
use tokio::sync::Mutex;

use super::HandExecutorFactory;
use super::container_skills::ContainerSkillCache;

pub(super) enum SessionAgentLauncher {
    Namespace(Arc<NamespaceSandbox>),
    Container {
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
    },
}

impl SessionAgentLauncher {
    fn operation_scope(&self) -> String {
        match self {
            Self::Namespace(sandbox) => sandbox.handle().sandbox_id,
            Self::Container { sandbox, .. } => sandbox.id().to_string(),
        }
    }

    pub(super) async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<
        (
            Box<dyn pc::ProcessHandle>,
            Box<dyn awaken_run_executor_acp::AgentChannelType>,
        ),
        pc::SandboxError,
    > {
        match self {
            Self::Namespace(sandbox) => sandbox.spawn_agent(command).await,
            Self::Container { sandbox } => sandbox
                .spawn_agent_process(command)
                .await
                .map(|process| (process.process, process.channel)),
        }
    }

    async fn open_resident_channel(
        &self,
    ) -> Result<Box<dyn awaken_run_executor_acp::AgentChannelType>, pc::SandboxError> {
        match self {
            Self::Container { sandbox } => sandbox.open_agent_channel().await,
            Self::Namespace(_) => Err(pc::SandboxError::new(
                "namespace Session environments do not own a resident Hand",
            )),
        }
    }
}

struct HandBinding {
    process: Option<Box<dyn pc::ProcessHandle>>,
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
                // The process outcome is unknown. Starting another Hand could
                // violate the one-owner invariant, so fail closed.
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
    let Some(process) = binding.process else {
        // A resident Hand belongs to the Session Pod. Dropping the channel must
        // never terminate that workload.
        return true;
    };
    let started = std::time::Instant::now();
    if let Err(error) =
        awaken_run_executor_acp::Supervisor::reap(process.as_ref(), HAND_STOP_GRACE).await
    {
        awaken_observability::record_hand_lifecycle(event, "error", started.elapsed());
        tracing::warn!(
            process = %process.id(),
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
pub(crate) struct SessionHandExecutor {
    launcher: Arc<SessionAgentLauncher>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    lifecycle: Arc<HandLifecycle>,
    projection_update: Mutex<()>,
    projection_updating: AtomicBool,
    idle_after: Duration,
    residency: crate::deployment_config::ContainerHandResidency,
    container_skills: Option<(
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        Arc<ContainerSkillCache>,
    )>,
}

impl SessionHandExecutor {
    pub(super) fn namespace(
        sandbox: Arc<NamespaceSandbox>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        idle_after: Duration,
    ) -> Self {
        // Namespace adoption must rebuild its bind layout from the frozen Session
        // manifest before any process snapshots the mount namespace. Start with no
        // binding and launch lazily on the first tool invocation.
        Self::new(
            Arc::new(SessionAgentLauncher::Namespace(sandbox)),
            factory,
            hand_bin.into(),
            idle_after,
            crate::deployment_config::ContainerHandResidency::AttachedExec,
            None,
            None,
        )
    }

    pub(super) async fn container(
        sandbox: Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        skills: Arc<ContainerSkillCache>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        idle_after: Duration,
        residency: crate::deployment_config::ContainerHandResidency,
    ) -> Result<Self, pc::SandboxError> {
        let launcher = Arc::new(SessionAgentLauncher::Container {
            sandbox: sandbox.clone(),
        });
        let hand_bin = hand_bin.into();
        let executor = Self::new(
            launcher,
            factory,
            hand_bin,
            idle_after,
            residency,
            None,
            Some((sandbox, skills)),
        );
        let binding = executor.launch().await?;
        *executor.lifecycle.binding.lock().await = Some(binding);
        Ok(executor)
    }

    fn new(
        launcher: Arc<SessionAgentLauncher>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        idle_after: Duration,
        residency: crate::deployment_config::ContainerHandResidency,
        binding: Option<HandBinding>,
        container_skills: Option<(
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            Arc<ContainerSkillCache>,
        )>,
    ) -> Self {
        let operation_scope = launcher.operation_scope();
        let (activity, observed_activity) = tokio::sync::watch::channel(0);
        let executor = Self {
            launcher,
            factory,
            hand_bin,
            operation_scope,
            lifecycle: Arc::new(HandLifecycle {
                binding: Mutex::new(binding),
                closed: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                activity,
            }),
            projection_update: Mutex::new(()),
            projection_updating: AtomicBool::new(false),
            idle_after,
            residency,
            container_skills,
        };
        if residency == crate::deployment_config::ContainerHandResidency::AttachedExec {
            executor.spawn_idle_hibernation(observed_activity);
        }
        executor
    }

    pub(super) fn launcher(&self) -> Arc<SessionAgentLauncher> {
        self.launcher.clone()
    }

    async fn launch(&self) -> Result<HandBinding, pc::SandboxError> {
        let started = std::time::Instant::now();
        if self.residency == crate::deployment_config::ContainerHandResidency::Resident {
            // Pod Running/Ready can become observable before PID 1 has bound its
            // listener. Retry only this pre-dispatch attachment boundary.
            let mut attempts = 0_u8;
            let channel = loop {
                attempts += 1;
                match self.launcher.open_resident_channel().await {
                    Ok(channel) => break channel,
                    Err(error) if attempts < 50 => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        tracing::debug!(attempts, error = %error, "resident Hand is not ready");
                    }
                    Err(error) => {
                        awaken_observability::record_hand_lifecycle(
                            "attach_resident",
                            "error",
                            started.elapsed(),
                        );
                        return Err(error);
                    }
                }
            };
            awaken_observability::record_hand_lifecycle("attach_resident", "ok", started.elapsed());
            return Ok(HandBinding {
                executor: self.factory.bind(
                    channel,
                    &self.operation_scope,
                    ToolRecoveryCapability::DurableRequest,
                ),
                process: None,
            });
        }

        let command = pc::Command {
            argv: vec![self.hand_bin.clone(), "hand".into(), "--stdio".into()],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        };
        let (process, channel) = match self.launcher.spawn_agent(command).await {
            Ok(binding) => binding,
            Err(error) => {
                awaken_observability::record_hand_lifecycle("launch", "error", started.elapsed());
                return Err(error);
            }
        };
        awaken_observability::add_live_hand(1);
        awaken_observability::record_hand_lifecycle("launch", "ok", started.elapsed());
        Ok(HandBinding {
            executor: self.factory.bind(
                channel,
                &self.operation_scope,
                ToolRecoveryCapability::NonRecoverable,
            ),
            process: Some(process),
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

    pub(crate) async fn begin_projection_update(
        &self,
    ) -> Result<HandProjectionUpdate<'_>, pc::SandboxError> {
        let update = self.projection_update.lock().await;
        self.projection_updating.store(true, Ordering::Release);
        if self.lifecycle.closed.load(Ordering::Acquire) {
            return Err(pc::SandboxError::new("Session hand binding is closed"));
        }
        if self
            .lifecycle
            .hibernate_if_current(None, "projection_hibernate")
            .await
            || self.lifecycle.binding.lock().await.is_none()
        {
            Ok(HandProjectionUpdate {
                hand: self,
                _update: update,
                committed: false,
            })
        } else {
            Err(pc::SandboxError::new(
                "failed to hibernate Session hand before projection update",
            ))
        }
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
        let result = self.launch().await;
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
impl ToolExecutor for SessionHandExecutor {
    fn recovery_capability(&self, _tool_id: &str) -> ToolRecoveryCapability {
        self.residency.recovery_capability()
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if self.lifecycle.closed.load(Ordering::Acquire)
            || self.projection_updating.load(Ordering::Acquire)
        {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed or its resource projection is updating".into(),
            ));
        }
        self.lifecycle.touch();
        let mut binding = self.lifecycle.binding.lock().await;
        if self.lifecycle.closed.load(Ordering::Acquire)
            || self.projection_updating.load(Ordering::Acquire)
        {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed or its resource projection is updating".into(),
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
            && let Some((sandbox, skills)) = &self.container_skills
            && let Err(error) = skills.refresh(sandbox.as_ref()).await
        {
            tracing::warn!(error = %error, "failed to refresh container skill catalog");
        }
        self.lifecycle.touch();
        result
    }
}

/// Exclusive fence for one live resource-projection replacement. Dropping an
/// uncommitted guard deliberately leaves the Hand unavailable: the physical
/// projection may be partial and only the existing pending-generation retry can
/// make it authoritative again.
pub(crate) struct HandProjectionUpdate<'a> {
    hand: &'a SessionHandExecutor,
    _update: tokio::sync::MutexGuard<'a, ()>,
    committed: bool,
}

impl HandProjectionUpdate<'_> {
    pub(crate) fn commit(mut self) {
        self.committed = true;
        self.hand
            .projection_updating
            .store(false, Ordering::Release);
        self.hand.lifecycle.touch();
    }
}

impl Drop for HandProjectionUpdate<'_> {
    fn drop(&mut self) {
        if !self.committed {
            tracing::warn!(
                session_environment = %self.hand.operation_scope,
                "resource projection update did not commit; keeping Session hand fenced"
            );
        }
    }
}
