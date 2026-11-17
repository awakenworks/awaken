// @formal surface requirement=protocol.ui_adapters.stream_projection
// @formal reason: Coordinates replica-local SSE observation of the existing committed Session projection.
//! Single-flight reconciliation for the SSE subscribers of one Session.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use awaken_sync::LeafLock;
use awaken_time::mono::{self, Instant};
use tokio::time::Interval;

use super::{ManagedState, ProjectionObservationDecision, StateError};

const PERIOD: Duration = Duration::from_secs(1);
// Share a completed periodic refresh with subscribers of the same tick that
// resume a little later. A newly opened stream still needs a fresh observation.
const PERIODIC_REUSE: Duration = Duration::from_millis(100);

type Observation = Result<ProjectionObservationDecision, Arc<StateError>>;
type Completed = Option<(Instant, Observation)>;
type Entries = Arc<LeafLock<HashMap<String, RegisteredRefresh>>>;

#[derive(Clone)]
pub(crate) struct SseRefreshes(Entries);

impl Default for SseRefreshes {
    fn default() -> Self {
        Self(Arc::new(LeafLock::new(HashMap::new())))
    }
}

struct RegisteredRefresh {
    refresh: Arc<Entry>,
    subscribers: usize,
}

struct Entry {
    started: Instant,
    completed_generation: AtomicU64,
    completed: tokio::sync::Mutex<Completed>,
}

/// The registry only retains Sessions with an open or opening SSE subscription.
pub(crate) struct SessionSseRefresh {
    session_id: String,
    entry: Arc<Entry>,
    entries: Entries,
}

impl SseRefreshes {
    pub(crate) fn subscribe(&self, session_id: &str) -> SessionSseRefresh {
        let mut active = self.0.lock();
        let registration =
            active
                .entry(session_id.to_owned())
                .or_insert_with(|| RegisteredRefresh {
                    refresh: Arc::new(Entry {
                        started: mono::now(),
                        completed_generation: AtomicU64::new(0),
                        completed: Default::default(),
                    }),
                    subscribers: 0,
                });
        registration.subscribers += 1;
        SessionSseRefresh {
            session_id: session_id.to_owned(),
            entry: registration.refresh.clone(),
            entries: self.0.clone(),
        }
    }
}

impl SessionSseRefresh {
    pub(crate) fn interval(&self) -> Interval {
        let now = mono::now();
        let elapsed = now.duration_since(self.entry.started);
        let remaining = PERIOD - Duration::from_nanos(u64::from(elapsed.subsec_nanos()));
        tokio::time::interval_at(now + remaining, PERIOD)
    }

    pub(crate) async fn initial(&self, state: &ManagedState) -> Observation {
        self.refresh(state, false).await
    }

    pub(crate) async fn periodic(&self, state: &ManagedState) -> Observation {
        self.refresh(state, true).await
    }

    async fn refresh(&self, state: &ManagedState, reuse_recent: bool) -> Observation {
        let requested_generation = self.entry.completed_generation.load(Ordering::Relaxed);
        let mut completed = self.entry.completed.lock().await;
        if let Some((finished, observation)) = &*completed
            && (self.entry.completed_generation.load(Ordering::Relaxed) != requested_generation
                || reuse_recent && finished.elapsed() < PERIODIC_REUSE)
        {
            return observation.clone();
        }
        let observation = state
            .refresh_committed_events_for_observation(&self.session_id, None)
            .await
            .map_err(Arc::new);
        *completed = Some((mono::now(), observation.clone()));
        self.entry
            .completed_generation
            .fetch_add(1, Ordering::Relaxed);
        observation
    }
}

impl Drop for SessionSseRefresh {
    fn drop(&mut self) {
        let mut entries = self.entries.lock();
        let registration = entries
            .get_mut(&self.session_id)
            .expect("registered SSE refresh");
        registration.subscribers -= 1;
        if registration.subscribers == 0 {
            entries.remove(&self.session_id);
        }
    }
}
