//! Process-owned supervision for long-lived service tasks.
//!
//! Domain components register their recurring loops here, while the outermost
//! process remains the sole owner of cancellation, readiness, and bounded join.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tokio_util::sync::CancellationToken;

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
            "process tasks did not stop before the drain deadline: {}",
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
    accepting: AtomicBool,
    healthy: Arc<AtomicBool>,
    first_failure: Arc<Mutex<Option<TaskFailure>>>,
    failure_tx: mpsc::UnboundedSender<TaskFailure>,
    failure_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<TaskFailure>>,
    tasks: Mutex<Vec<TaskHandle>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Ok(tasks) = self.tasks.get_mut() {
            for task in tasks.drain(..) {
                task.task_abort.abort();
                task.watcher.abort();
            }
        }
    }
}

/// One process-wide registry for critical recurring tasks.
#[derive(Clone)]
pub struct ProcessTaskGroup {
    inner: Arc<Inner>,
}

impl Default for ProcessTaskGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessTaskGroup {
    #[must_use]
    pub fn new() -> Self {
        let (failure_tx, failure_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(Inner {
                cancellation: CancellationToken::new(),
                accepting: AtomicBool::new(true),
                healthy: Arc::new(AtomicBool::new(true)),
                first_failure: Arc::new(Mutex::new(None)),
                failure_tx,
                failure_rx: tokio::sync::Mutex::new(failure_rx),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Spawn a critical task with a child of the process cancellation token.
    /// Returning before cancellation, returning an error, or panicking marks the
    /// whole group unhealthy. Registration after shutdown is rejected loudly
    /// because silently detaching it would create a second lifecycle.
    pub fn spawn<F, Fut>(&self, name: impl Into<String>, build: F)
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        assert!(
            self.inner.accepting.load(Ordering::Acquire),
            "cannot add a process task after shutdown began"
        );
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
                .expect("process lifecycle failure lock poisoned");
            if first.is_none() {
                *first = Some(failure.clone());
                let _ = failure_tx.send(failure);
            }
        });
        self.inner
            .tasks
            .lock()
            .expect("process lifecycle task lock poisoned")
            .push(TaskHandle {
                name,
                task_abort,
                watcher,
            });
    }

    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.inner.healthy.load(Ordering::Acquire)
    }

    /// Wait for the first critical failure, or `None` when normal process
    /// cancellation wins. The first fault is retained as the diagnostic source
    /// of truth; later task fallout cannot overwrite it.
    pub async fn wait_for_failure(&self) -> Option<TaskFailure> {
        if let Some(failure) = self
            .inner
            .first_failure
            .lock()
            .expect("process lifecycle failure lock poisoned")
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
        self.inner.accepting.store(false, Ordering::Release);
        self.inner.cancellation.cancel();
        let tasks = std::mem::take(
            &mut *self
                .inner
                .tasks
                .lock()
                .expect("process lifecycle task lock poisoned"),
        );
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
