// @formal entrypoint remote_tool.hand_availability_recovery_and_terminal_transitions_are_closed
// @formal entrypoint remote_tool.hand_generation_exhaustion_is_terminal_and_never_wraps
// @formal entrypoint remote_tool.hand_launch_must_cross_a_tracked_starting_phase_before_ready
//! The single Hand lifecycle shared by every Hand-equipped Session environment.
//!
//! TrustedHost, Namespace and Container differ in how they open a channel. Tool
//! dispatch, retry fencing, idle hibernation, and process ownership remain one
//! Runtime-Host responsibility.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::Sandbox as _;
use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox_local::{LocalSandbox, NamespaceSandbox};
use awaken_tool_contract::tool::{ToolError, ToolExecutor, ToolOutput, ToolRecoveryCapability};
use tokio::sync::Mutex;

use super::container_skills::ContainerSkillCache;

pub(crate) use awaken_session_environment_contract::HandExecutorFactory;

pub(super) enum SessionAgentLauncher {
    Workdir(Arc<LocalSandbox>),
    Namespace(Arc<NamespaceSandbox>),
    Container {
        sandbox: Arc<dyn awaken_provisioning_contract::SessionSandbox>,
    },
}

impl SessionAgentLauncher {
    async fn execution_domain(
        &self,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, pc::SandboxError> {
        use awaken_tool_contract::tool_batch::ToolExecutionDomain;
        match self {
            Self::Container { sandbox } => Ok(sandbox.owned_process_binding().await?.map_or(
                ToolExecutionDomain::Opaque {
                    resource_origin: None,
                },
                |execution_ref| ToolExecutionDomain::ControlledProcess {
                    provider_ref: "kubernetes-sandbox".into(),
                    execution_ref,
                    resource_origin: None,
                },
            )),
            Self::Workdir(_) | Self::Namespace(_) => Ok(ToolExecutionDomain::Opaque {
                resource_origin: None,
            }),
        }
    }

    fn operation_scope(&self) -> String {
        match self {
            Self::Workdir(sandbox) => sandbox.handle().sandbox_id,
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
            Box<dyn awaken_agent_channel::AgentChannel>,
        ),
        pc::SandboxError,
    > {
        match self {
            Self::Workdir(sandbox) => sandbox.spawn_agent(command).await,
            Self::Namespace(sandbox) => sandbox.spawn_agent(command).await,
            Self::Container { sandbox } => sandbox
                .spawn_agent_process(command)
                .await
                .map(|process| (process.process, process.channel)),
        }
    }

    async fn open_resident_channel(
        &self,
    ) -> Result<Box<dyn awaken_agent_channel::AgentChannel>, pc::SandboxError> {
        match self {
            Self::Container { sandbox } => sandbox.open_agent_channel().await,
            Self::Workdir(_) | Self::Namespace(_) => Err(pc::SandboxError::unsupported(
                "host-backed Session environments do not own a resident Hand",
            )),
        }
    }
}

struct HandBinding {
    process: Option<Box<dyn pc::ProcessHandle>>,
    executor: Arc<dyn ToolExecutor>,
    recovery: Option<Arc<dyn awaken_session_environment_contract::HandRequestRecovery>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HandGenerationStep {
    next: u64,
    exhausted: bool,
}

/// Advance the activity fence without ever wrapping back to an older value.
///
/// `u64::MAX` is a terminal sentinel rather than a usable generation. Moving
/// into it exhausts the lifecycle and must fence dispatch permanently.
#[must_use]
const fn hand_generation_step(current: u64) -> HandGenerationStep {
    if current >= u64::MAX - 1 {
        HandGenerationStep {
            next: u64::MAX,
            exhausted: true,
        }
    } else {
        HandGenerationStep {
            next: current + 1,
            exhausted: false,
        }
    }
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
    Retired(HandStopProof),
    AlreadyVacant,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetireError {
    ReapFailed,
}

pub(crate) use awaken_session_environment_contract::HandStopProof;

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
            && self.generation.load(Ordering::Acquire) != u64::MAX
    }

    fn begin_projection_update(&self) -> bool {
        loop {
            if self.generation.load(Ordering::Acquire) == u64::MAX {
                self.fence_without_activity();
                return false;
            }
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
                // A concurrent touch may have consumed the terminal sentinel
                // after the pre-check. Keep the projection fenced in that race.
                if self.generation.load(Ordering::Acquire) == u64::MAX {
                    self.fence_without_activity();
                    return false;
                }
                return true;
            }
        }
    }

    fn commit_projection_update(&self) {
        if self.generation.load(Ordering::Acquire) == u64::MAX {
            self.fence_without_activity();
            return;
        }
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

    fn fence_without_activity(&self) -> bool {
        loop {
            let current = self.availability();
            if matches!(current, HandAvailability::Fenced | HandAvailability::Closed) {
                return false;
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
                return true;
            }
        }
    }

    fn fence(&self) {
        if self.fence_without_activity() {
            let _ = self.touch();
        }
    }

    /// Publish a fresh activity generation. `false` means the finite identity
    /// space is exhausted and the lifecycle has been fenced fail-closed.
    fn touch(&self) -> bool {
        loop {
            let current = self.generation.load(Ordering::Acquire);
            let step = hand_generation_step(current);
            if step.next == current {
                self.fence_without_activity();
                return false;
            }
            if self
                .generation
                .compare_exchange(current, step.next, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if step.exhausted {
                self.fence_without_activity();
            }
            let _ = self.activity.send(step.next);
            return !step.exhausted;
        }
    }

    fn close(&self) {
        if self
            .availability
            .swap(HandAvailability::Closed as u8, Ordering::AcqRel)
            != HandAvailability::Closed as u8
        {
            let _ = self.touch();
            // Wake active invocation observers even at the exhausted generation.
            self.activity
                .send_replace(self.generation.load(Ordering::Acquire));
        }
    }

    fn clear_binding(&self, binding: &mut HandBindingState) {
        *binding = HandBindingState::Vacant;
        self.binding_changed.notify_waiters();
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
        if let Ok(proof) = stop_hand_binding(current, event).await {
            debug_assert!(hand_binding_transition_admitted(
                HandBindingPhase::Ready,
                HandBindingPhase::Vacant,
            ));
            self.clear_binding(binding);
            Ok(RetireOutcome::Retired(proof))
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

async fn stop_hand_binding(
    binding: &HandBinding,
    event: &'static str,
) -> Result<HandStopProof, RetireError> {
    let Some(process) = binding.process.as_ref() else {
        // A resident Hand belongs to the Session Pod. Dropping the channel must
        // never terminate that workload.
        return Ok(HandStopProof::ResidentProcessRetained);
    };
    let started = awaken_time::mono::Stopwatch::start();
    if let Err(error) =
        awaken_run_executor_acp::Supervisor::reap(process.as_ref(), HAND_STOP_GRACE).await
    {
        awaken_observability::record_hand_lifecycle(event, "error", started.elapsed());
        tracing::warn!(
            process = %process.id(),
            error = %error,
            "failed to reap Session hand within the bounded signal ladder"
        );
        Err(RetireError::ReapFailed)
    } else {
        awaken_observability::add_live_hand(-1);
        awaken_observability::record_hand_lifecycle(event, "ok", started.elapsed());
        Ok(HandStopProof::AttachedProcessReaped)
    }
}

fn hand_launch_environment(workdir: bool, host_path: Option<String>) -> Vec<pc::EnvVar> {
    // TrustedHost Hand runs native tools after env_clear. Admit only the
    // operator's tool lookup path; Namespace/Container use their own image.
    if !workdir {
        return Vec::new();
    }
    host_path
        .map(|value| pc::EnvVar {
            name: "PATH".into(),
            value: pc::EnvValue::Inline { value },
            visibility: pc::EnvVisibility::Process,
        })
        .into_iter()
        .collect()
}

async fn launch_hand(
    launcher: Arc<SessionAgentLauncher>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    mode: HandMode,
) -> Result<HandBinding, pc::SandboxError> {
    let started = awaken_time::mono::Stopwatch::start();
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
        let bound = factory.bind(
            channel,
            &operation_scope,
            ToolRecoveryCapability::DurableRequest,
        );
        return Ok(HandBinding {
            executor: bound.executor,
            recovery: bound.recovery,
            process: None,
        });
    }

    let mut argv = vec![hand_bin, "hand".into(), "--stdio".into()];
    if let SessionAgentLauncher::Workdir(sandbox) = launcher.as_ref() {
        // The same provider-resolved paths serve native file tools and ACP.
        // These are CLI argv, not shell interpolation or model-controlled roots.
        argv.extend([
            "--mount-root".into(),
            sandbox
                .workspace_path()
                .join("mnt")
                .to_string_lossy()
                .into_owned(),
            "--workspace-root".into(),
            sandbox
                .workspace_path()
                .join(pc::WorkspaceLayout::ROOT.trim_start_matches('/'))
                .to_string_lossy()
                .into_owned(),
        ]);
    }
    if matches!(launcher.as_ref(), SessionAgentLauncher::Namespace(_)) {
        // The provider projects this root for Seatbelt. Container images keep
        // their existing CLI contract and already expose the default /mnt.
        argv.extend(["--mount-root".into(), "/mnt".into()]);
    }
    let command = pc::Command {
        argv,
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: hand_launch_environment(
            matches!(launcher.as_ref(), SessionAgentLauncher::Workdir(_)),
            std::env::var("PATH").ok(),
        ),
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
    let bound = factory.bind(
        channel,
        &operation_scope,
        ToolRecoveryCapability::NonRecoverable,
    );
    Ok(HandBinding {
        executor: bound.executor,
        recovery: bound.recovery,
        process: Some(process),
    })
}

/// The one Session-Environment-owned Hand binding.
/// The one Hand lifecycle bound to a Hand-equipped environment. Opaque:
/// consumers reach its tools through the environment's roles.
pub struct SessionHandExecutor {
    launcher: Arc<SessionAgentLauncher>,
    factory: Arc<dyn HandExecutorFactory>,
    hand_bin: String,
    operation_scope: String,
    lifecycle: Arc<HandLifecycle>,
    projection_update: Mutex<()>,
    idle_after: Duration,
    mode: HandMode,
    container_skills: Option<(
        Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        Arc<ContainerSkillCache>,
    )>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandMode {
    AttachedExec,
    Resident,
}

impl HandMode {
    fn from_residency(
        residency: awaken_session_environment_contract::ContainerHandResidency,
    ) -> Self {
        match residency {
            awaken_session_environment_contract::ContainerHandResidency::AttachedExec => {
                Self::AttachedExec
            }
            awaken_session_environment_contract::ContainerHandResidency::Resident => Self::Resident,
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
    pub(super) async fn contained_execution_domain(
        &self,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, pc::SandboxError> {
        self.launcher.execution_domain().await
    }

    pub(super) fn workdir(
        sandbox: Arc<LocalSandbox>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        idle_after: Duration,
    ) -> Self {
        Self::new(
            Arc::new(SessionAgentLauncher::Workdir(sandbox)),
            factory,
            hand_bin.into(),
            idle_after,
            awaken_session_environment_contract::ContainerHandResidency::AttachedExec,
            None,
        )
    }
    /// Exact in-sandbox execution binary selected by deployment configuration.
    /// Internal container effects must reuse this path so custom images have
    /// one capability contract instead of a second hard-coded binary location.
    pub(super) fn binary_path(&self) -> &str {
        &self.hand_bin
    }

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
            awaken_session_environment_contract::ContainerHandResidency::AttachedExec,
            None,
        )
    }

    pub(super) fn container(
        sandbox: Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        skills: Arc<ContainerSkillCache>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: impl Into<String>,
        idle_after: Duration,
        residency: awaken_session_environment_contract::ContainerHandResidency,
    ) -> Self {
        let launcher = Arc::new(SessionAgentLauncher::Container {
            sandbox: sandbox.clone(),
        });
        let hand_bin = hand_bin.into();
        Self::new(
            launcher,
            factory,
            hand_bin,
            idle_after,
            residency,
            Some((sandbox, skills)),
        )
    }

    fn new(
        launcher: Arc<SessionAgentLauncher>,
        factory: Arc<dyn HandExecutorFactory>,
        hand_bin: String,
        idle_after: Duration,
        residency: awaken_session_environment_contract::ContainerHandResidency,
        container_skills: Option<(
            Arc<dyn awaken_provisioning_contract::SessionSandbox>,
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
                    return Err(pc::SandboxError::backend("await_ready_binding", error));
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
                            Ok(RetireOutcome::Retired(_))
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
            return Err(pc::SandboxError::not_found(
                "Session hand binding is unavailable for projection update",
            ));
        }
        match self
            .lifecycle
            .hibernate_if_current(None, "projection_hibernate")
            .await
        {
            Ok(RetireOutcome::Retired(_) | RetireOutcome::AlreadyVacant) => {
                Ok(HandProjectionUpdate {
                    hand: self,
                    _update: update,
                    committed: false,
                })
            }
            Ok(RetireOutcome::Stale) => {
                self.lifecycle.fence();
                Err(pc::SandboxError::conflict(
                    "Session hand activity changed during projection update",
                ))
            }
            Err(RetireError::ReapFailed) => {
                self.lifecycle.fence();
                Err(pc::SandboxError::backend(
                    "begin_projection_update",
                    "failed to reap Session hand before projection update",
                ))
            }
        }
    }

    async fn finish_stop(&self, event: &'static str) -> Result<HandStopProof, pc::SandboxError> {
        match self.lifecycle.hibernate_if_current(None, event).await {
            Ok(RetireOutcome::Retired(proof)) => Ok(proof),
            Ok(RetireOutcome::AlreadyVacant) => Ok(HandStopProof::NoBinding),
            Ok(RetireOutcome::Stale) => Err(pc::SandboxError::conflict(
                "Session hand binding changed while stop was quiescing",
            )),
            Err(RetireError::ReapFailed) => Err(pc::SandboxError::backend(
                "finish_stop",
                "failed to reap Session hand during stop",
            )),
        }
    }

    /// Fence checkpoint admission and return retry-stable evidence. A resident
    /// Hand has no Worker-owned process handle, so its binding remains tracked:
    /// neither the first attempt nor a retry may turn Retained into Vacant.
    pub(super) async fn quiesce(&self) -> Result<HandStopProof, pc::SandboxError> {
        self.lifecycle.close();
        if self.mode == HandMode::Resident {
            return Ok(HandStopProof::ResidentProcessRetained);
        }
        self.finish_stop("checkpoint_quiesce").await
    }

    pub(super) async fn stop(&self) -> Result<HandStopProof, pc::SandboxError> {
        self.lifecycle.close();
        if self.mode == HandMode::Resident {
            let binding = self.lifecycle.binding.lock().await;
            if let HandBindingState::Ready(current) = &*binding
                && let Some(recovery) = &current.recovery
            {
                let cancel = async {
                    let channel = self.launcher.open_resident_channel().await?;
                    recovery
                        .recover(channel, true)
                        .await
                        .map_err(|error| pc::SandboxError::backend("hand_cancel", error))
                };
                tokio::time::timeout(HAND_STOP_GRACE, cancel)
                    .await
                    .map_err(|_| {
                        pc::SandboxError::backend(
                            "hand_cancel",
                            "Hand cancellation observation exceeded its stop budget",
                        )
                    })??;
            }
            // Preserve the same resident binding on success as on error or
            // timeout. Vacant would discard its unresolved request and turn a
            // later stop into NoBinding although the Pod-owned process lives.
            return Ok(HandStopProof::ResidentProcessRetained);
        }
        self.finish_stop("terminal_stop").await
    }

    /// Forget only the process-local reference after the owning provider's
    /// disposal completes. This issues no Hand I/O and grants no stop receipt.
    pub(super) async fn forget_after_provider_disposal(&self) {
        self.lifecycle.close();
        let mut binding = self.lifecycle.binding.lock().await;
        if matches!(*binding, HandBindingState::Ready(_)) {
            self.lifecycle.clear_binding(&mut binding);
        }
        // A launch still in progress remains owned by its existing completion
        // task, whose closed-lifecycle branch performs its own bounded reap.
    }

    async fn invoke_bound(
        &self,
        executor: &Arc<dyn ToolExecutor>,
        call: &ToolCall,
    ) -> Result<ToolOutput, ToolError> {
        let mut activity = self.lifecycle.activity.subscribe();
        let invocation = executor.invoke(call);
        tokio::pin!(invocation);
        loop {
            if self.lifecycle.availability() == HandAvailability::Closed {
                return Err(ToolError::Indeterminate(call.tool_id.clone()));
            }
            tokio::select! {
                result = &mut invocation => return if self.lifecycle.availability() == HandAvailability::Closed {
                    Err(ToolError::Indeterminate(call.tool_id.clone()))
                } else { result },
                changed = activity.changed() => {
                    if changed.is_err() { return Err(ToolError::Indeterminate(call.tool_id.clone())); }
                }
            }
        }
    }

    async fn replacement(&self) -> Result<(), ToolError> {
        let started = awaken_time::mono::Stopwatch::start();
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
    async fn execution_domain(
        &self,
        _call: &ToolCall,
    ) -> Result<awaken_tool_contract::tool_batch::ToolExecutionDomain, ToolError> {
        self.contained_execution_domain()
            .await
            .map_err(|error| ToolError::UnavailableBeforeDispatch(error.to_string()))
    }

    fn recovery_capability(&self, _tool_id: &str) -> ToolRecoveryCapability {
        self.mode.recovery_capability()
    }

    async fn invoke(&self, call: &ToolCall) -> Result<ToolOutput, ToolError> {
        if !self.lifecycle.is_open() {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand binding is closed or its resource projection is updating".into(),
            ));
        }
        if !self.lifecycle.touch() {
            return Err(ToolError::UnavailableBeforeDispatch(
                "Session hand activity generation is exhausted; environment must be reconstructed"
                    .into(),
            ));
        }
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
        let result = self.invoke_bound(&current.executor, call).await;
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
            self.invoke_bound(&current.executor, call).await
        } else {
            result
        };
        if awaken_session_configuration_contract::is_controlled_modification_member(
            call.tool_id.as_str(),
        ) && let Some((sandbox, skills)) = &self.container_skills
            && let Err(error) = skills.refresh(sandbox.as_ref()).await
        {
            let detail =
                format!("refresh container Skill catalog after workspace mutation: {error}");
            // The mutation has already crossed the Hand boundary. Refresh
            // atomically removed the last-good catalog; close the one Session
            // lifecycle before returning the Runtime-only fatal signal,
            // because reacquiring or replaying could duplicate the effect.
            self.lifecycle.close();
            // Projection failure must not hide stronger physical uncertainty
            // already reported by the Hand. Neither class permits reacquisition.
            if matches!(result, Err(ToolError::Indeterminate(_))) {
                return result;
            }
            return Err(ToolError::StateInvalidatedAfterDispatch(detail));
        }
        let _ = self.lifecycle.touch();
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
        let _ = self.hand.lifecycle.touch();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{SandboxEnvironmentAccess as _, UnusedHandExecutorFactory};
    use std::sync::Barrier;

    #[test]
    fn hand_tool_lookup_is_local_and_preserves_only_the_exact_host_path() {
        // H1 Workdir vs image-owned substrate x H2 absent/empty/nonempty PATH.
        // H1+present => one process-only inline PATH, byte-for-byte; all other
        // rows => no host environment. Empty is an explicit operator value.
        // The Native host fixture separately executes Bash, Git and resources
        // through the installed Hand, proving this reaches the real child.
        for workdir in [false, true] {
            for path in [
                None,
                Some(""),
                Some("C:\\tools with spaces;D:\\工具;quote'\";$literal"),
            ] {
                let env = hand_launch_environment(workdir, path.map(str::to_owned));
                if workdir && let Some(path) = path {
                    assert_eq!(
                        env,
                        vec![pc::EnvVar {
                            name: "PATH".into(),
                            value: pc::EnvValue::Inline { value: path.into() },
                            visibility: pc::EnvVisibility::Process,
                        }]
                    );
                } else {
                    assert!(env.is_empty());
                }
            }
        }
    }

    #[tokio::test]
    async fn workdir_unknown_hand_reap_retains_resources_until_exact_owner_is_reaped() {
        // Causes: W1 Workdir/Namespace disposal has no OS teardown; W2 tracked Hand
        // cannot be reaped; W3 same retained owner later proves exit.
        // Effects: E1 quiescence/disposal fail without deleting resource bytes;
        // E2 owner stays tracked, dispatch stays closed; E3 retry after W3
        // completes disposal. Rules W1+W2=>E1+E2, W1+W3=>E3. These injected
        // process receipts test lifecycle wiring, NOT Linux process-tree proof.
        // Linux requires both filesystem adapters; other platforms exercise the
        // portable Workdir row here and their OS-specific Namespace gate separately.
        use super::super::tests::{DoneProcess, FakeHandExecutor, UnreapableProcess, spec};
        use super::super::{SandboxEnvironment, SandboxEnvironmentProvider};
        for namespace in [false, true]
            .into_iter()
            .filter(|namespace| !namespace || cfg!(target_os = "linux"))
        {
            let base = tempfile::tempdir().unwrap();
            let provider = if namespace {
                SandboxEnvironmentProvider::namespace_with_agent_stderr(
                    base.path(),
                    false,
                    Arc::new(UnusedHandExecutorFactory),
                    "unused-hand",
                    Duration::ZERO,
                )
                .into_session_provider()
            } else {
                let mut provider =
                    SandboxEnvironmentProvider::trusted_host_workdir_with_agent_stderr(
                        base.path(),
                        false,
                    );
                provider.install_workdir_hand(
                    Arc::new(UnusedHandExecutorFactory),
                    "unused-hand".into(),
                    Duration::ZERO,
                );
                provider.into_session_provider()
            };
            let environment = provider.create(&spec()).await.unwrap();
            environment
                .projection()
                .attach_mount(pc::MountRequirement {
                    mount_id: "retained-resource".into(),
                    source: pc::MountSource::Inline {
                        contents: "retained exact bytes".into(),
                    },
                    mount_path: "/mnt/session/uploads/retained-resource".into(),
                    access: pc::MountAccess::ReadWrite,
                    lifetime: pc::MountLifetime::Session,
                    required: true,
                })
                .await
                .unwrap();
            let hand = match environment.sandbox_environment() {
                SandboxEnvironment::Workdir {
                    hand: Some(hand), ..
                }
                | SandboxEnvironment::Namespace { hand, .. } => hand.clone(),
                _ => panic!("filesystem Hand required"),
            };
            *hand.lifecycle.binding.lock().await = HandBindingState::Ready(HandBinding {
                process: Some(Box::new(UnreapableProcess)),
                executor: Arc::new(FakeHandExecutor),
                recovery: None,
            });
            assert!(environment.lifecycle().quiesce().await.is_err());
            assert!(environment.lifecycle().dispose().await.is_err());
            assert!(
                environment
                    .harvest()
                    .list_frozen_mount_files("/mnt/session/uploads")
                    .await
                    .unwrap()
                    .iter()
                    .any(|(_, bytes)| bytes == b"retained exact bytes")
            );
            assert!(hand.has_tracked_binding().await);
            assert!(!hand.lifecycle.is_open());
            if let HandBindingState::Ready(binding) = &mut *hand.lifecycle.binding.lock().await {
                binding.process = Some(Box::new(DoneProcess::success("same-owner-now-reaped")));
            } else {
                panic!("owner must remain tracked")
            }
            environment.lifecycle().dispose().await.unwrap();
            assert_eq!(
                environment.sandbox_environment().status().await.unwrap(),
                pc::SandboxStatus::Terminated
            );
            assert!(!hand.has_tracked_binding().await);
            assert!(!hand.lifecycle.is_open());
        }
    }

    fn lifecycle_with_generation(
        generation: u64,
    ) -> (Arc<HandLifecycle>, tokio::sync::watch::Receiver<u64>) {
        let (activity, observed_activity) = tokio::sync::watch::channel(generation);
        (
            Arc::new(HandLifecycle {
                binding: Mutex::new(HandBindingState::Vacant),
                binding_changed: tokio::sync::Notify::new(),
                availability: AtomicU8::new(HandAvailability::Open as u8),
                generation: AtomicU64::new(generation),
                activity,
            }),
            observed_activity,
        )
    }

    #[test]
    fn concurrent_final_generation_claims_fence_without_wrapping() {
        let (lifecycle, observed_activity) = lifecycle_with_generation(u64::MAX - 1);
        let start = Arc::new(Barrier::new(3));
        let contenders = (0..2)
            .map(|_| {
                let lifecycle = lifecycle.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    lifecycle.touch()
                })
            })
            .collect::<Vec<_>>();

        start.wait();
        for contender in contenders {
            assert!(!contender.join().expect("generation contender joins"));
        }
        assert_eq!(lifecycle.generation.load(Ordering::Acquire), u64::MAX);
        assert_eq!(lifecycle.availability(), HandAvailability::Fenced);
        assert!(!lifecycle.is_open());
        assert_eq!(*observed_activity.borrow(), u64::MAX);
    }

    #[tokio::test]
    async fn idle_retirement_waiting_on_binding_observes_a_concurrent_touch_as_stale() {
        let (lifecycle, _observed_activity) = lifecycle_with_generation(7);
        let binding = lifecycle.binding.lock().await;
        let retirement = {
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move {
                lifecycle
                    .hibernate_if_current(Some(7), "concurrent_idle_test")
                    .await
            })
        };

        tokio::task::yield_now().await;
        assert!(lifecycle.touch());
        drop(binding);

        assert_eq!(
            retirement.await.expect("idle retirement joins"),
            Ok(RetireOutcome::Stale)
        );
        assert_eq!(lifecycle.generation.load(Ordering::Acquire), 8);
    }

    #[test]
    fn close_wins_every_projection_begin_or_commit_interleaving() {
        for _ in 0..64 {
            let (lifecycle, _observed_activity) = lifecycle_with_generation(0);
            let start = Arc::new(Barrier::new(3));
            let projection = {
                let lifecycle = lifecycle.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    if lifecycle.begin_projection_update() {
                        std::thread::yield_now();
                        lifecycle.commit_projection_update();
                        let _ = lifecycle.touch();
                    }
                })
            };
            let close = {
                let lifecycle = lifecycle.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    lifecycle.close();
                })
            };

            start.wait();
            projection.join().expect("projection contender joins");
            close.join().expect("close contender joins");
            assert_eq!(lifecycle.availability(), HandAvailability::Closed);
            assert!(!lifecycle.is_open());
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "session_hand/scoped_tests.rs"]
mod scoped_tests;

#[cfg(kani)]
mod verification {
    use super::*;

    /// @obligation remote_tool.hand_availability_recovery_and_terminal_transitions_are_closed
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

    /// @obligation remote_tool.hand_launch_must_cross_a_tracked_starting_phase_before_ready
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

    /// @obligation remote_tool.hand_generation_exhaustion_is_terminal_and_never_wraps
    #[kani::proof]
    fn hand_generation_advance_never_wraps_and_exhaustion_is_terminal() {
        let current = kani::any::<u64>();
        let step = hand_generation_step(current);

        assert!(step.next >= current);
        if step.exhausted {
            assert_eq!(step.next, u64::MAX);
            assert!(current >= u64::MAX - 1);
        } else {
            assert_eq!(step.next, current + 1);
            assert!(step.next < u64::MAX);
        }
    }
}
