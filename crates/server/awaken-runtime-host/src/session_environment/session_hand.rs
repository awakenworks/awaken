//! The single Hand lifecycle shared by every tool-transparent Session environment.
//!
//! Namespace and Container providers differ only in how they open a channel. Tool
//! dispatch, retry fencing, idle hibernation, and process ownership remain one
//! Runtime-Host responsibility.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
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

enum HandBindingState {
    Vacant,
    Starting,
    Ready(HandBinding),
    LaunchFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandBindingPhase {
    Vacant,
    Starting,
    Ready,
    LaunchFailed,
}

#[must_use]
const fn hand_binding_transition_admitted(
    current: HandBindingPhase,
    next: HandBindingPhase,
) -> bool {
    matches!(
        (current, next),
        (HandBindingPhase::Vacant, HandBindingPhase::Starting)
            | (HandBindingPhase::Starting, HandBindingPhase::Ready)
            | (HandBindingPhase::Starting, HandBindingPhase::LaunchFailed)
            | (HandBindingPhase::Ready, HandBindingPhase::Vacant)
            | (HandBindingPhase::LaunchFailed, HandBindingPhase::Vacant)
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum HandAvailability {
    Open,
    ProjectionUpdate,
    Fenced,
    Closed,
}

#[must_use]
const fn hand_availability_transition_admitted(
    current: HandAvailability,
    next: HandAvailability,
) -> bool {
    matches!(
        (current, next),
        (HandAvailability::Open, HandAvailability::ProjectionUpdate)
            | (HandAvailability::Fenced, HandAvailability::ProjectionUpdate)
            | (HandAvailability::ProjectionUpdate, HandAvailability::Open)
            | (HandAvailability::Open, HandAvailability::Fenced)
            | (HandAvailability::ProjectionUpdate, HandAvailability::Fenced)
            | (HandAvailability::Open, HandAvailability::Closed)
            | (HandAvailability::ProjectionUpdate, HandAvailability::Closed)
            | (HandAvailability::Fenced, HandAvailability::Closed)
            | (HandAvailability::Closed, HandAvailability::Closed)
    )
}

impl HandAvailability {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Open,
            1 => Self::ProjectionUpdate,
            2 => Self::Fenced,
            3 => Self::Closed,
            _ => Self::Fenced,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetireOutcome {
    Retired,
    AlreadyVacant,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetireError {
    ReapFailed,
}

struct HandLifecycle {
    binding: Mutex<HandBindingState>,
    binding_changed: tokio::sync::Notify,
    availability: AtomicU8,
    generation: AtomicU64,
    activity: tokio::sync::watch::Sender<u64>,
}

impl HandLifecycle {
    fn availability(&self) -> HandAvailability {
        HandAvailability::from_raw(self.availability.load(Ordering::Acquire))
    }

    fn is_open(&self) -> bool {
        self.availability() == HandAvailability::Open
    }

    fn begin_projection_update(&self) -> bool {
        loop {
            let current = self.availability();
            if !hand_availability_transition_admitted(current, HandAvailability::ProjectionUpdate) {
                return false;
            }
            if self
                .availability
                .compare_exchange(
                    current as u8,
                    HandAvailability::ProjectionUpdate as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    fn commit_projection_update(&self) {
        debug_assert!(hand_availability_transition_admitted(
            HandAvailability::ProjectionUpdate,
            HandAvailability::Open,
        ));
        let _ = self.availability.compare_exchange(
            HandAvailability::ProjectionUpdate as u8,
            HandAvailability::Open as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn fence(&self) {
        loop {
            let current = self.availability();
            if matches!(current, HandAvailability::Fenced | HandAvailability::Closed) {
                return;
            }
            debug_assert!(hand_availability_transition_admitted(
                current,
                HandAvailability::Fenced,
            ));
            if self
                .availability
                .compare_exchange(
                    current as u8,
                    HandAvailability::Fenced as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.touch();
                return;
            }
        }
    }

    fn touch(&self) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.activity.send(generation);
    }

    fn close(&self) {
        if self
            .availability
            .swap(HandAvailability::Closed as u8, Ordering::AcqRel)
            != HandAvailability::Closed as u8
        {
            self.touch();
        }
    }

    async fn retire_binding(
        &self,
        binding: &mut HandBindingState,
        event: &'static str,
    ) -> Result<RetireOutcome, RetireError> {
        let current = match binding {
            HandBindingState::Ready(current) => current,
            HandBindingState::Vacant | HandBindingState::LaunchFailed(_) => {
                *binding = HandBindingState::Vacant;
                return Ok(RetireOutcome::AlreadyVacant);
            }
            HandBindingState::Starting => return Ok(RetireOutcome::Stale),
        };
        // Keep the binding installed while the external reap is in flight. If
        // this Future is cancelled, the mutex guard is dropped but the owner is
        // still tracked; a later retry can resume retirement and can never infer
        // Vacant and launch a second process from an unknown outcome.
        if stop_hand_binding(current, event).await {
            debug_assert!(hand_binding_transition_admitted(
                HandBindingPhase::Ready,
                HandBindingPhase::Vacant,
            ));
            *binding = HandBindingState::Vacant;
            self.binding_changed.notify_waiters();
            Ok(RetireOutcome::Retired)
        } else {
            // The process outcome is unknown. Starting another Hand could
            // violate the one-owner invariant, so fail closed.
            self.fence();
            Err(RetireError::ReapFailed)
        }
    }

    async fn hibernate_if_current(
        &self,
        expected_generation: Option<u64>,
        event: &'static str,
    ) -> Result<RetireOutcome, RetireError> {
        loop {
            let notified = self.binding_changed.notified();
            let mut binding = self.binding.lock().await;
            if expected_generation
                .is_some_and(|expected| self.generation.load(Ordering::Acquire) != expected)
            {
                return Ok(RetireOutcome::Stale);
            }
            if matches!(*binding, HandBindingState::Starting) {
                drop(binding);
                notified.await;
                continue;
            }
            return self.retire_binding(&mut binding, event).await;
        }
    }
}

const HAND_STOP_GRACE: Duration = Duration::from_secs(2);

async fn stop_hand_binding(binding: &HandBinding, event: &'static str) -> bool {
    let Some(process) = binding.process.as_ref() else {
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

async fn launch_hand(
    launcher: Arc<SessionAgentLauncher>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    mode: HandMode,
) -> Result<HandBinding, pc::SandboxError> {
    let started = std::time::Instant::now();
    if mode == HandMode::Resident {
        // Pod Running/Ready can become observable before PID 1 has bound its
        // listener. Retry only this pre-dispatch attachment boundary.
        let mut attempts = 0_u8;
        let channel = loop {
            attempts += 1;
            match launcher.open_resident_channel().await {
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
            executor: factory.bind(
                channel,
                &operation_scope,
                ToolRecoveryCapability::DurableRequest,
            ),
            process: None,
        });
    }

    let command = pc::Command {
        argv: vec![hand_bin, "hand".into(), "--stdio".into()],
        cwd: "/workspace".into(),
        env: Vec::new(),
        stdio: pc::Stdio::Piped,
    };
    let (process, channel) = match launcher.spawn_agent(command).await {
        Ok(binding) => binding,
        Err(error) => {
            awaken_observability::record_hand_lifecycle("launch", "error", started.elapsed());
            return Err(error);
        }
    };
    awaken_observability::add_live_hand(1);
    awaken_observability::record_hand_lifecycle("launch", "ok", started.elapsed());
    Ok(HandBinding {
        executor: factory.bind(
            channel,
            &operation_scope,
            ToolRecoveryCapability::NonRecoverable,
        ),
        process: Some(process),
    })
}

/// The one Session-Environment-owned Hand binding.
pub(crate) struct SessionHandExecutor {
    launcher: Arc<SessionAgentLauncher>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    lifecycle: Arc<HandLifecycle>,
    projection_update: Mutex<()>,
    idle_after: Duration,
    mode: HandMode,
    container_skills: Option<(
        Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
        Arc<ContainerSkillCache>,
    )>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandMode {
    AttachedExec,
    Resident,
}

impl HandMode {
    fn from_residency(residency: crate::deployment_config::ContainerHandResidency) -> Self {
        match residency {
            crate::deployment_config::ContainerHandResidency::AttachedExec => Self::AttachedExec,
            crate::deployment_config::ContainerHandResidency::Resident => Self::Resident,
        }
    }

    const fn recovery_capability(self) -> ToolRecoveryCapability {
        match self {
            Self::AttachedExec => ToolRecoveryCapability::NonRecoverable,
            Self::Resident => ToolRecoveryCapability::DurableRequest,
        }
    }

    const fn hibernates_when_idle(self) -> bool {
        matches!(self, Self::AttachedExec)
    }
}

impl SessionHandExecutor {
    #[cfg(test)]
    pub(super) async fn has_tracked_binding(&self) -> bool {
        matches!(
            *self.lifecycle.binding.lock().await,
            HandBindingState::Ready(_)
        )
    }

    #[cfg(test)]
    pub(super) fn projection_is_updating(&self) -> bool {
        self.lifecycle.availability() == HandAvailability::ProjectionUpdate
    }

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
            Some((sandbox, skills)),
        );
        executor.await_ready_binding().await?;
        Ok(executor)
    }

    fn new(
        launcher: Arc<SessionAgentLauncher>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        idle_after: Duration,
        residency: crate::deployment_config::ContainerHandResidency,
        container_skills: Option<(
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            Arc<ContainerSkillCache>,
        )>,
    ) -> Self {
        let operation_scope = launcher.operation_scope();
        let mode = HandMode::from_residency(residency);
        let (activity, observed_activity) = tokio::sync::watch::channel(0);
        let executor = Self {
            launcher,
            factory,
            hand_bin,
            operation_scope,
            lifecycle: Arc::new(HandLifecycle {
                binding: Mutex::new(HandBindingState::Vacant),
                binding_changed: tokio::sync::Notify::new(),
                availability: AtomicU8::new(HandAvailability::Open as u8),
                generation: AtomicU64::new(0),
                activity,
            }),
            projection_update: Mutex::new(()),
            idle_after,
            mode,
            container_skills,
        };
        if mode.hibernates_when_idle() {
            executor.spawn_idle_hibernation(observed_activity);
        }
        executor
    }

    pub(super) fn launcher(&self) -> Arc<SessionAgentLauncher> {
        self.launcher.clone()
    }

    fn start_owned_launch(&self) {
        let launcher = self.launcher.clone();
        let factory = self.factory.clone();
        let hand_bin = self.hand_bin.clone();
        let operation_scope = self.operation_scope.clone();
        let mode = self.mode;
        let lifecycle = self.lifecycle.clone();
        tokio::spawn(async move {
            let result = launch_hand(launcher, factory, hand_bin, operation_scope, mode).await;
            let mut state = lifecycle.binding.lock().await;
            match result {
                Ok(binding) => {
                    debug_assert!(hand_binding_transition_admitted(
                        HandBindingPhase::Starting,
                        HandBindingPhase::Ready,
                    ));
                    *state = HandBindingState::Ready(binding);
                    // Launch is owned by this task, not by the request Future.
                    // If the environment closed or fenced while spawning, keep
                    // the process tracked until the same owner reaps it.
                    if !lifecycle.is_open() {
                        let _ = lifecycle
                            .retire_binding(&mut state, "cancelled_launch_reap")
                            .await;
                    }
                }
                Err(error) => {
                    debug_assert!(hand_binding_transition_admitted(
                        HandBindingPhase::Starting,
                        HandBindingPhase::LaunchFailed,
                    ));
                    *state = HandBindingState::LaunchFailed(error.to_string());
                }
            }
            lifecycle.binding_changed.notify_waiters();
        });
    }

    async fn await_ready_binding(&self) -> Result<(), pc::SandboxError> {
        loop {
            let notified = self.lifecycle.binding_changed.notified();
            let mut state = self.lifecycle.binding.lock().await;
            match &mut *state {
                HandBindingState::Ready(_) => return Ok(()),
                HandBindingState::Vacant => {
                    debug_assert!(hand_binding_transition_admitted(
                        HandBindingPhase::Vacant,
                        HandBindingPhase::Starting,
                    ));
                    *state = HandBindingState::Starting;
                    self.start_owned_launch();
                }
                HandBindingState::Starting => {}
                HandBindingState::LaunchFailed(error) => {
                    let error = std::mem::take(error);
                    debug_assert!(hand_binding_transition_admitted(
                        HandBindingPhase::LaunchFailed,
                        HandBindingPhase::Vacant,
                    ));
                    *state = HandBindingState::Vacant;
                    return Err(pc::SandboxError::new(error));
                }
            }
            drop(state);
            notified.await;
        }
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
                            .is_none_or(|lifecycle| {
                                matches!(
                                    lifecycle.availability(),
                                    HandAvailability::Fenced | HandAvailability::Closed
                                )
                            })
                        {
                            break;
                        }
                    }
                    () = tokio::time::sleep(idle_after) => {
                        let Some(lifecycle) = lifecycle.upgrade() else {
                            break;
                        };
                        if matches!(
                            lifecycle.availability(),
                            HandAvailability::Fenced | HandAvailability::Closed
                        ) {
                            break;
                        }
                        if matches!(
                            lifecycle
                                .hibernate_if_current(Some(generation), "idle_hibernate")
                                .await,
                            Ok(RetireOutcome::Retired)
                        ) {
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
        if !self.lifecycle.begin_projection_update() {
            return Err(pc::SandboxError::new(
                "Session hand binding is unavailable for projection update",
            ));
        }
        match self
            .lifecycle
            .hibernate_if_current(None, "projection_hibernate")
            .await
        {
            Ok(RetireOutcome::Retired | RetireOutcome::AlreadyVacant) => Ok(HandProjectionUpdate {
                hand: self,
                _update: update,
                committed: false,
            }),
            Ok(RetireOutcome::Stale) => {
                self.lifecycle.fence();
                Err(pc::SandboxError::new(
                    "Session hand activity changed during projection update",
                ))
            }
            Err(RetireError::ReapFailed) => {
                self.lifecycle.fence();
                Err(pc::SandboxError::new(
                    "failed to reap Session hand before projection update",
                ))
            }
        }
    }

    pub(super) async fn stop(&self) {
        self.lifecycle.close();
        let _ = self
            .lifecycle
            .hibernate_if_current(None, "terminal_stop")
            .await;
    }

    async fn replacement(&self) -> Result<(), ToolError> {
        let started = std::time::Instant::now();
        let result = self.await_ready_binding().await;
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
        self.mode.recovery_capability()
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if !self.lifecycle.is_open() {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed or its resource projection is updating".into(),
            ));
        }
        self.lifecycle.touch();
        self.replacement().await?;
        let mut binding = self.lifecycle.binding.lock().await;
        if !self.lifecycle.is_open() {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed or its resource projection is updating".into(),
            ));
        }
        let HandBindingState::Ready(current) = &*binding else {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand launch did not install an owned binding".into(),
            ));
        };
        let result = current.executor.invoke(call).await;
        let result = if matches!(result, Err(ToolError::UnavailableBeforeDispatch(_))) {
            tracing::warn!(
                session_environment = %self.operation_scope,
                tool_id = %call.tool_id,
                "reacquiring expired Session hand before tool dispatch"
            );
            if self
                .lifecycle
                .retire_binding(&mut binding, "expired_reap")
                .await
                .is_err()
            {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "failed to reap expired Session hand; environment must be reconstructed".into(),
                ));
            }
            if !self.lifecycle.is_open() {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "Session hand binding closed during reacquisition".into(),
                ));
            }
            drop(binding);
            self.replacement().await?;
            binding = self.lifecycle.binding.lock().await;
            if !self.lifecycle.is_open() {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "Session hand binding closed during reacquisition".into(),
                ));
            }
            let HandBindingState::Ready(current) = &*binding else {
                return Err(ToolError::UnavailableBeforeDispatch(
                    "replacement Session hand was not installed".into(),
                ));
            };
            current.executor.invoke(call).await
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

impl Drop for SessionHandExecutor {
    fn drop(&mut self) {
        self.lifecycle.close();
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
        self.hand.lifecycle.commit_projection_update();
        self.hand.lifecycle.touch();
    }
}

impl Drop for HandProjectionUpdate<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.hand.lifecycle.fence();
            tracing::warn!(
                session_environment = %self.hand.operation_scope,
                "resource projection update did not commit; keeping Session hand fenced"
            );
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn hand_availability_has_only_explicit_recovery_and_terminal_transitions() {
        let current = match kani::any::<u8>() % 4 {
            0 => HandAvailability::Open,
            1 => HandAvailability::ProjectionUpdate,
            2 => HandAvailability::Fenced,
            _ => HandAvailability::Closed,
        };
        assert!(!hand_availability_transition_admitted(
            HandAvailability::Closed,
            HandAvailability::Open,
        ));
        assert!(!hand_availability_transition_admitted(
            HandAvailability::Fenced,
            HandAvailability::Open,
        ));
        if hand_availability_transition_admitted(current, HandAvailability::Open) {
            assert_eq!(current, HandAvailability::ProjectionUpdate);
        }
    }

    #[kani::proof]
    fn hand_binding_can_only_become_ready_through_tracked_starting() {
        let current = match kani::any::<u8>() % 4 {
            0 => HandBindingPhase::Vacant,
            1 => HandBindingPhase::Starting,
            2 => HandBindingPhase::Ready,
            _ => HandBindingPhase::LaunchFailed,
        };
        assert!(!hand_binding_transition_admitted(
            HandBindingPhase::Vacant,
            HandBindingPhase::Ready,
        ));
        if hand_binding_transition_admitted(current, HandBindingPhase::Ready) {
            assert_eq!(current, HandBindingPhase::Starting);
        }
    }
}
