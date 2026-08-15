//! Service-host supervision for long-lived component tasks.
//!
//! Domain components register their recurring loops here, while the outermost
//! service host remains the sole owner of cancellation, readiness, and bounded join.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

/// The three product service assemblies that share this lifecycle owner.
///
/// `Worker` is intentionally absent: a Worker has its own authority-free
/// executable and cannot be smuggled through a Control/Coordinator startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StartupRole {
    AllInOne = 0,
    Control = 1,
    Coordinator = 2,
}

impl StartupRole {
    /// Decode a role supplied at a serialization/process boundary. Unknown
    /// discriminants fail closed instead of inheriting an assembly manifest.
    #[must_use]
    pub const fn from_discriminant(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::AllInOne),
            1 => Some(Self::Control),
            2 => Some(Self::Coordinator),
            _ => None,
        }
    }
}

/// Independently owned pieces of a service startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum StartupComponent {
    Control = 0,
    Resources = 1,
    Coordinator = 2,
    LocalWorker = 3,
}

impl StartupComponent {
    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

const ALL_STARTUP_COMPONENTS: u8 = StartupComponent::Control.bit()
    | StartupComponent::Resources.bit()
    | StartupComponent::Coordinator.bit()
    | StartupComponent::LocalWorker.bit();

/// The exact authority manifest for one product service role.
#[must_use]
pub const fn required_startup_components(role: StartupRole) -> u8 {
    match role {
        StartupRole::AllInOne => ALL_STARTUP_COMPONENTS,
        StartupRole::Control => StartupComponent::Control.bit(),
        StartupRole::Coordinator => {
            StartupComponent::Resources.bit() | StartupComponent::Coordinator.bit()
        }
    }
}

/// Whether a role owns one startup component. Product wiring consumes this
/// selector instead of recreating role tests at every store/service boundary.
#[must_use]
pub const fn startup_requires(role: StartupRole, component: StartupComponent) -> bool {
    required_startup_components(role) & component.bit() != 0
}

/// Validate a complete startup manifest received at a process boundary.
/// Unknown roles, missing required components, and extra authority bits all
/// fail closed.
#[must_use]
pub const fn startup_wiring_is_exact(role_discriminant: u8, components: u8) -> bool {
    let Some(role) = StartupRole::from_discriminant(role_discriminant) else {
        return false;
    };
    components & !ALL_STARTUP_COMPONENTS == 0 && components == required_startup_components(role)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFailure {
    pub task: String,
    pub cause: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ShutdownError {
    pub timed_out: Vec<String>,
}

impl std::fmt::Display for ShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "service tasks did not stop before the drain deadline: {}",
            self.timed_out.join(", ")
        )
    }
}

impl std::error::Error for ShutdownError {}

struct TaskHandle {
    name: String,
    task_abort: AbortHandle,
    watcher: JoinHandle<()>,
}

struct Inner {
    cancellation: CancellationToken,
    healthy: Arc<AtomicBool>,
    first_failure: Arc<Mutex<Option<TaskFailure>>>,
    failure_tx: mpsc::UnboundedSender<TaskFailure>,
    failure_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<TaskFailure>>,
    registry: Mutex<TaskRegistry>,
    shutdown_guard: tokio::sync::Mutex<()>,
}

struct TaskRegistry {
    accepting: bool,
    tasks: Vec<TaskHandle>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Ok(registry) = self.registry.get_mut() {
            for task in registry.tasks.drain(..) {
                task.task_abort.abort();
                task.watcher.abort();
            }
        }
    }
}

/// One service-instance registry for critical recurring tasks.
#[derive(Clone)]
pub struct ServiceLifecycle {
    inner: Arc<Inner>,
}

impl Default for ServiceLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceLifecycle {
    #[must_use]
    pub fn new() -> Self {
        let (failure_tx, failure_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                cancellation: CancellationToken::new(),
                healthy: Arc::new(AtomicBool::new(true)),
                first_failure: Arc::new(Mutex::new(None)),
                failure_tx,
                failure_rx: tokio::sync::Mutex::new(failure_rx),
                registry: Mutex::new(TaskRegistry {
                    accepting: true,
                    tasks: Vec::new(),
                }),
                shutdown_guard: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Spawn a critical task with a child of the service cancellation token.
    /// Returning before cancellation, returning an error, or panicking marks the
    /// whole group unhealthy. Registration after shutdown is rejected loudly
    /// because silently detaching it would create a second lifecycle.
    pub fn spawn<F, Fut>(&self, name: impl Into<String>, build: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        // Registration and the shutdown transition share one critical section.
        // This is the linearization point: a task is either spawned and owned by
        // the registry, or rejected before its future is constructed.
        let mut registry = self
            .inner
            .registry
            .lock()
            .expect("service lifecycle registry lock poisoned");
        if !registry.accepting {
            drop(registry);
            panic!("cannot add a service task after shutdown began");
        }
        let name = name.into();
        let cancellation = self.inner.cancellation.child_token();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(build(task_cancellation));
        let task_abort = task.abort_handle();
        let healthy = self.inner.healthy.clone();
        let first_failure = self.inner.first_failure.clone();
        let failure_tx = self.inner.failure_tx.clone();
        let task_name = name.clone();
        let watcher = tokio::spawn(async move {
            let outcome = task.await;
            if cancellation.is_cancelled() {
                return;
            }
            let cause = match outcome {
                Ok(Ok(())) => "completed unexpectedly".to_owned(),
                Ok(Err(error)) => error,
                Err(error) if error.is_panic() => format!("panicked: {error}"),
                Err(error) => format!("stopped unexpectedly: {error}"),
            };
            let failure = TaskFailure {
                task: task_name,
                cause,
            };
            healthy.store(false, Ordering::Release);
            let mut first = first_failure
                .lock()
                .expect("service lifecycle failure lock poisoned");
            if first.is_none() {
                *first = Some(failure.clone());
                let _ = failure_tx.send(failure);
            }
        });
        registry.tasks.push(TaskHandle {
            name,
            task_abort,
            watcher,
        });
    }

    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Acquire)
    }

    /// Wait for the first critical failure, or `None` when normal service
    /// cancellation wins. The first fault is retained as the diagnostic source
    /// of truth; later task fallout cannot overwrite it.
    pub async fn wait_for_failure(&self) -> Option<TaskFailure> {
        if let Some(failure) = self
            .inner
            .first_failure
            .lock()
            .expect("service lifecycle failure lock poisoned")
            .clone()
        {
            return Some(failure);
        }
        let mut failures = self.inner.failure_rx.lock().await;
        tokio::select! {
            failure = failures.recv() => failure,
            () = self.inner.cancellation.cancelled() => None,
        }
    }

    /// Broadcast cancellation, join every registered task until one shared
    /// deadline, then abort only the tasks that ignored cooperative shutdown.
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), ShutdownError> {
        // Clones may initiate shutdown concurrently. Only one caller may own
        // the transferred task set; followers wait for that complete drain
        // before observing the already-empty registry. Without this barrier a
        // follower returned Ok while the leader still had live tasks and could
        // later report a timeout.
        let _shutdown_guard = self.inner.shutdown_guard.lock().await;
        let tasks = {
            let mut registry = self
                .inner
                .registry
                .lock()
                .expect("service lifecycle registry lock poisoned");
            registry.accepting = false;
            self.inner.cancellation.cancel();
            std::mem::take(&mut registry.tasks)
        };
        if tasks.is_empty() {
            return Ok(());
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut timed_out = Vec::new();
        for mut task in tasks {
            if task.watcher.is_finished() {
                let _ = task.watcher.await;
                continue;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero()
                || tokio::time::timeout(remaining, &mut task.watcher)
                    .await
                    .is_err()
            {
                timed_out.push(task.name);
                task.task_abort.abort();
                task.watcher.abort();
                let _ = task.watcher.await;
            }
        }
        if timed_out.is_empty() {
            Ok(())
        } else {
            Err(ShutdownError { timed_out })
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn symbolic_role(value: u8) -> StartupRole {
        match value % 3 {
            0 => StartupRole::AllInOne,
            1 => StartupRole::Control,
            _ => StartupRole::Coordinator,
        }
    }

    fn symbolic_component(value: u8) -> StartupComponent {
        match value % 4 {
            0 => StartupComponent::Control,
            1 => StartupComponent::Resources,
            2 => StartupComponent::Coordinator,
            _ => StartupComponent::LocalWorker,
        }
    }

    #[kani::proof]
    fn startup_wiring_is_exact_for_every_service_role() {
        let role = symbolic_role(kani::any());
        let component = symbolic_component(kani::any());
        let expected = match role {
            StartupRole::AllInOne => true,
            StartupRole::Control => component == StartupComponent::Control,
            StartupRole::Coordinator => matches!(
                component,
                StartupComponent::Resources | StartupComponent::Coordinator
            ),
        };
        assert_eq!(startup_requires(role, component), expected);
    }

    #[kani::proof]
    fn startup_wiring_requires_every_role_owned_component() {
        let role = symbolic_role(kani::any());
        let components: u8 = kani::any();
        if startup_wiring_is_exact(role as u8, components) {
            let component = symbolic_component(kani::any());
            assert_eq!(
                components & component.bit() != 0,
                startup_requires(role, component)
            );
        }
    }

    #[kani::proof]
    fn startup_wiring_fails_closed_for_unknown_missing_or_extra_authority() {
        let role_discriminant: u8 = kani::any();
        let components: u8 = kani::any();
        let admitted = startup_wiring_is_exact(role_discriminant, components);
        match StartupRole::from_discriminant(role_discriminant) {
            Some(role) => {
                assert_eq!(admitted, components == required_startup_components(role));
            }
            None => assert!(!admitted),
        }
    }
}
