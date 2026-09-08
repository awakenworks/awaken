//! I/O observation at the existing failure-injecting Dispatch adapter.
//! This controls returning/pending ports, never scheduler decisions.
use awaken_dispatch::DispatchError;
use std::future::Future;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Gc,
    Relay,
    Scan,
}
impl Operation {
    pub fn name(self) -> &'static str {
        match self {
            Self::Gc => "gc",
            Self::Relay => "relay",
            Self::Scan => "scan",
        }
    }
}

pub struct MaintenanceProbe {
    pause: Option<Operation>,
    fail: bool,
    paused: AtomicBool,
    pub entered: Notify,
    release: Semaphore,
    events: Mutex<Vec<serde_json::Value>>,
}
impl MaintenanceProbe {
    pub fn new(pause: Option<Operation>, fail: bool) -> Self {
        Self {
            pause,
            fail,
            paused: AtomicBool::new(false),
            entered: Notify::new(),
            release: Semaphore::new(0),
            events: Mutex::new(Vec::new()),
        }
    }
    pub fn event(&self, kind: &str, operation: Option<Operation>) {
        self.events
            .lock()
            .unwrap()
            .push(serde_json::json!({"kind": kind,
            "operation": operation.map_or("none", Operation::name)}));
    }
    pub fn snapshot(&self) -> Vec<serde_json::Value> {
        self.events.lock().unwrap().clone()
    }
    pub fn release(&self) {
        self.event("release", None);
        self.release.add_permits(1);
    }
    pub async fn observe<T>(
        &self,
        operation: Operation,
        future: impl Future<Output = Result<T, DispatchError>>,
    ) -> Result<T, DispatchError> {
        self.event("begin", Some(operation));
        let paused = self.pause == Some(operation) && !self.paused.swap(true, Ordering::SeqCst);
        if paused {
            self.entered.notify_one();
            self.release.acquire().await.unwrap().forget();
        }
        let result = if paused && self.fail {
            Err(DispatchError::Unavailable(
                "injected returning maintenance error".into(),
            ))
        } else {
            future.await
        };
        self.event(if result.is_ok() { "ok" } else { "error" }, Some(operation));
        result
    }
}
